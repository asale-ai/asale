import { useCallback, useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { groupModelsByFamily } from "@shared/model-groups";
import {
  invoke, inTauri, realTauri, runOAuthFlow, submitOAuthCode, fmtTokens,
  isSignedOut, gotoSignIn, requireSignIn, DaemonError, toDaemonError,
  type AccountStatus, type ClientStatus, type ImportAllResult, type Lane, type SupplyTest,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Empty, Mark, CopyChip, FoldToggle } from "../ui";
import {
  IconTrash, IconShield, IconChip, IconRefresh, IconPlus, IconPencil, IconInfo,
  IconZap, IconX, IconCheck, IconAlert,
} from "../icons";
import { errText } from "../errors";
import {
  VerifyPanel, type LaneVerdict, type VerifyOverview,
} from "../components/VerifyPanel";

/** The selling terms `setSell` can edit. Every field means "leave as is" when
 *  omitted — the page edits the switch, the cap and the band independently. */
interface SellTerms {
  dailyLimit?: number;
  minRatio?: number;
  maxRatio?: number;
  concurrency?: number;
}

/**
 * How the matcher currently sees this seller. `deprioritised` means every lane
 * below is declared and indexed but served last — the failure with no other
 * symptom.
 */
interface SellerStatus {
  score: number;
  min_score: number;
  deprioritised: boolean;
}

/** Subscriptions connected by signing in through a loopback OAuth callback. */
const PROVIDERS = [
  { id: "claude", label: "Claude Code" },
  { id: "claude_work", label: "Claude Work" },
  { id: "codex", label: "Codex / OpenAI" },
  { id: "gemini", label: "Gemini" },
];

/** Subscriptions authorised by device code. Same two-step flow, except the
 *  user confirms a short code instead of being redirected back — which is why
 *  these two also work when the UI runs in a browser on another machine. */
const DEVICE_PROVIDERS = [
  { id: "kimi", label: "Kimi Code" },
  { id: "xai", label: "Grok CLI" },
];

/** Custom endpoint — internal.
 *
 * An OpenAI-compatible endpoint the platform buys from and resells: same lanes,
 * same terms, same metering as a subscription, and it appears in the account
 * list below like any other. It differs only in where the requests go.
 *
 * One tile, with the base URL typed into it. It used to be one tile per vendor
 * with the URL baked in, which meant a code change and a release for every new
 * endpoint the platform signed up with. The daemon is the one that decides
 * whether a URL is usable anyway: it requires http(s) and probes
 * `GET {base}/models` with the key before storing anything, so a wrong host or
 * a dead key fails at connect time rather than on the first buyer.
 *
 * Only rendered when the daemon is running the feature (`ASALE_CUSTOM_ENDPOINTS`).
 */
const ENDPOINT_TILE = "custom";

/** Base URLs worth one click — the endpoints the platform itself has an account
 *  with. A shortcut for filling the field, not a restriction on what may go in
 *  it. B.AI keeps one base URL for many keys (its own docs say switching access
 *  method only replaces the key), and each key is a different tier at a
 *  different cost, so each is connected under its own name. */
const ENDPOINT_PRESETS = [
  { label: "OpenRouter", base: "https://openrouter.ai/api/v1" },
  { label: "BAI", base: "https://api.b.ai/v1" },
];

/** The protocols an endpoint may speak, in the order they are offered.
 *
 * Vendor names rather than the daemon's ids, because that is what the endpoint's
 * own docs call them: a host advertising "Anthropic-compatible" is `claude`
 * here. `""` is the first option and the default — the probe tries each in turn
 * and keeps the one that answers, which is a better answer than a guess by
 * somebody reading a reseller's marketing page. */
const ENDPOINT_WIRES = [
  { id: "", label: "" },
  { id: "openai", label: "OpenAI" },
  { id: "claude", label: "Anthropic" },
  { id: "gemini", label: "Gemini" },
  { id: "responses", label: "OpenAI Responses" },
];

/** What to call a protocol the daemon named back. */
const wireLabel = (id: string) => ENDPOINT_WIRES.find((w) => w.id === id)?.label || id;

/** The metered platform APIs, which issue keys rather than subscriptions.
 *  `keyUrl` is where the key is issued. */
const KEY_PROVIDERS = [
  { id: "kimi_api", label: "Moonshot API", keyUrl: "https://platform.moonshot.cn/console/api-keys" },
  { id: "xai_api", label: "xAI API", keyUrl: "https://console.x.ai" },
];

/** "in 4m 12s" — a countdown is what makes an auto-recovering pause read as
 *  "wait" rather than "broken". Returns "" once the instant has passed. */
const countdown = (until: number, now: number) => {
  const left = until - now;
  if (left <= 0) return "";
  const m = Math.floor(left / 60);
  const s = left % 60;
  return m > 0 ? `${m}m ${s}s` : `${s}s`;
};

/** Stable per-account key — a provider can hold several accounts. */
const keyOf = (a: { provider: string; account_id: string }) => `${a.provider}:${a.account_id}`;

/** Readable label for a credential store: `keychain:Claude Code-credentials` →
 *  `Claude Code-credentials`, a path → its last two segments. */
const shortSource = (s: string) =>
  s === "oauth" ? "Asale OAuth"
    : s.startsWith("keychain:") ? s.slice("keychain:".length)
    : s.split("/").filter(Boolean).slice(-2).join("/");

/** The legal range for a price ratio: the server clamps the market ratio to
 *  [0.05, 1.00], so 5 percent *of* list price is the deepest the market can
 *  ever price a model at and therefore the lowest floor worth accepting here.
 *
 *  A seller's decision is only ever "not below X" — there is no such thing as a
 *  price too good to accept — so the setting is that one number and nothing
 *  else. There is no "any price" any more: every account sells against a floor,
 *  and an account that has not chosen one sells against `RATIO_DEFAULT` rather
 *  than against the market's own bottom.
 *
 *  `RATIO_MIN` is the platform's floor and `RATIO_DEFAULT` is the one a seller
 *  starts on — they are deliberately different numbers, so that the default is
 *  a price somebody would plausibly have picked rather than the cheapest the
 *  market is allowed to go. */
const RATIO_MIN = 5;
const RATIO_MAX = 100;
const RATIO_DEFAULT = 10;
const clampRatio = (n: number) => Math.min(RATIO_MAX, Math.max(RATIO_MIN, n));
/** The floor in force for an account. Unset — null, or the 0 a row written
 *  before this setting existed carries — reads as the default rather than as
 *  the platform floor: nobody chose 0, and answering it with 5% would quietly
 *  halve what an upgraded seller asks. */
const floorOf = (a: { sell_min_ratio?: number | null }) =>
  a.sell_min_ratio && a.sell_min_ratio > 0 ? clampRatio(a.sell_min_ratio) : RATIO_DEFAULT;

/** Floors worth one click. */
const BAND_PRESETS = [RATIO_MIN, RATIO_DEFAULT, 50, 60, 80];

/** How many requests one subscription serves at once. The market is told this
 *  number and stops offering the lane work past it, so it is a ceiling the
 *  gateway honours rather than one this device has to enforce by refusing.
 *
 *  The range mirrors the daemon's own (`store::SELL_CONCURRENCY_RANGE`): a
 *  floor of 1, because "serve nothing" is the sell switch's job, and a ceiling
 *  that is a sanity bound on a typed number rather than a vendor limit. */
const SLOTS_MIN = 1;
const SLOTS_MAX = 64;
const SLOTS_DEFAULT = 5;
const clampSlots = (n: number) => Math.min(SLOTS_MAX, Math.max(SLOTS_MIN, n));
/** The value in force, with a 0 from a row written before the setting existed
 *  reading as the default rather than as "one at a time". */
const slotsOf = (a: { sell_concurrency?: number | null }) =>
  a.sell_concurrency && a.sell_concurrency > 0 ? clampSlots(a.sell_concurrency) : SLOTS_DEFAULT;

/** Concurrency values worth one click. */
const SLOT_PRESETS = [1, 3, 5, 10, 20];

/** Why a lane is or is not on the market, collapsed to the four cases the
 *  ranking chart draws differently.
 *
 *  These are *states*, not series, so they wear the app's status colours rather
 *  than a categorical palette — and each one carries its own words in the row
 *  beside the bar, because a colour on its own is not an answer. */
type LaneTone = "selling" | "price" | "blocked" | "off";

const laneTone = (l: Lane): LaneTone =>
  l.status === "selling" ? "selling"
    : l.status === "withheld" ? "price"
    : l.status === "off" ? "off"
    : "blocked";

/** How many model families a chart shows before it needs asking. Enough that
 *  the whole price question is answerable at a glance for every provider we
 *  sell, without a hundred-row catalog burying the account below it. */
const RANK_VISIBLE = 8;

/** One subscription's models, ranked by what the market currently pays for
 *  them, and split into what is selling and what is not.
 *
 *  The bar is the price as a fraction of the vendor's list price, so a longer
 *  bar is more money, and the account's floor is drawn on the same scale as the
 *  zone those bars have to land in. That is the whole question this chart
 *  exists to answer: which of my models has the market pushed below the price I
 *  said I would sell at.
 *
 *  The scale is fixed at 0–100% rather than fitted to the data. Bars need a zero
 *  baseline to be read as lengths at all, and the floor marker is only
 *  meaningful against an axis that does not move when the prices do.
 */
function DiscountRank({
  lanes, floor, now, onResume, resuming,
}: {
  lanes: Lane[];
  floor: number;
  now: number;
  onResume: (lane: Lane) => void;
  resuming: Record<string, boolean>;
}) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  /** Families whose older versions the operator asked to see, by family key. */
  const [openFamilies, setOpenFamilies] = useState<string[]>([]);

  // Cheapest first: the models the market has pushed furthest down are the ones
  // a floor is about, so they are the ones worth reading first. A lane with no
  // known price sorts last — it has nothing to rank on.
  const rows = useMemo(
    () => [...lanes].sort((a, b) => {
      const ra = a.ratio ?? 1e9;
      const rb = b.ratio ?? 1e9;
      return ra - rb || a.model.localeCompare(b.model);
    }),
    [lanes],
  );

  // One line per model family, same as the buy-side picker: an account that
  // sells seven `claude-opus-*` is answering one question about Opus, and
  // seven near-identical bars is the shape that hides it. Grouping runs on the
  // sorted rows, so a family lands where its newest version's price put it.
  //
  // The provider qualifies the key for the same reason it does in the picker —
  // two upstreams' same-named models are not versions of each other.
  const families = useMemo(
    () => groupModelsByFamily(rows, (l) => `${l.provider}/${l.model}`),
    [rows],
  );

  const view = useMemo(
    () => families.map((f) => {
      const open = openFamilies.includes(f.key);
      // An older version that needs the operator stays on screen whatever the
      // fold says: its resume button is the only way to clear it, and a chart
      // that folds away the one row carrying an action is worse than a long one.
      const shown = open ? f.all : [f.latest, ...f.older.filter((l) => l.requires_user)];
      return { key: f.key, latest: f.latest, older: f.older.length, open, shown };
    }),
    [families, openFamilies],
  );

  const visible = expanded ? view : view.slice(0, RANK_VISIBLE);
  const hidden = view.length - visible.length;
  // Counted over every lane, not over what the fold left on screen: these are
  // the account's totals, and a number that changed when a family was collapsed
  // would be answering a different question from the one it is labelled with.
  const sellingCount = rows.filter((l) => l.status === "selling").length;
  const priceCount = rows.filter((l) => l.status === "withheld").length;
  const priced = rows.filter((l) => l.ratio != null).length;

  const stateText = (l: Lane): string => {
    if (l.status === "selling") return t("publish.rank.onSale");
    if (l.status === "withheld") return t("publish.rank.heldOnPrice");
    if (l.status === "off") return t("publish.rank.switchedOff");
    if (l.paused_reason) return t(`publish.lanePause.${l.paused_reason}`, { defaultValue: l.paused_reason });
    return t(`publish.laneStatus.${l.status}`, { defaultValue: l.status });
  };

  return (
    <div className="disc-rank">
      <div className="dr-head">
        <span className="dr-title">{t("publish.rank.title")}</span>
        <span className="dr-count">{t("publish.rank.onSaleOfTotal", { n: sellingCount, total: rows.length })}</span>
      </div>

      {/* No price for anything is a state of its own, and a very different one
          from "the market pays nothing" — which is what a chart of empty bars
          would otherwise be saying. */}
      {priced === 0 && <div className="dr-nodata">{t("publish.rank.noPrices")}</div>}

      <div className="dr-rows">
        {visible.flatMap((fam) => fam.shown.map((l) => {
          const lk = `${l.provider}:${l.account_id}:${l.model}`;
          const tone = laneTone(l);
          const r = l.ratio;
          const back = countdown(Math.max(l.resume_at, l.cooldown_until ?? 0), now);
          const state = stateText(l);
          // The newest version leads its family and carries the fold; the rest
          // are its history, set back a step.
          const head = l === fam.latest;
          return (
            <div
              key={lk}
              className={`dr-row ${tone}${l.requires_user ? " attention" : ""}${head ? "" : " sub"}`}
              title={`${l.model} · ${state}${l.last_error ? `\n${l.last_error}` : ""}`}
            >
              <span className="dr-model mono">
                <span className="dr-name">{l.model}</span>
                {head && fam.older > 0 && (
                  <FoldToggle
                    n={fam.older}
                    open={fam.open}
                    onToggle={() =>
                      setOpenFamilies((o) =>
                        o.includes(fam.key) ? o.filter((x) => x !== fam.key) : [...o, fam.key])}
                  />
                )}
              </span>
              <div className="dr-track">
                {/* The zone the operator said they would sell in: everything at
                    or above the floor, with no upper edge to draw — there is no
                    such thing as a price too good to accept. Always drawn:
                    every account has a floor now. */}
                <span
                  className="dr-band to-top"
                  style={{ left: `${floor}%`, width: `${RATIO_MAX - floor}%` }}
                />
                {r != null && (
                  <span className="dr-fill" style={{ width: `${clampRatio(r)}%` }} />
                )}
              </div>
              <span className="dr-val mono tabular">{r == null ? "—" : `${r}%`}</span>
              <span className="dr-state">
                {state}
                {back && <span className="dr-back"> · {back}</span>}
              </span>
              {l.requires_user && (
                <button
                  className="lane-resume"
                  onClick={() => onResume(l)}
                  disabled={!inTauri || !!resuming[lk]}
                  title={t("publish.laneResumeHint")}
                >
                  {t("publish.laneResume")}
                </button>
              )}
            </div>
          );
        }))}
      </div>

      {/* A cap that hides rows has to say so — a chart that quietly stops at
          eight reads as a complete answer when it is not. */}
      {hidden > 0 && (
        <button className="lane-resume dr-more" onClick={() => setExpanded(true)}>
          {t("publish.rank.showAll", { n: hidden })}
        </button>
      )}
      {expanded && view.length > RANK_VISIBLE && (
        <button className="lane-resume dr-more" onClick={() => setExpanded(false)}>
          {t("publish.rank.showLess")}
        </button>
      )}

      <div className="dr-foot">
        <span className="dr-key">
          <i className="dr-swatch selling" /> {t("publish.rank.onSale")}
        </span>
        <span className="dr-key">
          <i className="dr-swatch price" /> {t("publish.rank.heldOnPrice")}
        </span>
        <span className="dr-key">
          <i className="dr-swatch blocked" /> {t("publish.rank.otherwiseOff")}
        </span>
        <span className="dr-scale">
          {t("publish.rank.bandFloor", { lo: floor })}
          {priceCount > 0 && ` · ${t("publish.rank.heldCount", { n: priceCount })}`}
        </span>
      </div>
    </div>
  );
}

/** Selling is per subscription account — there is no device-wide sell switch.
 *  The market session simply follows these switches (the daemon connects on the
 *  first account switched on and disconnects with the last), and the link's own
 *  state is shown by the global status widget in the top bar. */
/** The dialects a test request can be written in, in the order offered.
 *
 * This is a *buyer's* choice, not a fact about the seller: the gateway
 * translates between dialects, so an OpenAI-shaped request is legitimately
 * served by a Claude subscription. Offering it is what lets the test answer
 * "can the tool I care about buy from me" — Codex speaks Responses, Claude Code
 * speaks Anthropic, and the translation between them is itself a thing that can
 * break while every lane looks healthy.
 *
 * OpenAI-compatible is first because it is the one every buyer's tool can
 * speak, so it is the answer when the seller has no particular tool in mind.
 * The ids match `commands::probe::WIRES` on the daemon. */
const TEST_WIRES = [
  { id: "openai", label: "OpenAI" },
  { id: "claude", label: "Anthropic" },
  { id: "gemini", label: "Gemini" },
  { id: "responses", label: "OpenAI Responses" },
];

/** Buy from one of this account's own lanes, and show what came back.
 *
 * Every other control on this page reports what *this machine* believes. This
 * one is the only thing on it that can be wrong in the direction that matters:
 * it asks the market for an answer and gets one, or does not. So the result
 * says which of the two happened and how long it took, and — on the success
 * path — which subscription actually served, because a device can hold several
 * accounts of one provider and the seller-side pool, not this dialog, decides
 * which of them takes the request.
 *
 * The cost is stated before the button rather than after it. This spends real
 * balance and real subscription quota, and a test that quietly bills someone is
 * a worse tool than one that asks.
 */
function TestDialog({
  account, lanes, verdicts, onClose,
}: {
  account: AccountStatus;
  lanes: Lane[];
  verdicts: LaneVerdict[];
  onClose: () => void;
}) {
  const { t } = useTranslation();
  /* Two questions about the same lane, and sellers arrive wanting either.
   *
   * "Does it work" and "is it genuine" are not the same check and must not be
   * one button. The first is a round trip: one purchase, seconds, and a plain
   * yes or no. The second is a dozen purchases scored against how the model is
   * supposed to behave, and its answer is a standing that expires. Collapsing
   * them would make the fast check slow and the slow check look like a
   * connectivity test. */
  const [tab, setTab] = useState<"test" | "verify">("test");
  // Selling lanes first, then the rest: the default has to be a model that
  // could actually answer, or the first thing every seller sees is a failure
  // that is about their choice rather than about their supply.
  const options = useMemo(() => {
    const rank = (l: Lane) => (l.status === "selling" ? 0 : l.sell_enabled ? 1 : 2);
    return [...lanes].sort((a, b) => rank(a) - rank(b) || a.model.localeCompare(b.model));
  }, [lanes]);

  const [model, setModel] = useState(() => options[0]?.model ?? "");
  const [wire, setWire] = useState(TEST_WIRES[0].id);
  const [running, setRunning] = useState(false);
  const [result, setResult] = useState<SupplyTest | null>(null);
  const [err, setErr] = useState("");

  const chosen = options.find((l) => l.model === model);

  async function run() {
    setRunning(true);
    setResult(null);
    setErr("");
    try {
      setResult(await invoke<SupplyTest>("test_supply", {
        provider: account.provider,
        accountId: account.account_id,
        model,
        wire,
      }));
    } catch (e) {
      // Only the reasons the test could not be *run* land here; a refusal by
      // the market comes back as a result with `ok: false`.
      setErr(errText(e));
    } finally {
      setRunning(false);
    }
  }

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget && !running) onClose();
      }}
    >
      <div className="modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{t(tab === "test" ? "publish.test.title" : "publish.verify.title")}</h3>
          <button type="button" className="modal-x" onClick={onClose} disabled={running}>
            <IconX />
          </button>
        </div>
        <div style={{ padding: "0 var(--s5) var(--s5)" }}>
          <div className="band-presets" style={{ marginBottom: "var(--s4)" }}>
            <button
              className={`chip ${tab === "test" ? "on" : ""}`}
              onClick={() => setTab("test")}
              disabled={running}
            >
              {t("publish.test.tab")}
            </button>
            <button
              className={`chip ${tab === "verify" ? "on" : ""}`}
              onClick={() => setTab("verify")}
              disabled={running}
            >
              {t("publish.verify.tab")}
            </button>
          </div>

          <p className="sub">
            {t(tab === "test" ? "publish.test.body" : "publish.verify.body", {
              account: account.account_id,
            })}
          </p>

          <div className="field">
            <label>{t("publish.test.modelLabel")}</label>
            <select
              className="input"
              value={model}
              onChange={(e) => setModel(e.target.value)}
              disabled={running || options.length === 0}
            >
              {options.map((l) => (
                <option key={l.model} value={l.model}>
                  {l.model}
                  {l.status !== "selling" ? ` · ${t(`publish.laneStatus.${l.status}`, { defaultValue: l.status })}` : ""}
                </option>
              ))}
            </select>
            {/* A model that is not on the market will fail, and it will fail for
                a reason this page already shows above. Saying so first is
                cheaper than letting them buy the answer. */}
            {chosen && chosen.status !== "selling" && (
              <div className="hint">{t("publish.test.modelNotSelling")}</div>
            )}
          </div>

          {tab === "test" ? (
            <>
              <div className="field">
                <label>{t("publish.test.wireLabel")}</label>
                <select className="input" value={wire} onChange={(e) => setWire(e.target.value)} disabled={running}>
                  {TEST_WIRES.map((w) => (
                    <option key={w.id} value={w.id}>{w.label}</option>
                  ))}
                </select>
                <div className="hint">{t("publish.test.wireHint")}</div>
              </div>

              <div className="callout compact">
                <IconInfo /><span>{t("publish.test.costNote")}</span>
              </div>

              <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
                <button className="btn" onClick={run} disabled={running || !model}>
                  {running ? t("publish.test.running") : t("publish.test.run")}
                </button>
                <button className="btn subtle" onClick={onClose} disabled={running}>
                  {t("common.close")}
                </button>
              </div>

              {result && <TestResult result={result} />}
              <Err>{err}</Err>
            </>
          ) : (
            /* Keyed on the model so switching rows starts a clean panel rather
               than showing the previous model's verdict under a new name. */
            <VerifyPanel
              key={model}
              provider={account.provider}
              model={model}
              verdict={verdicts.find((v) => v.provider === account.provider && v.model === model)}
            />
          )}
        </div>
      </div>
    </div>
  );
}

/**
 * The gate in front of the sell switch.
 *
 * Turning selling on is the moment a subscription starts serving strangers'
 * prompts for money. Before that happens the platform buys a few completions
 * from the lane and checks that it behaves like the model it is about to be
 * sold as — and this dialog is where the seller watches that happen and then
 * confirms.
 *
 * # The confirm button is not the security control
 *
 * It cannot be. The daemon this dialog talks to is the software being verified,
 * and anyone willing to fake a model is willing to delete a button. What
 * actually keeps an unverified lane off the market is the gateway refusing to
 * route to it, which happens whether this dialog was ever opened.
 *
 * What the dialog is for is the seller: it is the one moment they are looking,
 * so it is where the arrangement gets explained — that this check happened,
 * that it will keep happening unannounced, and that the sampling is paid for
 * like any other sale.
 *
 * # Why it can be dismissed mid-run
 *
 * A run is tens of seconds. Trapping somebody behind a modal for that long, on
 * a check they did not ask for, is a bad trade for a dialog whose whole job is
 * to make the rule feel reasonable. The run continues on the server; reopening
 * the dialog picks the result back up.
 */
function VerifyGateDialog({
  account, lanes, verdicts, enforced, onConfirm, onCancel,
}: {
  account: AccountStatus;
  lanes: Lane[];
  verdicts: LaneVerdict[];
  /** False while the platform is measuring but not yet refusing. The dialog
   *  still runs and still explains — it just does not claim the lane cannot
   *  sell, because it can. */
  enforced: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  const models = useMemo(
    () => [...new Set(lanes.map((l) => l.model))].sort((a, b) => a.localeCompare(b)),
    [lanes],
  );
  const [model, setModel] = useState(() => models[0] ?? "");
  const [outcome, setOutcome] = useState("");

  const known = verdicts.find((v) => v.provider === account.provider && v.model === model);
  const standing = outcome || known?.status || "";
  /* `watch` sells. It means the run could not reach a confident conclusion,
   * which is not an accusation and does not cost a listing — the platform
   * simply samples that lane harder. Blocking here on anything short of a
   * clean pass would put the client's gate somewhere stricter than the
   * server's, and the seller would be stuck behind a button while their lane
   * was, in fact, allowed to trade. */
  const passed = standing === "pass" || standing === "watch";

  return (
    <div
      className="modal-backdrop"
      // Not dismissible by clicking away. The switch is already on, and a
      // stray click that left it on with nobody verifying is a lane earning
      // nothing for a reason its owner never saw. Both exits are explicit.
    >
      <div className="modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{t("publish.verify.gateTitle")}</h3>
          <button type="button" className="modal-x" onClick={onCancel}>
            <IconX />
          </button>
        </div>
        <div style={{ padding: "0 var(--s5) var(--s5)" }}>
          <p className="sub">{t("publish.verify.gateBody", { account: account.account_id })}</p>

          {models.length > 1 && (
            <div className="field">
              <label>{t("publish.verify.gateModelLabel")}</label>
              <select
                className="input"
                value={model}
                onChange={(e) => { setModel(e.target.value); setOutcome(""); }}
              >
                {models.map((m) => (
                  <option key={m} value={m}>{m}</option>
                ))}
              </select>
              {/* One model is verified here and the rest are verified as they
                  first serve. Saying so beats letting a seller believe the
                  whole account was cleared by one run. */}
              <div className="hint">{t("publish.verify.gateModelHint")}</div>
            </div>
          )}

          <VerifyPanel
            key={model}
            provider={account.provider}
            model={model}
            verdict={known}
            onOutcome={setOutcome}
          />

          <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
            <button className="btn" onClick={onConfirm} disabled={enforced && !passed}>
              {t("publish.verify.confirmSell")}
            </button>
            <button className="btn subtle" onClick={onCancel}>
              {t("publish.verify.cancelSell")}
            </button>
          </div>
          <div className="hint">
            {t(enforced && !passed ? "publish.verify.confirmBlocked" : "publish.verify.alreadyOn")}
          </div>
        </div>
      </div>
    </div>
  );
}

/** What came back, in the terms the seller asked the question in.
 *
 * A pass is not "200 OK" — it is that a request took the buyer's path and this
 * machine answered it, so the line that matters is the round trip and who
 * served. A failure is not a stack trace either: the market's own refusal
 * already carries a translated sentence, and the stage is what tells the seller
 * whether to look at their network, their balance, or their subscription.
 */
function TestResult({ result: r }: { result: SupplyTest }) {
  const { t } = useTranslation();
  const seconds = (r.elapsed_ms / 1000).toFixed(1);
  return (
    <div className={`callout ${r.ok ? "" : "warn"} card-foot`} style={{ display: "block" }}>
      <div className="value-row">
        {r.ok ? <IconCheck /> : <IconAlert />}
        <span className="value-strong">
          {r.ok ? t("publish.test.pass") : t(`publish.test.fail.${r.stage}`, { defaultValue: t("publish.test.fail.gateway") })}
        </span>
        <span className="faint mono">{t("publish.test.tookSeconds", { s: seconds })}</span>
      </div>

      {r.ok ? (
        <div className="fact-grid tight" style={{ marginTop: "var(--s3)" }}>
          <div className="fact">
            <span className="fact-k">{t("publish.test.servedBy")}</span>
            <span className="fact-v mono">{r.provenance?.upstream || "—"}</span>
          </div>
          <div className="fact">
            <span className="fact-k">{t("publish.test.tokens")}</span>
            <span className="fact-v mono">{`${r.in_tokens ?? 0} / ${r.out_tokens ?? 0}`}</span>
          </div>
          {/* Empty is a real answer from a reasoning model that spent its whole
              ceiling thinking, so the row is only drawn when there are words. */}
          {r.reply && (
            <div className="fact">
              <span className="fact-k">{t("publish.test.reply")}</span>
              <span className="fact-v mono">{r.reply}</span>
            </div>
          )}
        </div>
      ) : (
        // Only when there is something to add. A failure whose headline is the
        // whole explanation — nobody else served this, so nothing was proved —
        // would otherwise be followed by a generic "request failed", which reads
        // as a second, vaguer fault rather than as the same one.
        r.error && (
          <div className="hint" style={{ marginTop: "var(--s2)" }}>
            {errText(toDaemonError(r.error, t("common.requestFailed")))}
          </div>
        )
      )}
    </div>
  );
}

export function Publish() {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");

  const [accounts, setAccounts] = useState<AccountStatus[]>([]);
  const [acctLoading, setAcctLoading] = useState(inTauri);
  const [acctErr, setAcctErr] = useState("");
  /** Accounts with a sell action in flight, keyed by `provider:account_id`. */
  const [pending, setPending] = useState<Record<string, boolean>>({});
  /** Which account's supply test is open, by the same key; "" = none. */
  const [testing, setTesting] = useState("");
  /* The account whose verification gate is open, and the terms it will apply
     once the seller confirms. Held together because the gate interrupts a
     `setSell` call that was already on its way — confirming has to resume it
     with exactly the arguments it had, not with defaults. */
  const [gate, setGate] = useState<{ key: string; terms: SellTerms } | null>(null);
  const [verification, setVerification] = useState<VerifyOverview | null>(null);

  // Per-model lane state. `now` ticks once a second so the countdowns on
  // cooling lanes actually count down between the 4s account polls.
  const [lanes, setLanes] = useState<Lane[]>([]);
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000));
  const [resuming, setResuming] = useState<Record<string, boolean>>({});

  // Per-account daily cap drafts (edited independently of the switch). The cap
  // reads as a value next to the other account facts and only turns into an
  // input once the pencil is clicked — a permanently open field reads as
  // "unsaved" and does not line up with the read-only facts beside it.
  const [limitDraft, setLimitDraft] = useState<Record<string, string>>({});
  const [limitSaved, setLimitSaved] = useState("");
  const [limitEditing, setLimitEditing] = useState("");

  // Per-account price-floor drafts, edited on the same pencil pattern as the
  // cap above. One number: the share of list price at or above which this
  // subscription sells.
  const [bandDraft, setBandDraft] = useState<Record<string, string>>({});
  const [bandSaved, setBandSaved] = useState("");
  const [bandEditing, setBandEditing] = useState("");

  // Per-account concurrency drafts, same pencil pattern again: how many
  // requests this subscription is willing to have in flight at once.
  const [slotsDraft, setSlotsDraft] = useState<Record<string, string>>({});
  const [slotsSaved, setSlotsSaved] = useState("");
  const [slotsEditing, setSlotsEditing] = useState("");

  // The code a device-code login is waiting on, shown until it completes.
  const [deviceCode, setDeviceCode] = useState<{ provider: string; code: string; url: string } | null>(null);
  /** The in-flight loopback login, so its code can be pasted back when the
   *  callback lands on a machine that is not the daemon's. */
  const [pasteFlow, setPasteFlow] = useState<{ provider: string; flowId: string } | null>(null);
  const [pasteDraft, setPasteDraft] = useState("");

  // Platform endpoints: whether this account may use them at all, which
  // endpoint's form is open, and the terms being typed into it.
  const [endpointsOn, setEndpointsOn] = useState(false);
  const [epProvider, setEpProvider] = useState("");
  const [epBase, setEpBase] = useState("");
  const [epWire, setEpWire] = useState("");
  const [epKey, setEpKey] = useState("");
  const [epName, setEpName] = useState("");
  const [epFloor, setEpFloor] = useState("");
  const [epSlots, setEpSlots] = useState(String(SLOTS_DEFAULT));
  const [epBusy, setEpBusy] = useState(false);

  // API-key connect: which vendor's form is open, and its draft.
  const [keyProvider, setKeyProvider] = useState("");
  const [keyDraft, setKeyDraft] = useState("");
  const [keyLabel, setKeyLabel] = useState("");

  // Local-CLI import: the daemon runs it at startup, this only re-runs it.
  const [rescanning, setRescanning] = useState(false);
  const [importMsg, setImportMsg] = useState("");
  const [importWarnings, setImportWarnings] = useState<string[]>([]);
  // Tools currently buying. Their local accounts are left out of the list
  // below, and an account that vanished needs an explanation, not silence.
  // Polled with the accounts would mean a keychain + filesystem scan every few
  // seconds, so this is read once and refreshed when the user rescans.
  const [buyingTools, setBuyingTools] = useState<string[]>([]);
  const [sellerStatus, setSellerStatus] = useState<SellerStatus | null>(null);
  const [importErr, setImportErr] = useState("");

  // Two things decide this — a build flag, which cannot change while the app is
  // up, and whether the signed-in account is a platform operator, which can:
  // this page is reachable before signing in, and selling through an endpoint
  // of one's own is an operator capability the gateway enforces on every
  // declaration. So it is polled rather than asked once. The cost is a local
  // RPC; the daemon caches the server's verdict for a minute behind it.
  //
  // The failure case keeps the last answer instead of hiding the tile: a
  // momentarily unreachable daemon is not a demotion, and a tile that
  // disappears mid-edit takes the half-typed form with it.
  useEffect(() => {
    if (!inTauri) return;
    let alive = true;
    const poll = () =>
      invoke<{ enabled: boolean }>("custom_endpoints_status")
        .then((r) => { if (alive) setEndpointsOn(r.enabled); })
        .catch(() => {});
    poll();
    const id = setInterval(poll, 8000);
    return () => { alive = false; clearInterval(id); };
  }, []);

  const loadAccounts = useCallback(() => {
    if (!inTauri) return;
    invoke<AccountStatus[]>("list_accounts")
      .then((list) => {
        setAccounts(list);
        setAcctErr("");
        // Seed each account's cap input once; don't clobber an in-progress edit.
        setLimitDraft((d) => {
          const next = { ...d };
          for (const a of list) if (next[keyOf(a)] === undefined) next[keyOf(a)] = String(a.sell_daily_limit || 0);
          return next;
        });
        setBandDraft((d) => {
          const next = { ...d };
          for (const a of list) if (next[keyOf(a)] === undefined) next[keyOf(a)] = String(floorOf(a));
          return next;
        });
      })
      .catch((e) => setAcctErr(errText(e)))
      .finally(() => setAcctLoading(false));
    invoke<{ lanes: Lane[] }>("list_lanes")
      .then((r) => setLanes(r.lanes))
      .catch(() => {});
  }, []);

  const loadBuying = useCallback(() => {
    if (!inTauri) return;
    invoke<ClientStatus>("client_status")
      .then((s) => setBuyingTools(s.buying))
      .catch(() => {});
  }, []);

  useEffect(loadBuying, [loadBuying]);

  useEffect(() => {
    if (!inTauri) return;
    loadAccounts();
    const id = setInterval(loadAccounts, 4000);
    const tick = setInterval(() => setNow(Math.floor(Date.now() / 1000)), 1000);
    return () => { clearInterval(id); clearInterval(tick); };
  }, [loadAccounts]);

  /* Lane verification standing.
   *
   * Polled rather than fetched once: a shadow sample can change a lane's
   * standing at any moment without the seller having done anything, and the
   * badge on this page is the only place they would ever see that happen.
   * Slowly, though — a verdict is minted by a run that takes a minute and
   * stands for a week, so there is nothing here that rewards a fast poll.
   *
   * A failure is left silent. Verification is not what this page is for, and a
   * red banner about it would sit on top of the switches and terms somebody
   * actually came here to change. */
  useEffect(() => {
    if (!inTauri) return;
    const poll = () =>
      invoke<VerifyOverview>("lane_verification_overview")
        .then(setVerification)
        .catch(() => {});
    poll();
    const id = setInterval(poll, 60000);
    return () => clearInterval(id);
  }, []);

  // Reputation standing. The gateway reports it once per supply declaration
  // (every 60s), so polling faster than that would only re-read the same value.
  useEffect(() => {
    if (!inTauri) return;
    const poll = () =>
      invoke<SellerStatus | null>("seller_status")
        .then((s) => setSellerStatus(s ?? null))
        .catch(() => {});
    poll();
    const id = setInterval(poll, 30000);
    return () => clearInterval(id);
  }, []);

  // The account whose test dialog is open. Resolved from the live list rather
  // than captured when the button was pressed, so an account removed (or
  // switched off) under a four-second poll closes its own dialog.
  const testAccount = accounts.find((a) => keyOf(a) === testing && a.sell_enabled);
  // Resolved from the live list for the same reason `testAccount` is: an
  // account removed under the four-second poll closes its own dialog. Not
  // filtered on `sell_enabled` — the whole point of this one is that the
  // account is not selling yet.
  const gateAccount = accounts.find((a) => keyOf(a) === gate?.key);

  /** Put a lane the operator has fixed back on the market. */
  async function resume(lane: Lane) {
    const k = `${lane.provider}:${lane.account_id}:${lane.model}`;
    setResuming((r) => ({ ...r, [k]: true }));
    setAcctErr("");
    try {
      await invoke("resume_lane", {
        provider: lane.provider,
        accountId: lane.account_id,
        model: lane.model,
      });
      loadAccounts();
    } catch (e) {
      setAcctErr(errText(e));
    } finally {
      setResuming((r) => ({ ...r, [k]: false }));
    }
  }

  /** Flip one account's sell switch, or save one of its selling terms, via the
   *  same RPC. Terms left out are kept as they are by the daemon. */
  async function setSell(
    a: AccountStatus,
    enabled: boolean,
    terms: SellTerms = {},
    /** Set once the verification gate has been confirmed, so the resumed call
     *  does not re-open the dialog it just came out of. */
    verified = false,
  ) {
    const k = keyOf(a);
    const { dailyLimit, minRatio, maxRatio, concurrency } = terms;
    setAcctErr("");
    // Going on the market needs a session — check before the switch moves, so a
    // signed-out user lands on the sign-in form instead of watching the switch
    // turn itself back off. Only the off → on transition: editing the terms of
    // an account that is already selling, and switching one off, stay local.
    if (enabled && !a.sell_enabled && !(await requireSignIn("errors.session.signInToSell"))) return;
    // Same transition, one step later: verify before the lane can be bought
    // from. Deliberately not on the way *off* — being unable to stop selling
    // because a verification would not run is the worse failure by a distance.
    //
    // The switch is written *first* and the dialog opens after, which is the
    // opposite of how this reads and the only order that works. The platform
    // verifies a lane by buying from it, and a lane is only declared to the
    // market while its account's switch is on — so holding the switch until
    // verification passed meant verifying a lane that did not exist, and every
    // run failed with `no_supply`. Turning the switch on does not put the lane
    // on sale: the gateway declares it `unverified` and refuses to route
    // buyers to it until a verdict says otherwise. Cancelling the dialog turns
    // the switch back off.
    const needsGate = enabled && !a.sell_enabled && !verified && verification?.enabled;
    setPending((p) => ({ ...p, [k]: true }));
    // Optimistic: the list refreshes on a 4s poll, too slow for a toggle.
    setAccounts((list) =>
      list.map((x) => (keyOf(x) === k
        ? {
            ...x,
            sell_enabled: enabled,
            sell_daily_limit: dailyLimit ?? x.sell_daily_limit,
            sell_min_ratio: minRatio ?? x.sell_min_ratio,
            sell_max_ratio: maxRatio ?? x.sell_max_ratio,
            sell_concurrency: concurrency ?? x.sell_concurrency,
          }
        : x)),
    );
    try {
      await invoke("set_account_sell", {
        provider: a.provider,
        accountId: a.account_id,
        enabled,
        ...(dailyLimit === undefined ? {} : { dailyLimit }),
        ...(minRatio === undefined ? {} : { minRatio }),
        ...(maxRatio === undefined ? {} : { maxRatio }),
        ...(concurrency === undefined ? {} : { concurrency }),
      });
      loadAccounts();
      if (needsGate) setGate({ key: k, terms });
    } catch (e) {
      setAcctErr(errText(e));
      // The session can lapse between the check above and this call.
      if (isSignedOut(e)) gotoSignIn((e as DaemonError).key);
      loadAccounts(); // roll the optimistic update back to server truth
    } finally {
      setPending((p) => ({ ...p, [k]: false }));
    }
  }

  async function saveLimit(a: AccountStatus) {
    const raw = parseInt(limitDraft[keyOf(a)] ?? "0", 10);
    const dailyLimit = Number.isFinite(raw) && raw > 0 ? raw : 0;
    setLimitEditing("");
    await setSell(a, a.sell_enabled, { dailyLimit });
    setLimitSaved(keyOf(a));
    setTimeout(() => setLimitSaved(""), 2000);
  }

  /** Save one account's price floor. An empty or unreadable number falls back to
   *  the default rather than to the platform floor: a half-typed form must not
   *  quietly offer the subscription at the cheapest price the market allows.
   *
   *  The ceiling always goes up with it: the daemon still holds a band, and
   *  anything less than list price there would withhold models on a rule this
   *  page no longer offers a way to see or unset. */
  async function saveBand(a: AccountStatus) {
    const raw = parseInt(bandDraft[keyOf(a)] ?? "", 10);
    const minRatio = Number.isFinite(raw) ? clampRatio(raw) : RATIO_DEFAULT;
    setBandEditing("");
    setBandDraft((d) => ({ ...d, [keyOf(a)]: String(minRatio) }));
    await setSell(a, a.sell_enabled, { minRatio, maxRatio: RATIO_MAX });
    setBandSaved(keyOf(a));
    setTimeout(() => setBandSaved(""), 2000);
  }

  /** Open the floor editor on the value currently in force, for the same reason
   *  `editLimit` does. */
  function editBand(a: AccountStatus) {
    const k = keyOf(a);
    setBandDraft((d) => ({ ...d, [k]: String(floorOf(a)) }));
    setBandEditing(k);
  }

  /** Save one account's concurrency. An unreadable number falls back to the
   *  default rather than to the floor: a half-typed form must not quietly take
   *  four fifths of the subscription's capacity off the market. */
  async function saveSlots(a: AccountStatus) {
    const raw = parseInt(slotsDraft[keyOf(a)] ?? "", 10);
    const concurrency = Number.isFinite(raw) ? clampSlots(raw) : SLOTS_DEFAULT;
    setSlotsEditing("");
    setSlotsDraft((d) => ({ ...d, [keyOf(a)]: String(concurrency) }));
    await setSell(a, a.sell_enabled, { concurrency });
    setSlotsSaved(keyOf(a));
    setTimeout(() => setSlotsSaved(""), 2000);
  }

  /** Open the concurrency editor on the value currently in force, for the same
   *  reason `editLimit` does. */
  function editSlots(a: AccountStatus) {
    const k = keyOf(a);
    setSlotsDraft((d) => ({ ...d, [k]: String(slotsOf(a)) }));
    setSlotsEditing(k);
  }

  /** Open the cap editor on the value currently in force (so cancelling an edit
   *  and reopening never resumes a half-typed number). */
  function editLimit(a: AccountStatus) {
    const k = keyOf(a);
    setLimitDraft((d) => ({ ...d, [k]: String(a.sell_daily_limit || 0) }));
    setLimitEditing(k);
  }

  async function connect(provider: string) {
    setErr(""); setMsg(""); setBusy(true); setPasteDraft("");
    try {
      // Two-step: the daemon opens/returns the authorize URL, we poll for the
      // loopback callback + token exchange to finish.
      const r = await runOAuthFlow<{ account_id: string }>(
        "oauth_login",
        { provider },
        (start) => setPasteFlow({ provider, flowId: start.flow_id }),
      );
      setMsg(t("publish.connected", { provider, account: r.account_id }));
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); setPasteFlow(null); }
  }

  /** Finish a login whose callback never arrived, from what the user copied out
   *  of the browser's address bar. The daemon takes it from there — the poll in
   *  `connect` above is still running and picks up the result. */
  async function submitPastedCode() {
    if (!pasteFlow || !pasteDraft.trim()) return;
    setErr("");
    try {
      await submitOAuthCode(pasteFlow.flowId, pasteDraft.trim());
      setPasteDraft("");
    } catch (e) { setErr(errText(e)); }
  }

  /** Device-code login. Identical two-step flow, except the user confirms a
   *  short code rather than being redirected back — so the code has to stay on
   *  screen for the whole wait, not just be opened and forgotten. */
  async function connectDevice(provider: string) {
    setErr(""); setMsg(""); setBusy(true); setDeviceCode(null);
    try {
      const r = await runOAuthFlow<{ account_id: string }>(
        "oauth_device_login",
        { provider },
        (start) => setDeviceCode({ provider, code: start.user_code ?? "", url: start.auth_url }),
      );
      setMsg(t("publish.connected", { provider, account: r.account_id }));
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); setDeviceCode(null); }
  }

  /** Only one connect form is open at a time. The tiles are one row of choices,
   *  so two forms open below it read as two things to fill in — and a key typed
   *  into the one that scrolled out of view would still be sitting in state for
   *  whichever submit button is on screen. Opening either clears the other. */
  function openKeyForm(id: string) {
    setEpProvider(""); setEpBase(""); setEpKey(""); setEpName(""); setEpFloor("");
    setKeyProvider(id); setKeyDraft(""); setKeyLabel("");
  }

  function openEndpointForm(open: boolean) {
    setKeyProvider(""); setKeyDraft(""); setKeyLabel("");
    setEpProvider(open ? ENDPOINT_TILE : "");
    setEpBase(""); setEpWire(""); setEpKey(""); setEpName(""); setEpFloor("");
    setEpSlots(String(SLOTS_DEFAULT));
  }

  /** Save a pasted API key as an account. The daemon checks it against the
   *  vendor first, so a dead key is refused here rather than failing every task
   *  it later gets matched to. */
  async function connectKey() {
    setErr(""); setMsg(""); setBusy(true);
    try {
      const r = await invoke<{ account_id: string }>("connect_api_key", {
        provider: keyProvider,
        apiKey: keyDraft,
        ...(keyLabel.trim() ? { label: keyLabel.trim() } : {}),
      });
      setMsg(t("publish.connected", { provider: keyProvider, account: r.account_id }));
      setKeyProvider(""); setKeyDraft(""); setKeyLabel("");
      loadAccounts();
    } catch (e) { setErr(errText(e)); } finally { setBusy(false); }
  }

  // Re-run the local-CLI import (same routine the daemon runs on startup) —
  // used when the user has just logged into a CLI and wants it picked up now.
  async function rescanCli() {
    setRescanning(true); setImportErr(""); setImportMsg(""); setImportWarnings([]);
    try {
      const r = await invoke<ImportAllResult>("import_cli_all");
      setImportMsg(
        r.imported.length > 0
          ? t("publish.cliImportedN", { n: r.imported.length })
          : t("publish.cliNone"),
      );
      setImportWarnings(r.warnings);
      loadBuying();
      if (r.errors.length > 0) setImportErr(r.errors.map((e) => `${e.provider}: ${e.error}`).join("; "));
      loadAccounts();
    } catch (e) { setImportErr(errText(e)); } finally { setRescanning(false); }
  }

  /** Connect a custom endpoint. The daemon validates the URL and probes
   *  `GET {base}/models` with the key before storing anything, so a wrong host
   *  or a dead key fails here rather than on the first buyer. An empty name is
   *  left to the daemon, which falls back to the endpoint's host — that keeps
   *  the row stable across re-runs instead of naming it after this form. */
  async function connectEndpoint() {
    setErr(""); setMsg(""); setEpBusy(true);
    try {
      const r = await invoke<{
        account_id: string;
        wire: string;
        endpoint_models: number;
        sellable_models: string[];
      }>(
        "connect_custom_endpoint",
        {
          baseUrl: epBase.trim(),
          apiKey: epKey.trim(),
          // Empty means "find out" — the daemon probes each protocol in turn.
          wire: epWire,
          label: epName.trim(),
          // Empty floor means the default; the daemon clamps it either way.
          minRatio: epFloor.trim() ? clampRatio(parseInt(epFloor, 10) || RATIO_DEFAULT) : RATIO_DEFAULT,
          concurrency: clampSlots(parseInt(epSlots, 10) || SLOTS_DEFAULT),
        },
      );
      // The protocol is named back whether it was chosen or found — when it was
      // found, it is the news, and when it was chosen it is the confirmation
      // that the endpoint really answered on it.
      setMsg(t("publish.endpointConnected", {
        account: r.account_id,
        wire: wireLabel(r.wire),
        served: r.endpoint_models,
        selling: r.sellable_models.length,
      }));
      // The key is never read back; clear the form rather than leave a secret
      // sitting in a field the next submit would resend.
      setEpProvider(""); setEpBase(""); setEpWire(""); setEpKey(""); setEpName(""); setEpFloor("");
      loadAccounts();
    } catch (e) {
      setErr(errText(e));
    } finally {
      setEpBusy(false);
    }
  }

  async function removeAccount(a: AccountStatus) {
    setAcctErr("");
    try {
      // A platform endpoint carries two settings no other account has (its base
      // URL and its cached model list); its own command takes those with it, so
      // a re-added endpoint never inherits a stale list.
      await (a.provider === "custom"
        ? invoke<boolean>("remove_custom_endpoint", { accountId: a.account_id })
        : invoke<boolean>("remove_account", { provider: a.provider, accountId: a.account_id }));
      loadAccounts();
    } catch (e) { setAcctErr(errText(e)); }
  }

  const statusPill = (s: AccountStatus["status"]) => {
    const cls = s === "available" ? "on" : s === "cooldown" || s === "exhausted" ? "warn" : "off";
    return <span className={`pill ${cls}`}>{t(`publish.status${s.charAt(0).toUpperCase()}${s.slice(1)}`)}</span>;
  };

  const open = KEY_PROVIDERS.find((p) => p.id === keyProvider);
  const openEp = endpointsOn && epProvider === ENDPOINT_TILE;
  // http(s) is the daemon's own rule (`connect_custom_endpoint`); checking it
  // here too means the submit button says "not yet" instead of the endpoint
  // check failing on something the form could see for itself.
  const epBaseOk = /^https?:\/\/\S+$/i.test(epBase.trim());

  const connectGrid = (
    <>
      <div className="pick-grid">
        {PROVIDERS.map((p) => (
          <button key={p.id} className="pick" onClick={() => connect(p.id)} disabled={busy || !inTauri}>
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectVia")}</span>
            </span>
          </button>
        ))}
        {DEVICE_PROVIDERS.map((p) => (
          <button
            key={p.id}
            className={`pick ${deviceCode?.provider === p.id ? "active" : ""}`}
            onClick={() => connectDevice(p.id)}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectViaCode")}</span>
            </span>
          </button>
        ))}
        {KEY_PROVIDERS.map((p) => (
          <button
            key={p.id}
            className={`pick ${keyProvider === p.id ? "active" : ""}`}
            onClick={() => openKeyForm(keyProvider === p.id ? "" : p.id)}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectViaKey")}</span>
            </span>
          </button>
        ))}
        {/* Internal: only when the daemon runs the feature. */}
        {endpointsOn && (
          <button
            className={`pick ${openEp ? "active" : ""}`}
            onClick={() => openEndpointForm(!openEp)}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={ENDPOINT_TILE} /></span>
            <span>
              <span className="pick-title">{t("publish.endpointTile")}</span>
              <span className="pick-sub">{t("publish.connectViaEndpoint")}</span>
            </span>
          </button>
        )}
      </div>

      {/* Offered in the desktop shell too. Its callback lands on the same
          machine as the daemon so it usually arrives on its own — but "usually"
          is not "always" (a blocked loopback port, a browser that swallowed the
          redirect), and a login with no way to finish it is a dead end. Only the
          reason it can fail differs, so only the hint does. */}
      {pasteFlow && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>{t(realTauri ? "publish.pasteHintDesktop" : "publish.pasteHint")}</span>
          </div>
          {/* The callback URL is long, so it gets the full row and the submit
              button sits beside it rather than on a line of its own. */}
          <div className="field">
            <label htmlFor="oauthpaste">{t("publish.pasteLabel")}</label>
            <div className="paste-row">
              <input
                id="oauthpaste"
                className="input mono"
                autoComplete="off"
                spellCheck={false}
                value={pasteDraft}
                placeholder={t("publish.pastePlaceholder")}
                onChange={(e) => setPasteDraft(e.target.value)}
                onKeyDown={(e) => { if (e.key === "Enter" && pasteDraft.trim()) submitPastedCode(); }}
              />
              <button className="btn" onClick={submitPastedCode} disabled={!pasteDraft.trim()}>
                {t("publish.pasteSubmit")}
              </button>
            </div>
          </div>
        </div>
      )}

      {deviceCode && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>
              {t("publish.deviceHint")}{" "}
              <a href={deviceCode.url} target="_blank" rel="noreferrer">{deviceCode.url}</a>
            </span>
          </div>
          {deviceCode.code && (
            <div className="devicecode">
              <span className="devicecode-label">{t("publish.deviceCode")}</span>
              <CopyChip value={deviceCode.code} />
            </div>
          )}
          <p className="muted">{t("publish.deviceWaiting")}</p>
        </div>
      )}

      {open && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>
              {t("publish.keyHint")}{" "}
              <a href={open.keyUrl} target="_blank" rel="noreferrer">{open.keyUrl}</a>
            </span>
          </div>
          <div className="field">
            <label htmlFor="apikey">{t("publish.keyLabel", { provider: open.label })}</label>
            <input
              id="apikey"
              className="input"
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={keyDraft}
              placeholder={t("publish.keyPlaceholder")}
              onChange={(e) => setKeyDraft(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter" && keyDraft.trim()) connectKey(); }}
            />
          </div>
          <div className="field">
            <label htmlFor="apikeylabel">{t("publish.keyName")}</label>
            <input
              id="apikeylabel"
              className="input"
              value={keyLabel}
              placeholder={t("publish.keyNamePlaceholder")}
              onChange={(e) => setKeyLabel(e.target.value)}
            />
          </div>
          <div className="keyform-actions">
            <button className="btn sm" onClick={connectKey} disabled={busy || !keyDraft.trim()}>
              {t("publish.keyConnect")}
            </button>
            <button className="btn sm ghost" onClick={() => openKeyForm("")} disabled={busy}>
              {t("publish.keyCancel")}
            </button>
          </div>
        </div>
      )}

      {/* A custom endpoint takes its terms up front, unlike a subscription:
          it costs real money per token, so the floor that keeps it from selling
          under cost is part of connecting it, not something to remember to set
          afterwards. Both terms are editable later on the account row, like any
          other account's. */}
      {openEp && (
        <div className="keyform fade-in">
          <div className="callout info">
            <IconInfo />
            <span>{t("publish.endpointHint")}</span>
          </div>
          <div className="field">
            <label htmlFor="ep-base">{t("publish.endpointBase")}</label>
            <input
              id="ep-base"
              className="input mono"
              autoComplete="off"
              spellCheck={false}
              value={epBase}
              placeholder="https://openrouter.ai/api/v1"
              onChange={(e) => setEpBase(e.target.value)}
            />
            {/* The two the platform has an account with, kept as one click each
                now that the tiles they used to have are gone. */}
            <div className="band-presets">
              <span className="band-cap">{t("publish.bandQuick")}</span>
              {ENDPOINT_PRESETS.map((p) => (
                <button
                  key={p.base}
                  className={`chip ${epBase.trim() === p.base ? "on" : ""}`}
                  onClick={() => setEpBase(p.base)}
                >
                  {p.label}
                </button>
              ))}
            </div>
          </div>
          {/* Left on "find out" unless its operator knows better: a reseller's
              docs say "OpenAI-compatible" for hosts that answer `/messages`,
              and the probe settles it in one round trip. Named explicitly, a
              protocol the endpoint does not actually serve fails at connect
              rather than on the first sale. */}
          <div className="field">
            <label>{t("publish.endpointWire")}</label>
            <div className="band-presets">
              {ENDPOINT_WIRES.map((w) => (
                <button
                  key={w.id}
                  className={`chip ${epWire === w.id ? "on" : ""}`}
                  onClick={() => setEpWire(w.id)}
                >
                  {w.label || t("publish.endpointWireAuto")}
                </button>
              ))}
            </div>
          </div>
          <div className="field">
            <label htmlFor="ep-key">{t("publish.endpointKey")}</label>
            <input
              id="ep-key"
              className="input mono"
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={epKey}
              placeholder={t("publish.keyPlaceholder")}
              onChange={(e) => setEpKey(e.target.value)}
              onKeyDown={(e) => { if (e.key === "Enter" && epBaseOk && epKey.trim()) connectEndpoint(); }}
            />
          </div>
          <div className="ep-terms">
            <div className="field">
              <label htmlFor="ep-name">{t("publish.keyName")}</label>
              <input
                id="ep-name"
                className="input"
                value={epName}
                placeholder={t("publish.endpointNamePlaceholder")}
                onChange={(e) => setEpName(e.target.value)}
              />
            </div>
            <div className="field">
              <label htmlFor="ep-floor">{t("publish.endpointFloor")}</label>
              <div className="input-row">
                <span className="band-cap">≥</span>
                <input
                  id="ep-floor"
                  className="input mono band-input"
                  type="number"
                  min={RATIO_MIN}
                  max={RATIO_MAX}
                  value={epFloor}
                  placeholder={String(RATIO_DEFAULT)}
                  onChange={(e) => setEpFloor(e.target.value)}
                />
                <span className="unit">%</span>
              </div>
            </div>
            <div className="field">
              <label htmlFor="ep-slots">{t("publish.lanesLabel")}</label>
              <div className="input-row">
                <input
                  id="ep-slots"
                  className="input mono band-input"
                  type="number"
                  min={SLOTS_MIN}
                  max={SLOTS_MAX}
                  value={epSlots}
                  onChange={(e) => setEpSlots(e.target.value)}
                />
                <span className="unit">{t("publish.unitRequests")}</span>
              </div>
            </div>
          </div>
          <div className="hint">{t("publish.endpointTermsHint")}</div>
          <div className="keyform-actions">
            <button
              className="btn sm"
              onClick={connectEndpoint}
              disabled={epBusy || !epBaseOk || !epKey.trim()}
            >
              {epBusy ? t("publish.endpointChecking") : t("publish.keyConnect")}
            </button>
            <button className="btn sm ghost" onClick={() => openEndpointForm(false)} disabled={epBusy}>
              {t("publish.keyCancel")}
            </button>
          </div>
        </div>
      )}
    </>
  );

  return (
    <div>
      <PageHead
        title={t("publish.title")}
        sub={t("publish.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("publish.cliRescanHint")}
            onClick={rescanCli}
            disabled={!inTauri || rescanning}
            spinning={rescanning}
          />
        }
      />

      <Err>{err}</Err>

      {/* Above everything on this page: while it is showing, little below it
          will earn, however healthy it looks. */}
      {sellerStatus?.deprioritised && (
        <div className="callout warn" role="alert">
          <IconInfo />
          <div>
            <strong>{t("publish.rankedLastTitle")}</strong>
            <div className="text-sm" style={{ marginTop: 4 }}>
              {t("publish.rankedLastBody", { score: sellerStatus.score, min: sellerStatus.min_score })}
            </div>
            <div className="text-sm faint" style={{ marginTop: 4 }}>
              {t("publish.rankedLastHow")}
            </div>
          </div>
        </div>
      )}

      {/* Connecting comes first: with no account yet the rest of the page has
          nothing to show, and the empty state points "above" for OAuth. */}
      <Card
        icon={<IconPlus />}
        title={t("publish.connectTitle")}
        desc={t("publish.connectDesc")}
      >
        {connectGrid}
        <Ok>{msg}</Ok>
      </Card>

      <Card
        icon={<IconShield />}
        title={t("publish.accountsTitle")}
        desc={t("publish.accountsDesc")}
        right={<span className="count-chip">{acctLoading ? "—" : accounts.length}</span>}
      >
        {acctLoading ? (
          <SkeletonRows rows={2} />
        ) : accounts.length === 0 ? (
          <Empty
            icon={<IconChip />}
            title={t("publish.accountsEmptyTitle")}
            desc={t("publish.accountsEmpty")}
            action={
              <button className="btn sm ghost" onClick={rescanCli} disabled={!inTauri || rescanning}>
                <IconRefresh className={rescanning ? "spin" : undefined} />
                {rescanning ? t("publish.scanning") : t("publish.cliRescan")}
              </button>
            }
          />
        ) : (
          <div className="acct-list">
            {accounts.map((a) => {
              const k = keyOf(a);
              const limit = a.sell_daily_limit;
              // What actually stops this account is the plan's 5h rolling
              // window, so that is the bar: today's tokens against a
              // daily-equivalent that no upstream enforces only ever read as a
              // limit that isn't there. A metered key has no window at all —
              // its bar is the operator's own daily cap, or nothing.
              const windowed = !a.key_based && a.window_cap > 0;
              const used = windowed ? a.used_window : a.used_today;
              const denom = windowed ? a.window_cap : limit;
              const pct = denom > 0 ? (used / denom) * 100 : 0;
              const capPct = a.daily_cap > 0 && limit > 0 && !a.key_based
                ? Math.round((limit / a.daily_cap) * 100)
                : 0;
              // Every lane of this account, switched on or not: the ranking
              // chart below doubles as this subscription's price board, and
              // "what would I be selling, and at what discount" is a question
              // worth being able to answer *before* flipping the switch.
              const own = lanes.filter((l) => l.provider === a.provider && l.account_id === a.account_id);
              const floor = floorOf(a);
              // How many requests this subscription serves at once, as declared
              // to the market. Named `slots` here because `lanes` in this scope
              // is already the account's (account, model) rows.
              const slots = slotsOf(a);
              // The daily cap is spent: `rebuild_pool` has already clamped this
              // account's quota to zero, so every one of its models is off the
              // market until the UTC rollover. That is a whole-subscription
              // stop, and it earns a line of its own rather than being left to
              // be inferred from an `exhausted` pill.
              const capSpent = limit > 0 && a.used_today >= limit;
              return (
                <div key={k} className={`acct ${a.sell_enabled ? "selling" : ""}`}>
                  <div className="acct-head">
                    {/* Every platform endpoint is booked under `custom`, so the
                        provider would give all of them the same glyph; their
                        own name is what tells OpenRouter from BAI. */}
                    <Mark id={a.provider === "custom" ? a.account_id : a.provider} />
                    <div className="acct-id">
                      <div className="acct-name">
                        {a.account_id}
                        {a.plan && <span className="muted"> · {a.plan}</span>}
                      </div>
                      <div className="acct-meta">
                        <span className="mono">{a.provider}</span>
                        {statusPill(a.status)}
                        {a.shared_with_local_cli
                          ? <span className="pill warn" title={t("publish.sharedHint")}>{t("publish.sharedBadge")}</span>
                          : <span className="pill on" title={t("publish.ownedHint")}>{t("publish.ownedBadge")}</span>}
                      </div>
                    </div>
                    <div className="acct-actions">
                      <label className="switch" title={t("publish.sellSwitch")}>
                        <input
                          type="checkbox"
                          checked={a.sell_enabled}
                          onChange={(e) => setSell(a, e.target.checked)}
                          disabled={!inTauri || !!pending[k]}
                        />
                        <span className="track" />
                      </label>
                      {/* Only offered on an account that is actually selling: a
                          test of a switched-off account has one possible answer
                          and the switch beside it already gives it. */}
                      <button
                        className="icon-btn sm"
                        onClick={() => setTesting(k)}
                        disabled={!inTauri || !a.sell_enabled || own.length === 0}
                        title={t("publish.test.open")}
                        aria-label={t("publish.test.open")}
                      >
                        <IconZap />
                      </button>
                      <button className="icon-btn ghost-danger" onClick={() => removeAccount(a)} title={t("publish.remove")}>
                        <IconTrash />
                      </button>
                    </div>
                  </div>

                  {a.status === "expired" && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.expiredHint")}</span>
                    </div>
                  )}

                  {capSpent && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.limitReached", { limit: fmtTokens(limit) })}</span>
                    </div>
                  )}

                  {/* Throughput first — it is the number this whole page exists
                      to move. Against the 5h window for a subscription, against
                      the operator's daily cap for a key. */}
                  <div className="acct-usage">
                    <div className="au-head">
                      <span>{windowed ? t("publish.windowUsed") : t("publish.limitUsedToday")}</span>
                      <span className="mono tabular">
                        {fmtTokens(used)}
                        {denom > 0 && <span className="faint"> / {fmtTokens(denom)}</span>}
                        {denom > 0 && <span className="au-pct"> · {Math.round(Math.min(100, pct))}%</span>}
                      </span>
                    </div>
                    <div className="bar">
                      <span
                        className={pct >= 90 ? "danger" : pct >= 70 ? "warn" : ""}
                        style={{ width: `${Math.min(100, pct)}%`, minWidth: pct > 0 ? 3 : 0 }}
                      />
                    </div>
                  </div>

                  <div className="acct-grid">
                    {/* Daily cap: a value with a pencil, an input once clicked */}
                    <div className="field">
                      <label>{t("publish.limitLabel")}</label>
                      {limitEditing === k ? (
                        <div className="input-row">
                          <input
                            className="input mono"
                            type="number"
                            min={0}
                            autoFocus
                            value={limitDraft[k] ?? ""}
                            onChange={(e) => setLimitDraft((d) => ({ ...d, [k]: e.target.value }))}
                            onKeyDown={(e) => {
                              if (e.key === "Enter") saveLimit(a);
                              if (e.key === "Escape") setLimitEditing("");
                            }}
                            placeholder="0"
                          />
                          <span className="unit">{t("publish.unitTokensDay")}</span>
                          <button className="btn sm" onClick={() => saveLimit(a)} disabled={!inTauri || !!pending[k]}>
                            {t("publish.limitSave")}
                          </button>
                          <button className="btn sm ghost" onClick={() => setLimitEditing("")}>
                            {t("publish.limitCancel")}
                          </button>
                        </div>
                      ) : (
                        <div className="value-row">
                          {/* The unit rides on the number itself. "500K" next
                              to a percentage band is a quantity with no
                              dimension, and tokens-vs-dollars is exactly the
                              guess a seller must not have to make. */}
                          <span className="value-strong mono tabular">
                            {limit > 0 ? fmtTokens(limit) : t("publish.limitNoCap")}
                          </span>
                          {limit > 0 && <span className="unit">{t("publish.unitTokensDay")}</span>}
                          <button
                            className="icon-btn sm"
                            onClick={() => editLimit(a)}
                            disabled={!inTauri || !!pending[k]}
                            title={t("publish.limitEdit")}
                            aria-label={t("publish.limitEdit")}
                          >
                            <IconPencil />
                          </button>
                          {limitSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                        </div>
                      )}
                      {/* The unit now rides on the value itself, so the hint
                          carries the thing the number alone cannot say: how
                          much of the plan a cap that size actually is. */}
                      <div className="hint">
                        {limit > 0
                          ? (capPct > 0
                              ? <>{capPct}% {t("publish.ofSubscription")}</>
                              : t("publish.limitTokensPerDay"))
                          : a.key_based ? t("publish.limitUnlimitedKey") : t("publish.limitUnlimited")}
                      </div>

                      {/* The price floor, on the same pattern: the fraction of
                          list price this subscription is willing to trade at.
                          A model the market has pushed below it leaves the
                          market until it comes back — see the chart below. */}
                      <label className="acct-sub-label after">{t("publish.bandLabel")}</label>
                      {bandEditing === k ? (
                        <div className="band-edit">
                          <div className="input-row">
                            <span className="band-cap">≥</span>
                            <input
                              className="input mono band-input"
                              type="number"
                              min={RATIO_MIN}
                              max={RATIO_MAX}
                              autoFocus
                              aria-label={t("publish.bandMin")}
                              value={bandDraft[k] ?? ""}
                              onChange={(e) => setBandDraft((d) => ({ ...d, [k]: e.target.value }))}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") saveBand(a);
                                if (e.key === "Escape") setBandEditing("");
                              }}
                              placeholder={String(RATIO_DEFAULT)}
                            />
                            <span className="unit">%</span>
                          </div>
                          <div className="band-presets">
                            <span className="band-cap">{t("publish.bandQuick")}</span>
                            {BAND_PRESETS.map((preset) => (
                              <button
                                key={preset}
                                className={`chip${Number(bandDraft[k]) === preset ? " on" : ""}`}
                                onClick={() => setBandDraft((d) => ({ ...d, [k]: String(preset) }))}
                              >
                                {`≥ ${preset}%`}
                              </button>
                            ))}
                          </div>
                          <div className="input-row">
                            <button className="btn sm" onClick={() => saveBand(a)} disabled={!inTauri || !!pending[k]}>
                              {t("publish.limitSave")}
                            </button>
                            <button className="btn sm ghost" onClick={() => setBandEditing("")}>
                              {t("publish.limitCancel")}
                            </button>
                          </div>
                        </div>
                      ) : (
                        <div className="value-row">
                          <span className="value-strong mono tabular">
                            {`≥ ${floor}%`}
                          </span>
                          <span className="unit">{t("publish.unitOfList")}</span>
                          <button
                            className="icon-btn sm"
                            onClick={() => editBand(a)}
                            disabled={!inTauri || !!pending[k]}
                            title={t("publish.bandEdit")}
                            aria-label={t("publish.bandEdit")}
                          >
                            <IconPencil />
                          </button>
                          {bandSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                        </div>
                      )}
                      {/* Say what the setting *does*, in the same words the
                          chart below uses, rather than restating its units. */}
                      <div className="hint">
                        {t("publish.bandHintFloor", { lo: floor })}
                      </div>

                      {/* How much of this subscription is on offer at any one
                          moment. Declared to the market, so the gateway stops
                          handing this account work past the number rather than
                          this device having to refuse it — a refusal costs the
                          lane's reputation, a ceiling does not. */}
                      <label className="acct-sub-label after">{t("publish.lanesLabel")}</label>
                      {slotsEditing === k ? (
                        <div className="band-edit">
                          <div className="input-row">
                            <input
                              className="input mono band-input"
                              type="number"
                              min={SLOTS_MIN}
                              max={SLOTS_MAX}
                              autoFocus
                              aria-label={t("publish.lanesLabel")}
                              value={slotsDraft[k] ?? ""}
                              onChange={(e) => setSlotsDraft((d) => ({ ...d, [k]: e.target.value }))}
                              onKeyDown={(e) => {
                                if (e.key === "Enter") saveSlots(a);
                                if (e.key === "Escape") setSlotsEditing("");
                              }}
                              placeholder={String(SLOTS_DEFAULT)}
                            />
                            <span className="unit">{t("publish.unitRequests")}</span>
                          </div>
                          <div className="band-presets">
                            <span className="band-cap">{t("publish.bandQuick")}</span>
                            {SLOT_PRESETS.map((preset) => (
                              <button
                                key={preset}
                                className={`chip${Number(slotsDraft[k]) === preset ? " on" : ""}`}
                                onClick={() => setSlotsDraft((d) => ({ ...d, [k]: String(preset) }))}
                              >
                                {preset}
                              </button>
                            ))}
                          </div>
                          <div className="input-row">
                            <button className="btn sm" onClick={() => saveSlots(a)} disabled={!inTauri || !!pending[k]}>
                              {t("publish.limitSave")}
                            </button>
                            <button className="btn sm ghost" onClick={() => setSlotsEditing("")}>
                              {t("publish.limitCancel")}
                            </button>
                          </div>
                        </div>
                      ) : (
                        <div className="value-row">
                          <span className="value-strong mono tabular">{slots}</span>
                          <span className="unit">{t("publish.unitRequests")}</span>
                          <button
                            className="icon-btn sm"
                            onClick={() => editSlots(a)}
                            disabled={!inTauri || !!pending[k]}
                            title={t("publish.lanesEdit")}
                            aria-label={t("publish.lanesEdit")}
                          >
                            <IconPencil />
                          </button>
                          {slotsSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                        </div>
                      )}
                      <div className="hint">{t("publish.lanesHint", { n: slots })}</div>
                    </div>

                    {/* No expiry here: the only timestamp we hold is the access
                        token's, which the daemon refreshes on its own — showing
                        it reads as "this subscription dies tonight". A
                        credential that really is dead says so in the status
                        pill and the note above. */}
                    <div className="fact-grid tight">
                      <div className="fact">
                        <span className="fact-k">{t("publish.usedTodayFact")}</span>
                        <span className="fact-v mono">{fmtTokens(a.used_today)}</span>
                      </div>
                      {windowed && (
                        <div className="fact">
                          <span className="fact-k">{t("publish.quotaLeft")}</span>
                          <span className="fact-v mono">{fmtTokens(a.quota_remaining)}</span>
                        </div>
                      )}
                    </div>
                  </div>

                  {/* Per-model price and state, then where the credential came
                      from. Both sit below a hairline: the chart answers "which
                      of my models is the market paying for" when that is the
                      question, and stays quiet the rest of the time. */}
                  {(own.length > 0 || a.sources.length > 0) && (
                    <div className="acct-detail">
                      {own.length > 0 && (
                        <DiscountRank
                          lanes={own}
                          floor={floor}
                          now={now}
                          onResume={resume}
                          resuming={resuming}
                        />
                      )}
                      {a.sources.length > 0 && (
                        <div className="ad-row">
                          <span className="meta-k">{t("publish.tokenSources")}</span>
                          <div className="ad-chips">
                            {a.sources.map((s, i) => (
                              <span key={s} className={`pill mono plain ${i === 0 ? "accent" : ""}`} title={s}>
                                <span>{shortSource(s)}</span>
                              </span>
                            ))}
                            {a.sources.length > 1 && (
                              <span className="faint ad-note" title={t("publish.mergedHint")}>
                                {t("publish.mergedBadge", { n: a.sources.length })}
                              </span>
                            )}
                          </div>
                        </div>
                      )}
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
        <Ok>{importMsg}</Ok>
        {importWarnings.length > 0 && (
          <div className="callout warn card-foot">
            <IconShield /><span>{t("publish.envWarning", { vars: importWarnings.join(", ") })}</span>
          </div>
        )}
        {buyingTools.length > 0 && (
          <div className="callout card-foot">
            <IconInfo />
            <span>{t("publish.cliSkippedBuying", { tools: buyingTools.join(", ") })}</span>
          </div>
        )}
        <Err>{importErr}</Err>
        <Err>{acctErr}</Err>
      </Card>

      {/* Keyed on the account so switching rows resets the dialog's own state
          rather than showing the previous account's result under a new name. */}
      {testAccount && (
        <TestDialog
          key={testing}
          account={testAccount}
          lanes={lanes.filter((l) => l.provider === testAccount.provider && l.account_id === testAccount.account_id)}
          verdicts={verification?.lanes ?? []}
          onClose={() => setTesting("")}
        />
      )}

      {gateAccount && (
        <VerifyGateDialog
          key={gate?.key}
          account={gateAccount}
          lanes={lanes.filter((l) => l.provider === gateAccount.provider && l.account_id === gateAccount.account_id)}
          verdicts={verification?.lanes ?? []}
          enforced={verification?.enforced ?? false}
          onConfirm={() => {
            setGate(null);
            loadAccounts();
          }}
          // Backing out is a real decision, not a dismissal: the switch is
          // already on, and leaving it on would mean an account sitting in
          // `unverified` earning nothing while its owner believes they
          // declined. Turning it off is local and always allowed.
          onCancel={() => {
            const a = gateAccount;
            setGate(null);
            void setSell(a, false);
          }}
        />
      )}
    </div>
  );
}

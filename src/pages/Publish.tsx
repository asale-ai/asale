import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { PROVIDERS as ALL_PROVIDERS } from "@shared/providers.generated";
import { groupModelsByFamily } from "@shared/model-groups";
import {
  invoke, inTauri, realTauri, runOAuthFlow, submitOAuthCode, fmtTokens,
  isSignedOut, gotoSignIn, requireSignIn, DaemonError, toDaemonError,
  type AccountStatus, type ClientStatus, type ImportAllResult, type Lane, type MarketModel,
  type SupplyTest,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Empty, Mark, CopyChip, FoldToggle } from "../ui";
import {
  IconTrash, IconShield, IconChip, IconRefresh, IconPlus, IconPencil, IconInfo,
  IconZap, IconX, IconCheck, IconAlert, IconChevronDown,
} from "../icons";
import { ModelMultiSelect, type ModelOption } from "../components/ModelPicker";
import { errText } from "../errors";
import { formatAge } from "./Limits";
import {
  BatchVerifyPanel, ModelChecklist, recordedStatus, type LaneVerdict, type VerifyOverview,
} from "../components/VerifyPanel";

/** The selling terms `setSell` can edit. Every field means "leave as is" when
 *  omitted — the page edits the switch, the cap and the band independently. */
interface SellTerms {
  dailyLimit?: number;
  minRatio?: number;
  maxRatio?: number;
  concurrency?: number;
  /** Which of the account's models are for sale; `[]` puts all of them back. */
  models?: string[];
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

// The three tile groups below are the generated provider table split by how an
// account is connected — which is exactly what separates them on screen. A new
// provider appears in the right group by virtue of its `credential`, with no
// edit here; a provider missing from all three would be one nobody can connect,
// which is the failure these used to produce silently.

/** Subscriptions connected by signing in through a loopback OAuth callback. */
const PROVIDERS = ALL_PROVIDERS.filter((p) => p.credential === "oauth");

/** Subscriptions authorised by device code. Same two-step flow, except the
 *  user confirms a short code instead of being redirected back — which is why
 *  these two also work when the UI runs in a browser on another machine. */
const DEVICE_PROVIDERS = ALL_PROVIDERS.filter((p) => p.credential === "device_flow");

/** One described field.
 *
 *  Six types, all of them an input this page already had. A type it has never
 *  heard of renders nothing rather than a broken control — which is also why
 *  the server has a test that every field it describes is one of these. */
function OfferFieldInput({
  field, value, onChange, onSubmit,
}: {
  field: OfferField;
  value: string;
  onChange: (v: string) => void;
  onSubmit: () => void;
}) {
  const id = `offer-${field.key}`;
  const enter = (e: React.KeyboardEvent) => { if (e.key === "Enter") onSubmit(); };
  if (field.type === "choice") {
    return (
      <div className="field">
        <label>{field.label}</label>
        <div className="band-presets">
          {(field.options ?? []).map((o) => (
            <button
              key={o.value}
              className={`chip ${value === o.value ? "on" : ""}`}
              onClick={() => onChange(o.value)}
            >
              {o.label}
            </button>
          ))}
        </div>
      </div>
    );
  }
  if (field.type === "percent" || field.type === "int") {
    return (
      <div className="field">
        <label htmlFor={id}>{field.label}</label>
        <div className="input-row">
          {field.type === "percent" && <span className="band-cap">≥</span>}
          <input
            id={id}
            className="input mono band-input"
            type="number"
            min={field.min}
            max={field.max}
            value={value}
            placeholder={field.placeholder}
            onChange={(e) => onChange(e.target.value)}
            onKeyDown={enter}
          />
          {field.type === "percent" && <span className="unit">%</span>}
        </div>
      </div>
    );
  }
  return (
    <div className="field">
      <label htmlFor={id}>{field.label}</label>
      <input
        id={id}
        className={`input ${field.type === "text" ? "" : "mono"}`}
        type={field.type === "secret" ? "password" : "text"}
        autoComplete="off"
        spellCheck={false}
        value={value}
        placeholder={field.placeholder}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={enter}
      />
      {/* Values worth one click. A shortcut for filling the field, never a
          restriction on what may go in it. */}
      {field.presets && field.presets.length > 0 && (
        <div className="band-presets">
          {field.presets.map((p) => (
            <button
              key={p.value}
              className={`chip ${value.trim() === p.value ? "on" : ""}`}
              onClick={() => onChange(p.value)}
            >
              {p.label}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}

/** A connect form the server described, for a family that needs more than a
 *  pasted key.
 *
 *  The page does not know what any of it means. It draws six field types and
 *  posts `{key: value}` to the named command — which is the point: what an
 *  endpoint is called, which hosts are worth one click and which protocols are
 *  offered are facts about the platform, and they used to be constants in this
 *  file. See `api::capabilities` on the server for why they moved.
 *
 *  Nothing here is executable. A description crosses the wire; the behaviour is
 *  this component. */
type OfferFieldType = "url" | "secret" | "text" | "choice" | "percent" | "int";
type OfferField = {
  key: string;
  type: OfferFieldType;
  label: string;
  required?: boolean;
  placeholder?: string;
  /** One click each, for a field whose useful values are few and known. */
  presets?: { label: string; value: string }[];
  options?: { value: string; label: string }[];
  min?: number;
  max?: number;
};
type OfferSection = {
  id: string;
  provider: string;
  /** The daemon RPC this form posts to. */
  command: string;
  title: string;
  hint?: string;
  fields: OfferField[];
};
/** What the connect screen may draw, as the daemon last answered. */
type ConnectOffer = { providers: string[]; sections: OfferSection[] };

/** What is drawn before the daemon has answered — offline, signed out, or an
 *  older deployment. The set that is right for everyone. */
const DEFAULT_OFFER: ConnectOffer = {
  providers: ALL_PROVIDERS.filter((p) => p.offeredByDefault).map((p) => p.id),
  sections: [],
};

/** The metered platform APIs, which issue keys rather than subscriptions.
 *  `keyUrl` is where the key is issued.
 *
 *  Read off the generated provider table rather than listed here: a provider
 *  that can be connected with a pasted key is one this form has to offer, and
 *  the two going out of step is a family nobody can connect. *Which* of them
 *  are drawn is `offer.providers`, not this list. */
const KEY_PROVIDERS = ALL_PROVIDERS.filter((p) => p.credential === "api_key");

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
  lanes, floor, now, onResume, onReauth, resuming, verdicts, gated,
}: {
  lanes: Lane[];
  floor: number;
  now: number;
  onResume: (lane: Lane) => void;
  /** Take the operator back through this account's sign-in. Absent for the
   *  families that have no re-connect flow (a custom endpoint), whose lanes
   *  fall back to the plain resume button. */
  onReauth?: () => void;
  resuming: Record<string, boolean>;
  /** This account's verification standing, one entry per lane that has one. */
  verdicts: LaneVerdict[];
  /** Whether a missing verdict actually keeps buyers away. False in a
   *  measure-only rollout, where saying "not selling" would be a lie. */
  gated: boolean;
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

  /** The verdict standing in the way of this lane selling, or "" when none is.
   *
   *  The lane's own `status` is what *this machine* is willing to do, and until
   *  now that was the whole of what the chart drew. The gate is the other half
   *  and it lives on the server: an unverified lane is kept out of every
   *  buyer's reach while the client goes on declaring it, so the page said
   *  "14 of 14 on sale" about an account the market would route exactly one
   *  request to. `pass` and `watch` both sell — an inconclusive run is not a
   *  finding against the seller — so only those two clear it. */
  const blockedBy = (l: Lane): string => {
    if (!gated || l.status !== "selling") return "";
    const st = recordedStatus(verdicts, l.provider, l.model);
    return st === "pass" || st === "watch" ? "" : st || "pending";
  };

  // Counted over every lane, not over what the fold left on screen: these are
  // the account's totals, and a number that changed when a family was collapsed
  // would be answering a different question from the one it is labelled with.
  const sellingCount = rows.filter((l) => l.status === "selling" && !blockedBy(l)).length;
  const priceCount = rows.filter((l) => l.status === "withheld").length;
  const priced = rows.filter((l) => l.ratio != null).length;

  const stateText = (l: Lane): string => {
    const blocked = blockedBy(l);
    if (blocked) return t(`publish.verify.outcome.${blocked}`);
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
          // Green is "this is earning". A lane the gate is holding is not, so
          // it reads like the other things that stop a sale rather than like a
          // sale.
          const tone: LaneTone = blockedBy(l) ? "blocked" : laneTone(l);
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
              {/* "Resume" means "I have fixed it". A revoked or expired
                  credential is the one pause the operator cannot fix anywhere
                  else, so pressing it there just puts the lane back for one
                  more 401 — the button has to be the sign-in itself. */}
              {l.requires_user && (l.paused_reason === "auth" && onReauth ? (
                <button
                  className="lane-resume"
                  onClick={onReauth}
                  disabled={!inTauri}
                  title={t("publish.laneReauthHint")}
                >
                  {t("publish.laneReauth")}
                </button>
              ) : (
                <button
                  className="lane-resume"
                  onClick={() => onResume(l)}
                  disabled={!inTauri || !!resuming[lk]}
                  title={t("publish.laneResumeHint")}
                >
                  {t("publish.laneResume")}
                </button>
              ))}
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
  account, lanes, verdicts, gated, onClose,
}: {
  account: AccountStatus;
  lanes: Lane[];
  verdicts: LaneVerdict[];
  /** Whether an unverified lane is actually being held out of buyers' reach.
   *  Follows `enforced`, not `enabled`: while the platform is only measuring,
   *  a lane with no verdict serves normally and greying it here would be this
   *  dialog inventing a refusal that is not happening. */
  gated: boolean;
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

  /* A lane the verification gate is holding back cannot be tested, and the
   * reason is worth spelling out because it looks like the opposite.
   *
   * The test is a real purchase through the ordinary front door, and it is
   * deliberately *not* exempt from the gate — a test that passed where a buyer
   * would fail is worse than no test. So until a verdict lands, every request
   * this dialog sends is refused, and it used to be refused as `no_supply`:
   * "nobody is selling this", about the seller's own switched-on account. The
   * server now names the real reason (`errors.market.laneUnverified`), and this
   * is the other half — not letting them spend the round trip to find out. */
  const blocked = useCallback((model: string) => {
    if (!gated) return false;
    const st = recordedStatus(verdicts, account.provider, model);
    return st !== "pass" && st !== "watch";
  }, [gated, verdicts, account.provider]);

  // Testable lanes first, then the rest, then the ones the gate is holding:
  // the default has to be a model that could actually answer, or the first
  // thing every seller sees is a failure that is about their choice rather
  // than about their supply.
  const options = useMemo(() => {
    const rank = (l: Lane) =>
      blocked(l.model) ? 3 : l.status === "selling" ? 0 : l.sell_enabled ? 1 : 2;
    return [...lanes].sort((a, b) => rank(a) - rank(b) || a.model.localeCompare(b.model));
  }, [lanes, blocked]);

  const models = useMemo(() => options.map((l) => l.model), [options]);
  /** The models this button may actually spend money on. */
  const testable = useMemo(() => models.filter((m) => !blocked(m)), [models, blocked]);
  const [picked, setPicked] = useState<string[]>(testable);
  const [wire, setWire] = useState(TEST_WIRES[0].id);
  const [running, setRunning] = useState(false);
  const [results, setResults] = useState<Record<string, SupplyTest | "running">>({});
  const [err, setErr] = useState("");

  // An account whose lanes load a moment after the dialog does should still
  // start out fully ticked. Keyed on the testable set rather than on every
  // model, so a verdict landing while the dialog is open brings the lane it
  // just cleared into the selection.
  useEffect(() => { setPicked(testable); }, [testable.join(" ")]); // eslint-disable-line react-hooks/exhaustive-deps

  const notSelling = picked.filter((m) => options.find((l) => l.model === m)?.status !== "selling").length;
  const blockedCount = models.filter(blocked).length;

  /** Buy from each picked lane in turn.
   *
   *  Sequential rather than concurrent, unlike a verification batch: this is a
   *  connectivity check whose whole answer is a round-trip time, and running
   *  several at once against one subscription measures the queue as much as the
   *  lane. A handful of seconds each is a wait people will sit through. */
  async function run() {
    setRunning(true);
    setErr("");
    setResults({});
    try {
      // Re-filtered rather than trusted: `picked` is state, and a verdict
      // arriving between the click and this loop could have moved a lane into
      // the gate's hands since it was ticked.
      for (const model of picked.filter((m) => !blocked(m))) {
        setResults((r) => ({ ...r, [model]: "running" }));
        try {
          const out = await invoke<SupplyTest>("test_supply", {
            provider: account.provider,
            accountId: account.account_id,
            model,
            wire,
          });
          setResults((r) => ({ ...r, [model]: out }));
        } catch (e) {
          // Only the reasons the test could not be *run* land here; a refusal by
          // the market comes back as a result with `ok: false`. One lane failing
          // this way is not a reason to abandon the rest of the batch.
          setResults((r) => {
            const next = { ...r };
            delete next[model];
            return next;
          });
          setErr(errText(e));
        }
      }
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
      <div className="modal verify-modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{t(tab === "test" ? "publish.test.title" : "publish.verify.title")}</h3>
          <button type="button" className="modal-x" onClick={onClose} disabled={running}>
            <IconX />
          </button>
        </div>
        <div className="verify-body">
          <div className="band-presets" style={{ marginBottom: "var(--s16)" }}>
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

          {/* One line per tab. Both checks cost real money and both are
              explained in a sentence; stacking the explanation, the cost note
              and a per-model caveat on top of each other only buried all three. */}
          <p className="sub">
            {t(tab === "test" ? "publish.test.note" : "publish.verify.batchNote", {
              account: account.account_id,
            })}
          </p>

          {tab === "test" ? (
            <>
              <ModelChecklist
                models={models}
                value={picked}
                onChange={setPicked}
                disabled={running}
                label={t("publish.test.pickModels")}
                unavailable={blocked}
                note={(m) => {
                  // The gate outranks every other annotation: a lane it is
                  // holding cannot be tested at all, so what its own switch or
                  // its last result says about it is beside the point.
                  if (blocked(m)) return <span className="warn">{t("publish.test.needsVerify")}</span>;
                  const r = results[m];
                  if (r === "running") return <span className="faint">{t("publish.test.running")}</span>;
                  if (r) {
                    return r.ok
                      ? <span className="good">{t("publish.test.tookSeconds", { s: (r.elapsed_ms / 1000).toFixed(1) })}</span>
                      : <span className="bad">{t(`publish.test.fail.${r.stage}`, { defaultValue: t("publish.test.fail.gateway") })}</span>;
                  }
                  const l = options.find((x) => x.model === m);
                  return l && l.status !== "selling"
                    ? <span className="faint">{t(`publish.laneStatus.${l.status}`, { defaultValue: l.status })}</span>
                    : null;
                }}
              />

              <div className="field">
                <label>{t("publish.test.wireLabel")}</label>
                <select className="input" value={wire} onChange={(e) => setWire(e.target.value)} disabled={running}>
                  {TEST_WIRES.map((w) => (
                    <option key={w.id} value={w.id}>{w.label}</option>
                  ))}
                </select>
              </div>

              {/* A model that is not on the market will fail, and it will fail
                  for a reason this page already shows above. Saying so first is
                  cheaper than letting them buy the answer. */}
              {notSelling > 0 && (
                <div className="hint">{t("publish.test.someNotSelling", { n: notSelling })}</div>
              )}

              {/* Not a warning about the test — a pointer to the one action
                  that clears it. The verification lives one tab away in this
                  same dialog, so the button switches to it rather than sending
                  the seller back to the page to find it. */}
              {blockedCount > 0 && (
                <div className="callout compact">
                  <IconShield />
                  <span>{t("publish.test.someUnverified", { n: blockedCount })}</span>
                  <button
                    type="button"
                    className="lane-resume"
                    onClick={() => setTab("verify")}
                    disabled={running}
                  >
                    {t("publish.test.goVerify")}
                  </button>
                </div>
              )}

              <div className="btn-row" style={{ marginTop: "var(--s16)" }}>
                <button className="btn" onClick={run} disabled={running || picked.length === 0}>
                  {running
                    ? t("publish.test.runningN", {
                        done: Object.values(results).filter((r) => r !== "running").length,
                        total: picked.length,
                      })
                    : t("publish.test.runN", { n: picked.length })}
                </button>
                <button className="btn subtle" onClick={onClose} disabled={running}>
                  {t("common.close")}
                </button>
              </div>

              {/* The detail — who served, how many tokens, what came back — is
                  only worth the space for one lane at a time, so the checklist
                  carries the verdicts and this carries the last full answer. */}
              {(() => {
                const last = [...picked].reverse().find((m) => results[m] && results[m] !== "running");
                const r = last ? results[last] : undefined;
                return r && r !== "running" ? <TestResult result={r} /> : null;
              })()}
              <Err>{err}</Err>
            </>
          ) : (
            <BatchVerifyPanel
              provider={account.provider}
              models={models}
              verdicts={verdicts}
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
 * # There is nothing here to confirm
 *
 * There used to be a confirm button, and it was never the security control —
 * it could not be. The daemon this dialog talks to is the software being
 * verified, and anyone willing to fake a model is willing to delete a button.
 * What actually keeps an unverified lane off the market is the gateway refusing
 * to route to it, which happens whether this dialog was ever opened. All the
 * button did was hold the seller behind a decision they had already made by
 * flipping the switch.
 *
 * What the dialog is for is the seller: it is the one moment they are looking,
 * so it is where the arrangement gets explained — that this check happened,
 * that it will keep happening unannounced, and that the sampling is paid for
 * like any other sale. That makes it the same panel the sell page's own
 * "verify" tab shows, under a different sentence, and it is deliberately built
 * from the same component so the two cannot drift apart.
 *
 * # Why it can be dismissed mid-run
 *
 * A run is tens of seconds. Trapping somebody behind a modal for that long, on
 * a check they did not ask for, is a bad trade for a dialog whose whole job is
 * to make the rule feel reasonable. The run continues on the server; reopening
 * the dialog picks the result back up. Closing leaves the switch where the
 * seller put it — on, with the gateway holding the lane until a verdict lands.
 */
function VerifyGateDialog({
  account, lanes, verdicts, onClose,
}: {
  account: AccountStatus;
  lanes: Lane[];
  verdicts: LaneVerdict[];
  onClose: () => void;
}) {
  const { t } = useTranslation();
  const models = useMemo(
    () => [...new Set(lanes.map((l) => l.model))].sort((a, b) => a.localeCompare(b)),
    [lanes],
  );

  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}
    >
      <div className="modal verify-modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{t("publish.verify.gateTitle")}</h3>
          <button type="button" className="modal-x" onClick={onClose}>
            <IconX />
          </button>
        </div>
        <div className="verify-body">
          {/* One paragraph, and only one. It has to carry three facts — the
              subscription starts serving strangers, the platform buys from it
              first to check, and it will go on doing so unannounced at its own
              expense — and it used to carry them in a paragraph plus three
              stacked callouts plus two hints, which is a wall rather than an
              explanation. */}
          <p className="sub">{t("publish.verify.gateNote", { account: account.account_id })}</p>

          <BatchVerifyPanel
            provider={account.provider}
            models={models}
            verdicts={verdicts}
          />
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
        <div className="fact-grid tight" style={{ marginTop: "var(--s12)" }}>
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
          <div className="hint" style={{ marginTop: "var(--s8)" }}>
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
  /** Which account's pre-sale verification dialog is open, by the same key;
   *  "" = none. It only reports — the switch it follows is already written. */
  const [gate, setGate] = useState("");
  const [verification, setVerification] = useState<VerifyOverview | null>(null);

  /** Which account tiles the operator has folded open or shut, by the same key.
   *
   *  Only the ones they touched: the rest fall back to the count rule below, so
   *  a tile does not change state under them when an account is added or
   *  removed. */
  const [openAcct, setOpenAcct] = useState<Record<string, boolean>>({});
  /** Whether a tile nobody has touched starts open.
   *
   *  Two subscriptions fit on a screen with their settings showing, and seeing
   *  them is the point of the page. Past that the page is a wall of identical
   *  editors and the list stops being scannable — so the default flips, and the
   *  folded head keeps the parts worth comparing across accounts. */
  const openByDefault = accounts.length <= 2;

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

  // Which of an account's models it sells. No draft: the picker's own dialog
  // has the confirm step, so what comes back is already the decision.
  const [modelsSaved, setModelsSaved] = useState("");
  /** Vendor and display name per market id, so the picker can group an
   *  account's models by the vendor that made them rather than by the
   *  credential family they happen to be sold through — one Model Studio key
   *  serves Alibaba's, DeepSeek's and Moonshot's models alike. Best-effort: an
   *  id the board does not carry still appears, under its bare id. */
  const [marketModels, setMarketModels] = useState<Record<string, { vendor: string; label: string }>>({});

  // The code a device-code login is waiting on, shown until it completes.
  const [deviceCode, setDeviceCode] = useState<{ provider: string; code: string; url: string } | null>(null);
  /** The in-flight loopback login, so its code can be pasted back when the
   *  callback lands on a machine that is not the daemon's. */
  const [pasteFlow, setPasteFlow] = useState<{ provider: string; flowId: string } | null>(null);
  const [pasteDraft, setPasteDraft] = useState("");

  /** What this account may connect, as the daemon last answered. Not a
   *  compiled-in list: which credential families a login is entitled to is the
   *  server's answer and the server's rule (`declare_supply` drops the lanes),
   *  so the page asks rather than deciding. */
  const [offer, setOffer] = useState<ConnectOffer>(DEFAULT_OFFER);
  /** Which described form is open, and what is being typed into it. The draft
   *  is keyed by field, because this page does not know what the fields are. */
  const [openSection, setOpenSection] = useState("");
  const [sectionDraft, setSectionDraft] = useState<Record<string, string>>({});
  const [sectionBusy, setSectionBusy] = useState(false);

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

  // Polled rather than asked once: this page is reachable before signing in,
  // and what a login is entitled to changes when it signs in, signs out, or is
  // granted a family. The cost is a local RPC; the daemon caches the server's
  // answer for a minute behind it.
  //
  // The failure case keeps the last answer instead of falling back: a
  // momentarily unreachable daemon is not a revocation, and a tile that
  // disappears mid-edit takes the half-typed form with it.
  // Read once, not polled: the catalog's vendor per model changes when the
  // platform lands a new model, not while this page is open, and it is only
  // used to label and group the sell-model picker.
  useEffect(() => {
    if (!inTauri) return;
    invoke<{ models: MarketModel[] }>("market_models")
      .then((r) => {
        const map: Record<string, { vendor: string; label: string }> = {};
        for (const m of r.models) map[m.model] = { vendor: m.provider, label: m.display_name || m.model };
        setMarketModels(map);
      })
      .catch(() => {});
  }, []);

  useEffect(() => {
    if (!inTauri) return;
    let alive = true;
    const poll = () =>
      invoke<ConnectOffer>("connect_offer")
        .then((r) => {
          if (!alive) return;
          // A daemon too old to answer this sends back something else; a list
          // of provider ids is the one shape worth acting on.
          if (Array.isArray(r?.providers)) {
            setOffer({ providers: r.providers, sections: r.sections ?? [] });
          }
        })
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
  const loadVerification = useCallback(async (): Promise<VerifyOverview | null> => {
    if (!inTauri) return null;
    try {
      const v = await invoke<VerifyOverview>("lane_verification_overview");
      setVerification(v);
      return v;
    } catch {
      return null;
    }
  }, []);

  useEffect(() => {
    if (!inTauri) return;
    void loadVerification();
    const id = setInterval(() => { void loadVerification(); }, 60000);
    return () => clearInterval(id);
  }, [loadVerification]);

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
  const gateAccount = accounts.find((a) => keyOf(a) === gate);

  /** Accounts already shown the verification dialog in this visit to the page.
   *
   *  Without it the auto-open below and the seller's dismissal fight each
   *  other: the condition that opened the dialog is still true the moment it
   *  closes, so it would reopen on the next render forever. A ref rather than
   *  state because nothing renders from it, and per visit rather than for good
   *  because an unverified lane is a lane earning nothing — worth raising again
   *  next time they come to look at their selling, just not every four
   *  seconds. */
  const prompted = useRef<Set<string>>(new Set());

  /** Whether the *automatic* prompt has already fired this visit.
   *
   *  Separate from `prompted` above, which is per account. A device holding
   *  four unverified accounts would otherwise answer every dismissal with the
   *  next account's dialog, which is not a prompt — it is a queue the seller
   *  has to click their way out of. One is enough to make the point; the rest
   *  are reachable from the lane list behind it. */
  const autoPrompted = useRef(false);

  /** The connect card, so a re-sign-in started from an account further down the
   *  page can scroll the form it just opened into view. */
  const connectCard = useRef<HTMLDivElement>(null);

  /** Show the verification dialog for one account.
   *
   *  `force` means the seller just did something — moved the switch — and is
   *  owed the dialog whether or not they have already dismissed one for this
   *  account. Without it this is the automatic prompt, which stands aside for a
   *  dismissal.
   *
   *  Either way it stands the *automatic* prompt down for the rest of the
   *  visit. One dialog is the point, and it does not matter which path opened
   *  it: answering a dismissal with the next account's copy is the queue this
   *  is written to avoid. */
  const openGate = useCallback((k: string, force = false) => {
    if (!force && prompted.current.has(k)) return;
    prompted.current.add(k);
    autoPrompted.current = true;
    setGate(k);
  }, []);

  /* An account selling models that have never passed verification is earning
   * nothing from them, and nothing on the market will change that on its own —
   * the gateway holds an unverified lane out of every buyer's reach until a
   * verdict lands, and the only run that arrives promptly is one the seller
   * starts. So the dialog is put in front of them.
   *
   * Deliberately *not* a verification started automatically. A run buys real
   * completions from the seller's own subscription, and starting that without
   * being asked is not this page's call to make even where the platform eats
   * the bill — see `Kind::Admission` settling at zero. The seller presses the
   * button; this only makes sure they are shown it.
   *
   * The switch-on path opens the same dialog through the same door, so a seller
   * who dismissed it there is not shown it again by this. */
  useEffect(() => {
    if (!verification?.enabled || autoPrompted.current) return;
    // Something else already has the screen; try again on the next poll.
    if (gate || testing || deviceCode || pasteFlow) return;
    // Including a half-finished edit. These polls tick every four seconds, and
    // a modal landing on top of a number somebody is typing loses the number.
    if (limitEditing || bandEditing || slotsEditing) return;
    // Lanes arrive a beat after accounts do. Reading "no lanes" as "no lane
    // needs verifying" would burn the one prompt this account gets.
    if (lanes.length === 0) return;
    const waiting = accounts.find((a) => {
      if (!a.sell_enabled || prompted.current.has(keyOf(a))) return false;
      const mine = lanes.filter((l) => l.provider === a.provider && l.account_id === a.account_id);
      return mine.length > 0 && mine.some((l) => {
        const st = recordedStatus(verification.lanes, l.provider, l.model);
        return st !== "pass" && st !== "watch";
      });
    });
    if (waiting) openGate(keyOf(waiting));
  }, [
    accounts, lanes, verification, gate, testing, deviceCode, pasteFlow,
    limitEditing, bandEditing, slotsEditing, openGate,
  ]);

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
  async function setSell(a: AccountStatus, enabled: boolean, terms: SellTerms = {}) {
    const k = keyOf(a);
    const { dailyLimit, minRatio, maxRatio, concurrency, models } = terms;
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
    // buyers to it until a verdict says otherwise, so the switch can safely
    // stay on whether or not the seller sits through the dialog.
    const turningOn = enabled && !a.sell_enabled;
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
            sell_models: models ?? x.sell_models,
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
        ...(models === undefined ? {} : { models }),
      });
      loadAccounts();
      // Decided on a *fresh* reading, not the polled one. The overview is
      // fetched on mount and once a minute after that, and the first fetch is
      // exactly the one a new seller misses: they arrive signed out, the call
      // 401s, `verification` stays null, and the switch they flip a moment
      // later finds `null?.enabled` — so the dialog never opened, and the lane
      // sat unverified and unbuyable with nothing on screen to say why.
      if (turningOn) {
        const v = (await loadVerification()) ?? verification;
        if (v?.enabled) openGate(k, true);
      }
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

  /** Save which of an account's models are for sale.
   *
   *  An empty list is not "sell nothing" — that is what the account switch is
   *  for — but "sell everything this subscription can serve", which is the
   *  default and the only choice that keeps working as the catalog grows. So
   *  clearing the picks restores the default rather than emptying the account. */
  async function saveModels(a: AccountStatus, models: string[]) {
    await setSell(a, a.sell_enabled, { models });
    setModelsSaved(keyOf(a));
    setTimeout(() => setModelsSaved(""), 2000);
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

  /** Whether this family can be signed in to again from here. Everything drawn
   *  as an ordinary tile can; a family that arrived as a described form cannot,
   *  because re-keying it would mean re-opening the whole form and that is
   *  worse than the plain resume it already has. */
  const canReauth = (a: AccountStatus) =>
    !offer.sections.some((x) => x.provider === a.provider)
    && !!ALL_PROVIDERS.find((p) => p.id === a.provider);

  /** The credential is dead — take the operator back through the same sign-in
   *  that created this account, rather than through remove-and-add.
   *
   *  Nothing else is needed: the daemon upserts on `account_id`, and
   *  `credential_replaced` clears this account's `auth` pauses (on disk and in
   *  the pool) and re-declares every lane they were holding. So there is no
   *  "now press resume" step afterwards, and there must not be one — the
   *  in-memory `auth_failed` flag is what the resume button reads, and it does
   *  not survive a restart. */
  function reconnect(a: AccountStatus) {
    const cred = ALL_PROVIDERS.find((p) => p.id === a.provider)?.credential;
    if (cred === "oauth") return void connect(a.provider);
    if (cred === "device_flow") return void connectDevice(a.provider);
    // Key families: the form lives in the connect card above this one, so the
    // button has to bring it into view as well as open it. A family that
    // arrived as a described form goes back through *that* form rather than the
    // plain key box — its fields (base URL, model list, whatever the manifest
    // asked for) are the connection, and a bare key would rebuild it wrong.
    const described = offer.sections.find((x) => x.provider === a.provider);
    if (described) openSectionForm(described);
    else openKeyForm(a.provider);
    connectCard.current?.scrollIntoView({ behavior: "smooth", block: "start" });
  }

  /** Only one connect form is open at a time. The tiles are one row of choices,
   *  so two forms open below it read as two things to fill in — and a key typed
   *  into the one that scrolled out of view would still be sitting in state for
   *  whichever submit button is on screen. Opening either clears the other. */
  function openKeyForm(id: string) {
    setOpenSection(""); setSectionDraft({});
    setKeyProvider(id); setKeyDraft(""); setKeyLabel("");
  }

  /** Open one of the described forms, seeded with whatever defaults its fields
   *  carry — a `choice` starts on its first option, everything else empty. */
  function openSectionForm(section: OfferSection | null) {
    setKeyProvider(""); setKeyDraft(""); setKeyLabel("");
    setOpenSection(section?.id ?? "");
    setSectionDraft(
      Object.fromEntries(
        (section?.fields ?? []).map((f) => [f.key, f.type === "choice" ? (f.options?.[0]?.value ?? "") : ""])
      )
    );
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

  /** Post a described form to the command it names.
   *
   *  The values are sent under the field keys the server chose, so this page
   *  never learns what any of them mean. Numeric fields are converted because
   *  an input yields a string and the daemon's argument types do not — that is
   *  the only interpretation of the payload that happens here.
   *
   *  Validation is the daemon's: it is the side that probes the endpoint and
   *  checks the key. What this enforces is only `required`, so an obviously
   *  incomplete form says "not yet" instead of making a round trip to be told. */
  async function submitSection(section: OfferSection) {
    setErr(""); setMsg(""); setSectionBusy(true);
    try {
      const params: Record<string, string | number> = {};
      for (const f of section.fields) {
        const raw = (sectionDraft[f.key] ?? "").trim();
        if (f.type === "percent" || f.type === "int") {
          // An empty number field means "the default", which is the daemon's
          // to pick — sending 0 would be a value, and the wrong one.
          if (raw === "") continue;
          const n = parseInt(raw, 10);
          if (!Number.isNaN(n)) params[f.key] = n;
          continue;
        }
        if (raw !== "") params[f.key] = raw;
      }
      const r = await invoke<{ account_id?: string }>(section.command, params);
      setMsg(t("publish.sectionConnected", { section: section.title, account: r?.account_id ?? "" }));
      // The key is never read back; clear the form rather than leave a secret
      // sitting in a field the next submit would resend.
      openSectionForm(null);
      loadAccounts();
    } catch (e) {
      setErr(errText(e));
    } finally {
      setSectionBusy(false);
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
  const section = offer.sections.find((x) => x.id === openSection) ?? null;
  /** Whether a described form has everything it said it needed. The daemon
   *  validates properly — it is the side that probes the endpoint and checks
   *  the key — so this only stops an obviously incomplete submit. */
  const sectionReady = (x: OfferSection) =>
    x.fields.every((f) => !f.required || (sectionDraft[f.key] ?? "").trim() !== "");

  /** Only what this login has been granted. The list is the server's, so
   *  opening a family to every seller is a server change rather than a client
   *  release.
   *
   *  A family the server also described a form for is drawn by that form and
   *  not here: `custom` is a granted family *and* a described section, and a
   *  plain paste-a-key tile for it is both a second "Custom endpoint" in the
   *  grid and a dead end — it has no `base_url` to connect to. The section
   *  wins wherever both exist. */
  const offered = <T extends { id: string }>(list: T[]) =>
    list.filter(
      (p) => offer.providers.includes(p.id) && !offer.sections.some((x) => x.provider === p.id),
    );

  const connectGrid = (
    <>
      <div className="pick-grid">
        {offered(PROVIDERS).map((p) => (
          <button key={p.id} className="pick" onClick={() => connect(p.id)} disabled={busy || !inTauri}>
            <span className="pick-ico"><Mark id={p.id} /></span>
            <span>
              <span className="pick-title">{p.label}</span>
              <span className="pick-sub">{t("publish.connectVia")}</span>
            </span>
          </button>
        ))}
        {offered(DEVICE_PROVIDERS).map((p) => (
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
        {offered(KEY_PROVIDERS).map((p) => (
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
        {/* Forms the server described, for the families that need more than a
            pasted key. Empty for a login that has not been granted one. */}
        {offer.sections.map((x) => (
          <button
            key={x.id}
            className={`pick ${openSection === x.id ? "active" : ""}`}
            onClick={() => openSectionForm(openSection === x.id ? null : x)}
            disabled={busy || !inTauri}
          >
            <span className="pick-ico"><Mark id={x.provider} /></span>
            <span>
              <span className="pick-title">{x.title}</span>
              <span className="pick-sub">{t("publish.connectViaKey")}</span>
            </span>
          </button>
        ))}
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

      {/* A described form. Everything specific to it — its title, its fields,
          which hosts are worth one click — came from the server; what is here
          is how to draw six field types and where to post them. */}
      {section && (
        <div className="keyform fade-in">
          {section.hint && (
            <div className="callout info">
              <IconInfo />
              <span>{section.hint}</span>
            </div>
          )}
          {section.fields.map((f) => (
            <OfferFieldInput
              key={f.key}
              field={f}
              value={sectionDraft[f.key] ?? ""}
              onChange={(v) => setSectionDraft((d) => ({ ...d, [f.key]: v }))}
              onSubmit={() => { if (sectionReady(section) && !sectionBusy) submitSection(section); }}
            />
          ))}
          <div className="keyform-actions">
            <button
              className="btn sm"
              onClick={() => submitSection(section)}
              disabled={sectionBusy || !sectionReady(section)}
            >
              {sectionBusy ? t("publish.sectionChecking") : t("publish.keyConnect")}
            </button>
            <button className="btn sm ghost" onClick={() => openSectionForm(null)} disabled={sectionBusy}>
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
            <div className="text-sm" style={{ marginTop: "var(--s4)" }}>
              {t("publish.rankedLastBody", { score: sellerStatus.score, min: sellerStatus.min_score })}
            </div>
            <div className="text-sm faint" style={{ marginTop: "var(--s4)" }}>
              {t("publish.rankedLastHow")}
            </div>
          </div>
        </div>
      )}

      {/* Connecting comes first: with no account yet the rest of the page has
          nothing to show, and the empty state points "above" for OAuth. */}
      <div ref={connectCard}>
        <Card
          icon={<IconPlus />}
          title={t("publish.connectTitle")}
          desc={t("publish.connectDesc")}
        >
          {connectGrid}
          <Ok>{msg}</Ok>
        </Card>
      </div>

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
              // The provider's own utilisation, when it publishes one. There is
              // no second bar any more: a local count against a guessed plan cap
              // read 100% on a Claude subscription that was 3% spent, and a page
              // saying "spent" over lanes that are selling is worse than no page
              // at all. Without an upstream reading the bar falls back to the one
              // local number that is real — what this account sold today against
              // the operator's own daily cap — and to nothing at all when they
              // have not set one.
              const upstream = !a.key_based && a.window_used_percent != null;
              const used = a.used_today;
              const denom = limit;
              const pct = upstream
                ? (a.window_used_percent as number)
                : denom > 0 ? (used / denom) * 100 : 0;
              const capPct = a.daily_cap > 0 && limit > 0 && !a.key_based
                ? Math.round((limit / a.daily_cap) * 100)
                : 0;
              // Every lane of this account, switched on or not: the ranking
              // chart below doubles as this subscription's price board, and
              // "what would I be selling, and at what discount" is a question
              // worth being able to answer *before* flipping the switch.
              const own = lanes.filter((l) => l.provider === a.provider && l.account_id === a.account_id);
              // Every model this account *could* sell — the lanes are built
              // before the selection narrows anything, so the ones switched off
              // are still here to be switched back on.
              const modelOptions: ModelOption[] = own
                .map((l) => ({ id: l.model, label: marketModels[l.model]?.label, vendor: marketModels[l.model]?.vendor }))
                .sort((x, y) => (x.vendor ?? "").localeCompare(y.vendor ?? "") || x.id.localeCompare(y.id));
              const floor = floorOf(a);
              // How many requests this subscription serves at once, as declared
              // to the market. Named `slots` here because `lanes` in this scope
              // is already the account's (account, model) rows.
              const slots = slotsOf(a);
              // The daily cap is spent: `rebuild_pool` has already zeroed this
              // account's remaining cap, so every one of its models is off the
              // market until the UTC rollover. That is a whole-subscription
              // stop, and it earns a line of its own rather than being left to
              // be inferred from an `exhausted` pill. It is also the *only*
              // local rule that can stop an account now.
              const capSpent = limit > 0 && a.used_today >= limit;
              // The credential needs a person. Read off the *lanes* as well as
              // the account, because the two outlive each other by different
              // amounts: the `expired` status comes from an in-memory
              // `auth_failed` that a daemon restart forgets, while the lanes'
              // `auth` pause is on disk and comes back. Asking only the account
              // meant a restart quietly dropped the banner and left six lanes
              // labelled "sign-in needed" with nothing on the page saying why.
              const deadCred = a.status === "expired" || own.some((l) => l.paused_reason === "auth");
              const open = openAcct[k] ?? openByDefault;
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
                      <button
                        className={`icon-btn sm acct-fold${open ? " on" : ""}`}
                        onClick={() => setOpenAcct((o) => ({ ...o, [k]: !open }))}
                        aria-expanded={open}
                        title={open ? t("publish.foldHide") : t("publish.foldShow")}
                        aria-label={open ? t("publish.foldHide") : t("publish.foldShow")}
                      >
                        <IconChevronDown />
                      </button>
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
                      {/* Walk the same sign-in that created this account, on
                          demand rather than only once it has visibly died: a
                          subscription whose plan changed upstream, or whose
                          scopes were re-granted, reads as healthy here and
                          still needs the credential taken round again. */}
                      <button
                        className="icon-btn sm"
                        onClick={() => reconnect(a)}
                        disabled={!inTauri || busy}
                        title={t("publish.reconnect")}
                        aria-label={t("publish.reconnect")}
                      >
                        <IconRefresh />
                      </button>
                      <button className="icon-btn ghost-danger" onClick={() => removeAccount(a)} title={t("publish.remove")}>
                        <IconTrash />
                      </button>
                    </div>
                  </div>

                  {deadCred && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.expiredHint")}</span>
                      {canReauth(a) && (
                        <button
                          className="btn ghost sm callout-act"
                          onClick={() => reconnect(a)}
                          disabled={!inTauri || busy}
                        >
                          {t("publish.reauth")}
                        </button>
                      )}
                    </div>
                  )}

                  {/* A different failure that used to wear the same label. The
                      upstream refused this *machine* — a region it does not
                      serve, a proxy in the way — and the credential is fine, so
                      the one thing this banner must not do is send somebody off
                      to sign in again. */}
                  {own.some((l) => l.paused_reason === "blocked") && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.blockedHint")}</span>
                    </div>
                  )}

                  {capSpent && (
                    <div className="callout warn compact">
                      <IconInfo /><span>{t("publish.limitReached", { limit: fmtTokens(limit) })}</span>
                    </div>
                  )}

                  {/* Folded, the tile still has to answer the questions a seller
                      asks across accounts rather than inside one: how loaded it
                      is, what it will not sell below, how much it serves at
                      once, and how many of its models are actually on offer.
                      The banners above are deliberately *not* folded away — a
                      dead credential is not a detail. */}
                  {!open && (
                    <div className="acct-sum">
                      <span className="mono tabular">
                        {upstream || denom > 0
                          ? `${Math.round(Math.min(100, pct))}%`
                          : fmtTokens(used)}
                        <span className="faint">
                          {" "}
                          {upstream ? (a.window_key ?? "") : t("publish.limitUsedToday")}
                        </span>
                      </span>
                      <span className="mono tabular">{`\u2265 ${floor}%`} <span className="faint">{t("publish.unitOfList")}</span></span>
                      <span className="mono tabular">{slots} <span className="faint">{t("publish.unitRequests")}</span></span>
                      <span>
                        {a.sell_models.length > 0
                          ? t("publish.sellModelsSome", { n: a.sell_models.length, total: modelOptions.length })
                          : t("publish.sellModelsAll", { n: modelOptions.length })}
                      </span>
                    </div>
                  )}

                  {/* Throughput first — it is the number this whole page exists
                      to move. Against the 5h window for a subscription, against
                      the operator's daily cap for a key. */}
                  {open && (
                  <div className="acct-usage">
                    <div className="au-head">
                      <span>
                        {upstream
                          ? t("publish.windowUpstream", { window: a.window_key ?? "" })
                          : t("publish.limitUsedToday")}
                        {/* Whose number this is, and how old. A reading from the
                            vendor is worth saying out loud — it is the one that
                            decides whether these lanes sell. */}
                        {upstream && formatAge(a.window_as_of, t) && (
                          <span className="faint"> · {formatAge(a.window_as_of, t)}</span>
                        )}
                      </span>
                      <span className="mono tabular">
                        {upstream ? (
                          <>{Math.round(Math.min(100, pct))}%</>
                        ) : (
                          <>
                            {fmtTokens(used)}
                            {denom > 0 && <span className="faint"> / {fmtTokens(denom)}</span>}
                            {denom > 0 && <span className="au-pct"> · {Math.round(Math.min(100, pct))}%</span>}
                          </>
                        )}
                      </span>
                    </div>
                    <div className="bar">
                      <span
                        className={pct >= 90 ? "danger" : pct >= 70 ? "warn" : ""}
                        style={{ width: `${Math.min(100, pct)}%`, minWidth: pct > 0 ? 3 : 0 }}
                      />
                    </div>
                  </div>
                  )}

                  {open && (
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

                      {/* Which of this subscription's models are for sale. No
                          picks means all of them, which is the default and the
                          only answer that stays right as the platform prices
                          new models — narrowing is a decision, and it has to be
                          made again for anything new. */}
                      <label className="acct-sub-label after">{t("publish.sellModelsLabel")}</label>
                      <div className="value-row">
                        <span className="value-strong">
                          {a.sell_models.length > 0
                            ? t("publish.sellModelsSome", { n: a.sell_models.length, total: modelOptions.length })
                            : t("publish.sellModelsAll", { n: modelOptions.length })}
                        </span>
                        {modelsSaved === k && <span className="value-note ok">{t("publish.limitSaved")}</span>}
                      </div>
                      <ModelMultiSelect
                        options={modelOptions}
                        value={a.sell_models}
                        onChange={(next) => saveModels(a, next)}
                        disabled={!inTauri || !!pending[k]}
                        title={t("publish.sellModelsPick")}
                        addLabel={t("publish.sellModelsAdd")}
                      />
                      <div className="hint">{t("publish.sellModelsHint")}</div>
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
                      {/* No "quota left" row. The only honest answer to how much
                          a subscription has left is the provider's own
                          percentage, which is the bar above; the token figure
                          that used to sit here was `guessed cap − what we sold`,
                          and it was wrong by an order of magnitude. */}
                    </div>
                  </div>
                  )}

                  {/* Per-model price and state, then where the credential came
                      from. Both sit below a hairline: the chart answers "which
                      of my models is the market paying for" when that is the
                      question, and stays quiet the rest of the time. */}
                  {open && (own.length > 0 || a.sources.length > 0) && (
                    <div className="acct-detail">
                      {own.length > 0 && (
                        <DiscountRank
                          lanes={own}
                          floor={floor}
                          now={now}
                          onResume={resume}
                          onReauth={canReauth(a) ? () => reconnect(a) : undefined}
                          resuming={resuming}
                          verdicts={verification?.lanes ?? []}
                          gated={!!verification?.enabled && !!verification?.enforced}
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
          gated={!!verification?.enabled && !!verification?.enforced}
          onClose={() => setTesting("")}
        />
      )}

      {gateAccount && (
        <VerifyGateDialog
          key={gate}
          account={gateAccount}
          lanes={lanes.filter((l) => l.provider === gateAccount.provider && l.account_id === gateAccount.account_id)}
          verdicts={verification?.lanes ?? []}
          // Closing is a dismissal, not a decision: turning the switch on was
          // the decision, and the gateway is the thing holding the lane back
          // until a verdict lands. A seller who has changed their mind turns
          // the same switch off in the list behind this dialog.
          onClose={() => {
            setGate("");
            loadAccounts();
          }}
        />
      )}
    </div>
  );
}

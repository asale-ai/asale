import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, inTauri, type CustomEndpoint } from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Empty } from "../ui";
import { IconPlus, IconRefresh, IconTrash, IconChip, IconInfo } from "../icons";
import { errText } from "../errors";

/** The floor an endpoint sells at, in whole percent *of* list price — the same
 *  number, and the same convention, as a subscription's price band. 10 is below
 *  every price the market can quote, i.e. "sell at whatever it pays". */
const RATIO_MIN = 10;
const RATIO_MAX = 100;
/** A metered endpoint costs its operator real money per token, so the default
 *  is a real floor rather than the subscription side's "any price": selling
 *  below what the tokens cost is the one failure that is not self-correcting. */
const RATIO_DEFAULT = 60;

/** Mirrors the daemon's `store::SELL_CONCURRENCY_RANGE`. */
const SLOTS_MIN = 1;
const SLOTS_MAX = 64;
const SLOTS_DEFAULT = 5;

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

/** Read a form field as a number, falling back to `def` for anything
 *  unreadable — a half-typed value must not become a term of the sale. */
const num = (raw: string, def: number, lo: number, hi: number) => {
  const n = parseInt(raw, 10);
  return Number.isFinite(n) ? clamp(n, lo, hi) : def;
};

/**
 * Custom endpoints — internal.
 *
 * An OpenAI-compatible endpoint (a base URL and a key) sold as if it were a
 * subscription: same lanes, same price floor, same concurrency ceiling, same
 * metering. It exists to put supply behind models the subscription sellers
 * happen not to cover, which is why it is platform machinery rather than a
 * seller feature — the tab only appears when the daemon was started with
 * `ASALE_CUSTOM_ENDPOINTS=1`.
 *
 * Two things differ from an ordinary account and both are visible here:
 * the endpoint's own model list is what it *can* serve, and only the part of it
 * the platform trades is what it *does* sell. The page shows both numbers,
 * because "connected, 400 models, selling 12" is the answer that explains an
 * endpoint that looks live and earns nothing.
 */
export function Endpoints() {
  const { t } = useTranslation();
  const [list, setList] = useState<CustomEndpoint[]>([]);
  const [loading, setLoading] = useState(true);
  const [err, setErr] = useState("");
  const [msg, setMsg] = useState("");
  const [busy, setBusy] = useState(false);
  /** Per-endpoint in-flight flag, so one row's action does not disable them all. */
  const [pending, setPending] = useState<Record<string, boolean>>({});

  // ── the connect form ──
  const [base, setBase] = useState("");
  const [key, setKey] = useState("");
  const [label, setLabel] = useState("");
  const [floor, setFloor] = useState(String(RATIO_DEFAULT));
  const [slots, setSlots] = useState(String(SLOTS_DEFAULT));

  const load = useCallback(async () => {
    if (!inTauri) { setLoading(false); return; }
    try {
      const r = await invoke<{ endpoints: CustomEndpoint[] }>("list_custom_endpoints");
      setList(r.endpoints ?? []);
      setErr("");
    } catch (e) {
      setErr(errText(e));
    } finally {
      setLoading(false);
    }
  }, []);

  useEffect(() => { load(); }, [load]);

  /** Connect a new endpoint, or reconfigure an existing one under the same
   *  name. The daemon probes `GET {base}/models` before storing anything, so a
   *  wrong URL or a dead key fails here rather than on the first buyer. */
  async function connect() {
    setErr(""); setMsg(""); setBusy(true);
    try {
      const r = await invoke<{ account_id: string; endpoint_models: number; sellable_models: string[] }>(
        "connect_custom_endpoint",
        {
          baseUrl: base.trim(),
          apiKey: key.trim(),
          label: label.trim() || undefined,
          minRatio: num(floor, RATIO_DEFAULT, RATIO_MIN, RATIO_MAX),
          concurrency: num(slots, SLOTS_DEFAULT, SLOTS_MIN, SLOTS_MAX),
        },
      );
      setMsg(t("endpoints.connected", {
        account: r.account_id,
        served: r.endpoint_models,
        selling: r.sellable_models.length,
      }));
      // The key is never read back — clear it rather than leave a secret in a
      // field the next edit would resubmit.
      setKey("");
      setBase("");
      setLabel("");
      load();
    } catch (e) {
      setErr(errText(e));
    } finally {
      setBusy(false);
    }
  }

  /** Run one endpoint's action with its own pending flag and a reload after. */
  async function act(accountId: string, fn: () => Promise<unknown>) {
    setErr(""); setMsg("");
    setPending((p) => ({ ...p, [accountId]: true }));
    try {
      await fn();
      await load();
    } catch (e) {
      setErr(errText(e));
    } finally {
      setPending((p) => ({ ...p, [accountId]: false }));
    }
  }

  /** Flip the sell switch, or save one of the terms. Same RPC a subscription
   *  uses — one place decides what a lane offers, whatever kind of account is
   *  behind it. */
  const setSell = (e: CustomEndpoint, enabled: boolean, terms: { minRatio?: number; concurrency?: number } = {}) =>
    act(e.account_id, () => invoke("set_account_sell", {
      provider: "custom",
      accountId: e.account_id,
      enabled,
      ...terms,
    }));

  const refresh = (e: CustomEndpoint) =>
    act(e.account_id, async () => {
      const r = await invoke<{ endpoint_models: number; sellable_models: string[] }>(
        "refresh_custom_endpoint",
        { accountId: e.account_id },
      );
      setMsg(t("endpoints.refreshed", {
        account: e.account_id,
        served: r.endpoint_models,
        selling: r.sellable_models.length,
      }));
    });

  const remove = (e: CustomEndpoint) => {
    if (!window.confirm(t("endpoints.removeConfirm", { account: e.account_id }))) return;
    act(e.account_id, () => invoke("remove_custom_endpoint", { accountId: e.account_id }));
  };

  return (
    <div>
      <PageHead
        title={t("endpoints.title")}
        sub={t("endpoints.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("endpoints.reload")}
            onClick={load}
            disabled={!inTauri || loading}
            spinning={loading}
          />
        }
      />

      <Err>{err}</Err>

      <Card icon={<IconPlus />} title={t("endpoints.addTitle")} desc={t("endpoints.addDesc")}>
        <div className="keyform">
          <div className="keyform-hint">
            <IconInfo />
            <span>{t("endpoints.internalNote")}</span>
          </div>
          <div className="field">
            <label htmlFor="ep-base">{t("endpoints.baseLabel")}</label>
            <input
              id="ep-base"
              value={base}
              spellCheck={false}
              placeholder="https://openrouter.ai/api/v1"
              onChange={(ev) => setBase(ev.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor="ep-key">{t("endpoints.keyLabel")}</label>
            <input
              id="ep-key"
              type="password"
              autoComplete="off"
              spellCheck={false}
              value={key}
              placeholder="sk-…"
              onChange={(ev) => setKey(ev.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor="ep-label">{t("endpoints.nameLabel")}</label>
            <input
              id="ep-label"
              value={label}
              placeholder={t("endpoints.namePlaceholder")}
              onChange={(ev) => setLabel(ev.target.value)}
            />
          </div>
          <div className="field">
            <label htmlFor="ep-floor">{t("endpoints.floorLabel")}</label>
            <div className="input-row">
              <span className="band-cap">≥</span>
              <input
                id="ep-floor"
                className="mono band-input"
                type="number"
                min={RATIO_MIN}
                max={RATIO_MAX}
                value={floor}
                onChange={(ev) => setFloor(ev.target.value)}
              />
              <span className="unit">%</span>
            </div>
            <div className="hint">{t("endpoints.floorHint")}</div>
          </div>
          <div className="field">
            <label htmlFor="ep-slots">{t("endpoints.slotsLabel")}</label>
            <div className="input-row">
              <input
                id="ep-slots"
                className="mono band-input"
                type="number"
                min={SLOTS_MIN}
                max={SLOTS_MAX}
                value={slots}
                onChange={(ev) => setSlots(ev.target.value)}
              />
              <span className="unit">{t("publish.unitRequests")}</span>
            </div>
            <div className="hint">{t("endpoints.slotsHint")}</div>
          </div>
          <div className="keyform-actions">
            <button
              className="btn sm"
              onClick={connect}
              disabled={!inTauri || busy || !base.trim() || !key.trim()}
            >
              {busy ? t("endpoints.connecting") : t("endpoints.connect")}
            </button>
          </div>
          <Ok>{msg}</Ok>
        </div>
      </Card>

      <Card
        icon={<IconChip />}
        title={t("endpoints.listTitle")}
        desc={t("endpoints.listDesc")}
        right={<span className="count-chip">{loading ? "—" : list.length}</span>}
      >
        {loading ? (
          <SkeletonRows rows={2} />
        ) : list.length === 0 ? (
          <Empty icon={<IconChip />} title={t("endpoints.emptyTitle")} desc={t("endpoints.emptyDesc")} />
        ) : (
          list.map((e) => {
            const wait = !!pending[e.account_id];
            return (
              <div className={`acct ${e.sell_enabled ? "selling" : ""}`} key={e.account_id}>
                <div className="acct-head">
                  <div className="acct-id">
                    <div className="acct-name">{e.account_id}</div>
                    <div className="acct-meta">
                      <span className="mono">{e.base_url}</span>
                    </div>
                  </div>
                  <div className="acct-actions">
                    {/* The switch is the same per-account one the sell page
                        drives; an endpoint that is off keeps its terms. */}
                    <label className="switch" title={t("endpoints.sellSwitch")}>
                      <input
                        type="checkbox"
                        checked={e.sell_enabled}
                        onChange={(ev) => setSell(e, ev.target.checked)}
                        disabled={!inTauri || wait}
                      />
                      <span className="track" />
                    </label>
                    <IconAction
                      icon={<IconRefresh />}
                      label={t("endpoints.refresh")}
                      onClick={() => refresh(e)}
                      disabled={!inTauri || wait}
                      spinning={wait}
                    />
                    <IconAction
                      icon={<IconTrash />}
                      label={t("endpoints.remove")}
                      onClick={() => remove(e)}
                      disabled={!inTauri || wait}
                    />
                  </div>
                </div>

                <div className="fact-grid tight">
                  <div className="fact">
                    <span className="fact-k">{t("endpoints.floorFact")}</span>
                    <span className="fact-v mono">
                      {e.min_ratio > RATIO_MIN ? `≥ ${e.min_ratio}%` : t("publish.bandNone")}
                    </span>
                  </div>
                  <div className="fact">
                    <span className="fact-k">{t("endpoints.slotsFact")}</span>
                    <span className="fact-v mono">{e.concurrency}</span>
                  </div>
                  <div className="fact">
                    <span className="fact-k">{t("endpoints.sellingFact")}</span>
                    <span className="fact-v mono">{e.sellable_models.length}</span>
                  </div>
                </div>

                {/* What is actually on the market from this endpoint. The list
                    is the answer to "it says connected, why is it earning
                    nothing" — an endpoint can serve hundreds of models and
                    share none with the platform's catalog. */}
                {e.sellable_models.length > 0 ? (
                  <details className="ep-models">
                    <summary>{t("endpoints.modelsSummary", { n: e.sellable_models.length })}</summary>
                    <div className="ep-model-list">
                      {e.sellable_models.map((m) => (
                        <span className="chip mono" key={m}>{m}</span>
                      ))}
                    </div>
                  </details>
                ) : (
                  <div className="hint">{t("endpoints.noOverlap")}</div>
                )}
              </div>
            );
          })
        )}
      </Card>
    </div>
  );
}

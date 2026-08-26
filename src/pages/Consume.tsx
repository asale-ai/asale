import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, pricePerMillion, fmtContext, isSignedOut, gotoSignIn, requireSignIn,
  DaemonError, type BuyTool, type BuyTools, type MarketModel, type ToolProcesses,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Mark } from "../ui";
import { ModelMultiSelect, type ModelOption } from "../components/ModelPicker";
import { IconRoute, IconConsume, IconRefresh, IconCheck, IconAlert, IconX, IconExternal } from "../icons";
import { errText } from "../errors";

const priceOf = (m: MarketModel, type: string) => m.prices.find((p) => p.token_type === type);
const usd = (micros: number) => `$${pricePerMillion(micros).toFixed(2)}`;

/**
 * Market catalog → picker options. The catalog is a few hundred models wide, so
 * each row carries the numbers a buyer actually chooses on: per-million in/out
 * price, context window, and the market's discount off the reference price.
 *
 * A model is listed because it is *priced*, which is not the same as being
 * *available*: the catalog carries every model the platform knows how to
 * settle, and at any moment all but a dozen have nobody selling them. Those
 * are filtered out of the picker by default — see `ModelOption.unavailable`.
 */
function toOptions(
  market: MarketModel[],
  t: (k: string, o?: Record<string, unknown>) => string,
): ModelOption[] {
  return market
    .map((m) => {
      const input = priceOf(m, "input");
      const output = priceOf(m, "output");
      const bits: string[] = [];
      if (input && output) bits.push(t("consume.priceMeta", { in: usd(input.market_price), out: usd(output.market_price) }));
      if (m.context_length > 0) bits.push(t("consume.contextMeta", { n: fmtContext(m.context_length) }));
      // Discount is only meaningful when the market really undercuts the list
      // price; the pricing loop reprices continuously, so this moves around.
      const off = input && input.ref_price > input.market_price
        ? Math.round((1 - input.market_price / input.ref_price) * 100)
        : 0;
      return {
        id: m.model,
        label: m.display_name || m.model,
        vendor: m.provider,
        meta: bits.join(" · ") || undefined,
        tag: off > 0 ? `-${off}%` : undefined,
        // Lanes, not capacity. A lane asking more than the market currently
        // pays is withheld: it is online and has tokens to sell, so it carries
        // capacity, but no buyer will ever be routed to it. Reading capacity
        // here listed those models as available and every one of them failed on
        // the first call — `online_lanes` is the count that has already had the
        // price test applied (see the server's `SupplyView::servable`).
        unavailable: m.online_lanes > 0 ? undefined : t("consume.noSupply"),
      };
    })
    .sort((a, b) => (a.label ?? a.id).localeCompare(b.label ?? b.id));
}

export function Consume() {
  const { t } = useTranslation();

  const [tools, setTools] = useState<BuyTool[]>([]);
  /** The market catalog, whole. The picker is what collapses it to one row per
   *  model family and hides what has no supply. */
  const [models, setModels] = useState<ModelOption[]>([]);
  const [loading, setLoading] = useState(inTauri);
  /** Tools with an action in flight, keyed by tool id. */
  const [pending, setPending] = useState<Record<string, boolean>>({});
  /** Tools changed since this page was opened. A running CLI reads its config
   *  once at startup, so neither the switch nor a model edit reaches a session
   *  that is already up — the row keeps saying so until the user dismisses it. */
  const [restart, setRestart] = useState<Record<string, boolean>>({});
  /** The sessions that reminder is actually about, or null before anything has
   *  been changed — the scan costs a process-table read, so it is only worth
   *  doing once there is a restart to ask for. */
  const [procs, setProcs] = useState<ToolProcesses | null>(null);
  const [scanning, setScanning] = useState(false);
  /** Config files with an open in flight, keyed by path. */
  const [opening, setOpening] = useState<Record<string, boolean>>({});
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const [refreshing, setRefreshing] = useState(false);

  const loadTools = useCallback(() => {
    if (!inTauri) return Promise.resolve();
    return invoke<BuyTools>("buy_tools")
      .then((r) => {
        setTools(r.tools || []);
        // The daemon repairs a drifted config while answering this call. Say
        // so — the file did change under the user, and the CLI that is already
        // running still needs restarting for it to matter.
        const fixed = (r.repaired || [])
          .map((id) => (r.tools || []).find((x) => x.id === id)?.label || id);
        if (fixed.length) setMsg(t("consume.repaired", { tools: fixed.join(t("common.listSep")) }));
      })
      .catch((e) => setErr(errText(e)));
  }, [t]);

  const refresh = useCallback(() => {
    setRefreshing(true);
    loadTools().finally(() => setRefreshing(false));
  }, [loadTools]);

  /** Look for CLI sessions still running on the config we just replaced.
   *
   *  Best-effort by design: a machine whose process table the daemon cannot
   *  read answers `scanned: false`, and the reminder stays the plain sentence it
   *  has always been rather than claiming nothing is running. A failed call is
   *  the same case and is deliberately not shown as an error — nothing the user
   *  asked for has failed. */
  const scanProcs = useCallback(() => {
    if (!inTauri) return;
    setScanning(true);
    invoke<ToolProcesses>("tool_processes")
      .then(setProcs)
      .catch(() => setProcs({ scanned: false, running: {} }))
      .finally(() => setScanning(false));
  }, []);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    Promise.allSettled([
      loadTools(),
      invoke<{ models: MarketModel[] }>("market_models")
        .then((r) => setModels(toOptions(r.models || [], t)))
        .catch(() => {}),
    ]).finally(() => setLoading(false));
  }, [loadTools, t]);

  /** Flip a tool's buy switch, and/or replace its model selection. */
  async function setBuy(tool: BuyTool, enabled: boolean, nextModels?: string[]) {
    setErr(""); setMsg("");
    // Buying needs a session (the daemon mints a consumer key against it), so
    // check before touching the switch: a signed-out user belongs on the
    // sign-in form, not in front of a switch that flicks on and back off.
    // Switching *off* is exempt — restoring the original config is local work,
    // and a signed-out user must always be able to undo.
    if (enabled && !(await requireSignIn("errors.session.signInToBuy"))) return;
    setPending((p) => ({ ...p, [tool.id]: true }));
    // Optimistic so the switch and the chips respond immediately.
    setTools((list) =>
      list.map((x) => (x.id === tool.id
        ? { ...x, enabled, models: nextModels ?? x.models }
        : x)),
    );
    try {
      await invoke("set_buy_tool", {
        tool: tool.id,
        enabled,
        ...(nextModels === undefined ? {} : { models: nextModels }),
      });
      if (enabled !== tool.enabled) {
        setMsg(enabled ? t("consume.buyOnDone", { tool: tool.label }) : t("consume.buyOffDone", { tool: tool.label }));
      } else if (nextModels !== undefined) {
        setMsg(t("consume.modelsDone", { tool: tool.label }));
      }
      setRestart((r) => ({ ...r, [tool.id]: true }));
      // Which sessions are on the config that just changed — asked for now
      // rather than on page load, because this is the first moment it matters.
      scanProcs();
      await loadTools();
    } catch (e) {
      setErr(errText(e));
      // A session that expired between the check above and this call (or one
      // the server rejected for its own reasons) lands here — same remedy.
      if (isSignedOut(e)) gotoSignIn((e as DaemonError).key);
      await loadTools(); // roll the optimistic update back to server truth
    } finally {
      setPending((p) => ({ ...p, [tool.id]: false }));
    }
  }

  /** Open a config file in whatever the OS opens it with.
   *
   *  The daemon does the opening, so the file opens on the machine the config
   *  is on — which is the daemon's, not the browser's, when the two differ. A
   *  path the switch has not written yet has no file to open; the daemon says
   *  so by opening the folder instead, and the message tells the user that is
   *  what happened. */
  async function openConfig(path: string) {
    setErr(""); setMsg("");
    setOpening((o) => ({ ...o, [path]: true }));
    try {
      const r = await invoke<{ path: string; folder: boolean }>("open_config_path", { path });
      setMsg(t(r.folder ? "consume.openedFolder" : "consume.openedFile", { path: r.path }));
    } catch (e) {
      setErr(errText(e));
    } finally {
      setOpening((o) => ({ ...o, [path]: false }));
    }
  }

  /** A tool's picks, deduped. Ids no longer in the catalog are kept as-is so a
   *  selection is never silently dropped from under the user. */
  const pickedOf = (tool: BuyTool) => [...new Set(tool.models)];

  return (
    <div>
      <PageHead
        title={t("consume.title")}
        sub={t("consume.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("consume.refresh")}
            onClick={refresh}
            disabled={!inTauri || refreshing}
            spinning={refreshing}
          />
        }
      />

      <Card
        icon={<IconConsume />}
        title={t("consume.toolsTitle")}
        desc={t("consume.toolsDesc")}
        right={<span className="count-chip">{loading ? "—/—" : `${tools.filter((x) => x.installed).length}/${tools.length}`}</span>}
      >
        {loading ? (
          <SkeletonRows rows={3} />
        ) : (
          <div className="acct-list">
            {tools.map((tool) => {
              const busy = !!pending[tool.id];
              // The switch is on but the config no longer points at us, and the
              // daemon's automatic re-apply could not fix it either (no cached
              // key, or the file is not writable) — everything it *can* fix is
              // already fixed by the time `buy_tools` answers.
              //
              // Never while a call is in flight: the switch is optimistic, so
              // between the click and the answer every tool being turned on
              // reads as "enabled, not in effect" and would flash a warning
              // blaming another program for a config nothing has written yet.
              const drifted = tool.enabled && !tool.in_effect && !busy;
              return (
                <div key={tool.id} className={`acct ${tool.enabled ? "selling" : ""} ${tool.installed ? "" : "muted-row"}`}>
                  <div className="acct-head">
                    <Mark id={tool.id} />
                    <div className="acct-id">
                      <div className="acct-name">{tool.label}</div>
                      <div className="acct-meta">
                        {tool.installed
                          ? <span className="pill on plain"><IconCheck /> {t("consume.installed")}</span>
                          : <span className="pill off">{t("consume.notInstalled")}</span>}
                        {tool.account
                          ? <span className="mono muted">{tool.account}{tool.plan && ` · ${tool.plan}`}</span>
                          : tool.installed && <span className="muted">{t("consume.noAccount")}</span>}
                        {tool.enabled && tool.in_effect && <span className="pill on">{t("consume.inEffect")}</span>}
                      </div>
                    </div>
                    <label className="switch" title={t("consume.buySwitch")}>
                      <input
                        type="checkbox"
                        checked={tool.enabled}
                        onChange={(e) => setBuy(tool, e.target.checked)}
                        disabled={!inTauri || busy || !tool.installed}
                      />
                      <span className="track" />
                    </label>
                  </div>

                  {drifted && (
                    <div className="callout warn compact">
                      <IconAlert /><span>{t("consume.drifted", { path: tool.config_path })}</span>
                      {/* Re-applying is what the daemon just tried and what the
                          old copy asked the user to do by hand ("switch it off
                          and on again"); the button does it in one click, and
                          keeps the backup taken when the switch went on. */}
                      <button
                        type="button"
                        className="btn ghost sm callout-act"
                        onClick={() => setBuy(tool, true)}
                        disabled={!inTauri || busy}
                      >
                        {t("consume.reapply")}
                      </button>
                    </div>
                  )}

                  {/* The change is on disk; the tool won't see it until it is
                      started again. Dismissible — it is a reminder, not a state
                      we can observe.

                      Under the sentence, the sessions the reminder is about:
                      the daemon can find the processes still holding the old
                      config, which turns "restart it" from advice into a list
                      you can check off. It only ever looks — a process attached
                      to somebody's terminal cannot be restarted from here, and
                      killing one would throw away whatever it was doing (see
                      `proc_scan`). */}
                  {restart[tool.id] && (
                    <div className="callout info compact">
                      <IconRefresh />
                      <div className="callout-body">
                        <span>{t("consume.restartNeeded", { tool: tool.label })}</span>
                        {procs?.scanned && (
                          <div className="callout-found">
                            {(procs.running[tool.id] ?? []).length === 0 ? (
                              <span className="muted">{t("consume.restartNoneRunning", { tool: tool.label })}</span>
                            ) : (
                              <>
                                <span className="muted">
                                  {t("consume.restartRunning", { n: procs.running[tool.id].length })}
                                </span>
                                {procs.running[tool.id].map((p) => (
                                  <span key={p.pid} className="pill mono plain" title={p.cmd}>
                                    PID {p.pid}
                                  </span>
                                ))}
                              </>
                            )}
                            <button
                              type="button"
                              className="btn sm subtle"
                              onClick={scanProcs}
                              disabled={!inTauri || scanning}
                            >
                              {t("consume.restartRescan")}
                            </button>
                          </div>
                        )}
                      </div>
                      <button
                        type="button"
                        className="callout-x"
                        onClick={() => setRestart((r) => ({ ...r, [tool.id]: false }))}
                        title={t("consume.restartDismiss")}
                      >
                        <IconX />
                      </button>
                    </div>
                  )}

                  {/* The model picker only applies to a tool that is buying. */}
                  {tool.enabled && (
                    <div>
                      <div className="acct-sub-label">
                        {t("consume.pickModels")}
                        {tool.models.length === 0 && <> · <b>{t("consume.anyModel")}</b></>}
                      </div>
                      {/* Rendered even when the catalog is unreachable: the
                          picks already saved must stay visible and removable. */}
                      <ModelMultiSelect
                        options={models}
                        value={pickedOf(tool)}
                        onChange={(next) => setBuy(tool, tool.enabled, next)}
                        disabled={!inTauri || busy || !tool.installed}
                        title={t("consume.pickModelsFor", { tool: tool.label })}
                      />
                      {models.length === 0 && (
                        <div className="acct-sub-label after">{t("consume.noModels")}</div>
                      )}
                      {/* Codex takes the model from its own catalog, not from
                          the request, so "any model" leaves it on models the
                          market cannot serve. */}
                      {tool.id === "codex" && tool.models.length === 0 && (
                        <div className="callout warn compact card-foot">
                          <IconAlert /><span>{t("consume.codexNeedsModel")}</span>
                        </div>
                      )}
                      {/* Same shape, different reason: opencode offers exactly
                          the models its config names, so an empty selection
                          leaves the provider installed with nothing to pick. */}
                      {tool.id === "opencode" && tool.models.length === 0 && (
                        <div className="callout warn compact card-foot">
                          <IconAlert /><span>{t("consume.opencodeNeedsModel")}</span>
                        </div>
                      )}
                      {/* Same shape once more: a custom Harness route serves
                          the models it lists and nothing else. */}
                      {tool.id === "dsh" && tool.models.length === 0 && (
                        <div className="callout warn compact card-foot">
                          <IconAlert /><span>{t("consume.dshNeedsModel")}</span>
                        </div>
                      )}
                    </div>
                  )}

                  <div className="acct-detail">
                    <div className="ad-row">
                      <span className="meta-k">{t("consume.configPaths")}</span>
                      <div className="ad-chips">
                        {/* The path is also the way in: reading the file asale
                            rewrote used to mean copying the path out and
                            hunting it down by hand. */}
                        {tool.config_paths.map((p) => (
                          <button
                            key={p}
                            type="button"
                            className="pill mono plain act"
                            title={t("consume.openConfig", { path: p })}
                            onClick={() => openConfig(p)}
                            disabled={!inTauri || !!opening[p]}
                          >
                            <span>{p}</span>
                            <IconExternal />
                          </button>
                        ))}
                      </div>
                    </div>
                  </div>
                </div>
              );
            })}
          </div>
        )}
        <Ok>{msg}</Ok>
        <Err>{err}</Err>
        {!loading && (
          <div className="callout card-foot">
            <IconRoute /><span>{t("consume.restartHint")}</span>
          </div>
        )}
      </Card>
    </div>
  );
}

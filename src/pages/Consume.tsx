import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri, pricePerMillion, fmtContext,
  type BuyTool, type BuyTools, type MarketModel,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Mark } from "../ui";
import { ModelMultiSelect, type ModelOption } from "../components/ModelPicker";
import { IconRoute, IconConsume, IconRefresh, IconCheck, IconAlert } from "../icons";

const priceOf = (m: MarketModel, type: string) => m.prices.find((p) => p.token_type === type);
const usd = (micros: number) => `$${pricePerMillion(micros).toFixed(2)}`;

/**
 * Market catalog → picker options. The catalog is a few hundred models wide, so
 * each row carries the numbers a buyer actually chooses on: per-million in/out
 * price, context window, and the market's discount off the reference price.
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
      };
    })
    .sort((a, b) => (a.label ?? a.id).localeCompare(b.label ?? b.id));
}

export function Consume() {
  const { t } = useTranslation();

  const [tools, setTools] = useState<BuyTool[]>([]);
  /** The market's models, deduped by family — one picker row per model name. */
  const [models, setModels] = useState<ModelOption[]>([]);
  const [loading, setLoading] = useState(inTauri);
  /** Tools with an action in flight, keyed by tool id. */
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");
  const [refreshing, setRefreshing] = useState(false);

  const loadTools = useCallback(() => {
    if (!inTauri) return Promise.resolve();
    return invoke<BuyTools>("buy_tools")
      .then((r) => setTools(r.tools || []))
      .catch((e) => setErr(String((e as Error).message)));
  }, []);

  const refresh = useCallback(() => {
    setRefreshing(true);
    loadTools().finally(() => setRefreshing(false));
  }, [loadTools]);

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
      }
      await loadTools();
    } catch (e) {
      setErr(String((e as Error).message));
      await loadTools(); // roll the optimistic update back to server truth
    } finally {
      setPending((p) => ({ ...p, [tool.id]: false }));
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
        right={<span className="count-chip">{tools.filter((x) => x.installed).length}/{tools.length}</span>}
      >
        {loading ? (
          <SkeletonRows rows={3} />
        ) : (
          <div className="acct-list">
            {tools.map((tool) => {
              const busy = !!pending[tool.id];
              // The switch is on but the config no longer points at us — a
              // manual edit, or another switcher took the file over.
              const drifted = tool.enabled && !tool.in_effect;
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
                    </div>
                  )}

                  <div className="acct-detail">
                    <div className="ad-row">
                      <span className="meta-k">{t("consume.configPaths")}</span>
                      <div className="ad-chips">
                        {tool.config_paths.map((p) => (
                          <span key={p} className="pill mono plain" title={p}><span>{p}</span></span>
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

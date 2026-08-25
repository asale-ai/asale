import { useCallback, useEffect, useState, type JSX } from "react";
import { useTranslation } from "react-i18next";
import {
  invoke, inTauri,
  type FirewallState, type FirewallTool, type FirewallMode, type FirewallScanner,
  type FirewallEvents, type FirewallVerdict, type FirewallPolicy,
} from "../lib";
import { Card, Ok, Err, SkeletonRows, PageHead, IconAction, Section, Empty, Mark } from "../ui";
import {
  IconShield, IconRefresh, IconAlert, IconCheck, IconKey, IconTerminal,
  IconRoute, IconEyeOff, IconZap,
} from "../icons";
import { errText } from "../errors";

/** The three modes, in the order they escalate. Kept here rather than served
 *  because the *order* is the argument the page is making: you start on the
 *  left and move right once you have looked at what it caught. */
const MODES: FirewallMode[] = ["audit", "balanced", "strict"];

/** One glyph per scanner. The ids come from the daemon; only the picture is
 *  ours. */
const SCANNER_ICON: Record<FirewallScanner, JSX.Element> = {
  secret: <IconKey />,
  injection: <IconZap />,
  hidden_unicode: <IconEyeOff />,
  tool_policy: <IconTerminal />,
  egress: <IconRoute />,
};

/** Which policy field a scanner's switch writes. */
const SCANNER_FIELD: Record<FirewallScanner, keyof FirewallPolicy> = {
  secret: "secret_scan",
  injection: "injection_scan",
  hidden_unicode: "hidden_unicode",
  tool_policy: "tool_policy",
  egress: "egress",
};

/** What the user can paste into the scratchpad, and how it is judged. Default
 *  is `tool_output` — something that came *back* — because that is the case
 *  this page exists to explain. */
const CHECK_KINDS = ["tool_output", "prompt", "tool_call", "url"] as const;
type CheckKind = (typeof CHECK_KINDS)[number];

const fmtTime = (ts: number) => new Date(ts * 1000).toLocaleString();

/** Comma/newline-separated text ↔ list, for the three advanced fields. */
const toList = (s: string) => s.split(/[\s,]+/).map((x) => x.trim()).filter(Boolean);

export function Security() {
  const { t } = useTranslation();

  const [state, setState] = useState<FirewallState | null>(null);
  const [events, setEvents] = useState<FirewallEvents | null>(null);
  const [loading, setLoading] = useState(inTauri);
  const [refreshing, setRefreshing] = useState(false);
  /** Rows with a call in flight, keyed by tool id or policy field. */
  const [pending, setPending] = useState<Record<string, boolean>>({});
  const [msg, setMsg] = useState("");
  const [err, setErr] = useState("");

  // The scratchpad.
  const [sample, setSample] = useState("");
  const [kind, setKind] = useState<CheckKind>("tool_output");
  const [verdict, setVerdict] = useState<FirewallVerdict | null>(null);
  const [checking, setChecking] = useState(false);

  // The three list fields are edited as text and committed on blur, so typing a
  // comma does not fire a write per keystroke.
  const [lists, setLists] = useState({ allow: "", deny: "", suppress: "" });

  const load = useCallback(() => {
    if (!inTauri) return Promise.resolve();
    return Promise.allSettled([
      invoke<FirewallState>("firewall_policy").then((r) => {
        setState(r);
        setLists({
          allow: r.policy.allow_hosts.join(", "),
          deny: r.policy.deny_hosts.join(", "),
          suppress: r.policy.suppress.join(", "),
        });
      }),
      invoke<FirewallEvents>("firewall_events", { limit: 50 }).then(setEvents),
    ]).then(([policy]) => {
      if (policy.status === "rejected") setErr(errText(policy.reason));
    });
  }, []);

  useEffect(() => {
    if (!inTauri) { setLoading(false); return; }
    load().finally(() => setLoading(false));
  }, [load]);

  const refresh = useCallback(() => {
    setRefreshing(true);
    load().finally(() => setRefreshing(false));
  }, [load]);

  /** Apply a policy answer. Every mutating command answers with the whole
   *  policy, so the page never has to guess what its own edit produced. */
  const applied = (r: { policy: FirewallPolicy }) =>
    setState((s) => (s ? { ...s, policy: r.policy, tools: s.tools.map((x) => ({ ...x, ...r.policy.tools[x.id] })) } : s));

  const setTool = (tool: FirewallTool, patch: { enabled?: boolean; mode?: FirewallMode }) => {
    const next = { enabled: patch.enabled ?? tool.enabled, mode: patch.mode ?? tool.mode };
    setErr(""); setMsg("");
    setPending((p) => ({ ...p, [tool.id]: true }));
    // Optimistic: the switch has to move under the finger, and the answer
    // carries the authoritative policy a moment later.
    setState((s) => (s ? { ...s, tools: s.tools.map((x) => (x.id === tool.id ? { ...x, ...next } : x)) } : s));
    invoke<{ policy: FirewallPolicy }>("set_firewall_tool", { tool: tool.id, ...next })
      .then(applied)
      .catch((e) => { setErr(errText(e)); load(); })
      .finally(() => setPending((p) => ({ ...p, [tool.id]: false })));
  };

  const setOption = (patch: Partial<FirewallPolicy>) => {
    const key = Object.keys(patch)[0];
    setErr(""); setMsg("");
    setPending((p) => ({ ...p, [key]: true }));
    setState((s) => (s ? { ...s, policy: { ...s.policy, ...patch } } : s));
    invoke<{ policy: FirewallPolicy }>("set_firewall_options", patch)
      .then(applied)
      .catch((e) => { setErr(errText(e)); load(); })
      .finally(() => setPending((p) => ({ ...p, [key]: false })));
  };

  const check = () => {
    if (!sample.trim()) return;
    setChecking(true); setErr("");
    invoke<{ verdict: FirewallVerdict }>("firewall_check", { text: sample, kind })
      .then((r) => setVerdict(r.verdict))
      .catch((e) => setErr(errText(e)))
      .finally(() => setChecking(false));
  };

  const policy = state?.policy;
  const guarded = state?.tools.filter((x) => x.enabled).length ?? 0;
  const enforcing = state?.tools.some((x) => x.enabled && x.mode !== "audit") ?? false;

  return (
    <>
      <PageHead
        title={t("security.title")}
        sub={t("security.sub")}
        actions={
          <IconAction
            icon={<IconRefresh />}
            label={t("security.refresh")}
            onClick={refresh}
            disabled={!inTauri || refreshing}
            spinning={refreshing}
          />
        }
      />

      {msg && <Ok>{msg}</Ok>}
      {err && <Err>{err}</Err>}

      {/* The log is evidence about processes that had a shell on this machine,
          so a broken chain is the one thing on this page that outranks the
          settings under it. */}
      {events?.tampered_at != null && (
        <Err>{t("security.tampered", { n: events.tampered_at + 1, path: state?.log_path })}</Err>
      )}

      <Card
        icon={<IconShield />}
        title={t("security.agentsTitle")}
        desc={t("security.agentsDesc")}
        right={<span className="count-chip">{loading ? "—/—" : `${guarded}/${state?.tools.length ?? 0}`}</span>}
      >
        {loading || !state ? (
          <SkeletonRows rows={3} />
        ) : (
          <div className="acct-list">
            {state.tools.map((tool) => {
              const busy = !!pending[tool.id];
              return (
                <div key={tool.id} className={`acct ${tool.enabled ? "selling" : ""} ${tool.installed ? "" : "muted-row"}`}>
                  <div className="acct-head">
                    <Mark id={tool.id} />
                    <div className="acct-id">
                      <div className="acct-name">{tool.label}</div>
                      <div className="acct-meta">
                        {tool.installed
                          ? <span className="pill on plain"><IconCheck /> {t("security.installed")}</span>
                          : <span className="pill off">{t("security.notInstalled")}</span>}
                        {tool.enabled && (
                          <span className={`pill ${tool.mode === "audit" ? "warn" : "on"}`}>
                            {t(`security.mode.${tool.mode}`)}
                          </span>
                        )}
                      </div>
                    </div>
                    <label className="switch" title={t("security.toolSwitch")}>
                      <input
                        type="checkbox"
                        checked={tool.enabled}
                        onChange={(e) => setTool(tool, { enabled: e.target.checked })}
                        disabled={!inTauri || busy}
                      />
                      <span className="track" />
                    </label>
                  </div>

                  {tool.enabled && (
                    <div className="fw-modes">
                      <div className="segmented sm">
                        {MODES.map((m) => (
                          <button
                            key={m}
                            type="button"
                            className={tool.mode === m ? "active" : ""}
                            onClick={() => setTool(tool, { mode: m })}
                            disabled={!inTauri || busy}
                          >
                            {t(`security.mode.${m}`)}
                          </button>
                        ))}
                      </div>
                      <span className="muted fw-mode-hint">{t(`security.modeHint.${tool.mode}`)}</span>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        )}
        {/* A protection that only covers traffic routed through this machine's
            proxy has to say so, or it will be read as covering everything. */}
        <p className="card-foot muted">{t("security.scopeNote")}</p>
      </Card>

      <Card icon={<IconZap />} title={t("security.scannersTitle")} desc={t("security.scannersDesc")}>
        {loading || !state || !policy ? (
          <SkeletonRows rows={4} />
        ) : (
          <div className="acct-list">
            {state.scanners.map((s) => {
              const field = SCANNER_FIELD[s.id];
              const on = !!policy[field];
              return (
                <div key={s.id} className={`acct ${on ? "selling" : ""}`}>
                  <div className="acct-head">
                    <div className="card-ico">{SCANNER_ICON[s.id]}</div>
                    <div className="acct-id">
                      <div className="acct-name">{t(`security.scanner.${s.id}.name`)}</div>
                      <div className="acct-meta">
                        <span className="pill plain">{t(`security.direction.${s.direction}`)}</span>
                        {s.rules > 0 && <span className="muted num">{t("security.ruleCount", { n: s.rules })}</span>}
                      </div>
                      <div className="muted fw-scanner-desc">{t(`security.scanner.${s.id}.desc`)}</div>
                    </div>
                    <label className="switch" title={t(`security.scanner.${s.id}.name`)}>
                      <input
                        type="checkbox"
                        checked={on}
                        onChange={(e) => setOption({ [field]: e.target.checked } as Partial<FirewallPolicy>)}
                        disabled={!inTauri || !!pending[field]}
                      />
                      <span className="track" />
                    </label>
                  </div>
                </div>
              );
            })}
          </div>
        )}
      </Card>

      {policy && (
        <Card icon={<IconRoute />} title={t("security.handlingTitle")} desc={t("security.handlingDesc")}>
          <div className="acct-list">
            {([
              ["redact_secrets", policy.redact_secrets],
              ["audit_log", policy.audit_log],
            ] as const).map(([field, on]) => (
              <div key={field} className="acct">
                <div className="acct-head">
                  <div className="acct-id">
                    <div className="acct-name">{t(`security.option.${field}.name`)}</div>
                    <div className="muted fw-scanner-desc">{t(`security.option.${field}.desc`)}</div>
                  </div>
                  <label className="switch">
                    <input
                      type="checkbox"
                      checked={on}
                      onChange={(e) => setOption({ [field]: e.target.checked } as Partial<FirewallPolicy>)}
                      disabled={!inTauri || !!pending[field]}
                    />
                    <span className="track" />
                  </label>
                </div>
              </div>
            ))}
          </div>

          <Section title={t("security.listsTitle")} desc={t("security.listsDesc")}>
            {([
              ["allow", "allow_hosts"],
              ["deny", "deny_hosts"],
              ["suppress", "suppress"],
            ] as const).map(([key, field]) => (
              <div key={key} className="field">
                <label htmlFor={`fw-${key}`}>{t(`security.list.${key}`)}</label>
                <input
                  id={`fw-${key}`}
                  className="input mono"
                  value={lists[key]}
                  placeholder={t(`security.listPlaceholder.${key}`)}
                  onChange={(e) => setLists((l) => ({ ...l, [key]: e.target.value }))}
                  onBlur={() => setOption({ [field]: toList(lists[key]) } as unknown as Partial<FirewallPolicy>)}
                  disabled={!inTauri}
                />
              </div>
            ))}
          </Section>
        </Card>
      )}

      <Card icon={<IconTerminal />} title={t("security.tryTitle")} desc={t("security.tryDesc")}>
        <div className="segmented sm" style={{ marginBottom: "var(--s8)" }}>
          {CHECK_KINDS.map((k) => (
            <button key={k} type="button" className={kind === k ? "active" : ""} onClick={() => setKind(k)}>
              {t(`security.checkKind.${k}`)}
            </button>
          ))}
        </div>
        <textarea
          className="input"
          rows={3}
          value={sample}
          placeholder={t("security.tryPlaceholder")}
          onChange={(e) => setSample(e.target.value)}
        />
        <div className="btn-row" style={{ marginTop: "var(--s8)" }}>
          <button type="button" className="btn" onClick={check} disabled={!inTauri || checking || !sample.trim()}>
            {t("security.tryRun")}
          </button>
          {!enforcing && <span className="muted">{t("security.tryAuditNote")}</span>}
        </div>
        {verdict && (
          <div className="fw-verdict">
            <span className={`pill ${verdict.decision === "block" ? "err" : verdict.decision === "warn" ? "warn" : "on"}`}>
              {t(`security.decision.${verdict.decision}`)}
            </span>
            {verdict.findings.length === 0
              ? <span className="muted">{t("security.noFindings")}</span>
              : <FindingList findings={verdict.findings} />}
          </div>
        )}
      </Card>

      <Card
        icon={<IconAlert />}
        title={t("security.eventsTitle")}
        desc={t("security.eventsDesc")}
        right={<span className="count-chip num">{events?.total ?? 0}</span>}
      >
        {loading ? (
          <SkeletonRows rows={2} />
        ) : !events?.events.length ? (
          <Empty icon={<IconShield />} title={t("security.eventsEmpty")} desc={t("security.eventsEmptyDesc")} />
        ) : (
          <div className="acct-list">
            {events.events.map((e, i) => (
              <div key={`${e.ts}-${i}`} className="acct">
                <div className="acct-head">
                  <div className="acct-id">
                    <div className="acct-name">
                      <span className={`pill ${e.decision === "block" ? "err" : e.decision === "warn" ? "warn" : "on"}`}>
                        {t(`security.decision.${e.decision}`)}
                      </span>
                      <span className="muted"> {e.agent} · {t(`security.eventKind.${e.kind}`, { defaultValue: e.kind })}</span>
                    </div>
                    <div className="muted num fw-scanner-desc">{fmtTime(e.ts)}</div>
                    <FindingList findings={e.findings} />
                  </div>
                </div>
              </div>
            ))}
          </div>
        )}
      </Card>
    </>
  );
}

/** Findings, as severity, which scanner caught it, and the rule id.
 *
 *  The id is what a suppression names, so it is shown rather than hidden behind
 *  the prose — and it is why the rule's own sentence stays in the tooltip
 *  rather than in the row: rule text is written once, in English, in the
 *  firewall's tables, and eighty translated sentences would go stale the first
 *  time a rule was added. The scanner name in front of it is translated, so a
 *  reader who does not want the English still learns what was caught. */
function FindingList({ findings }: { findings: FirewallVerdict["findings"] }) {
  const { t } = useTranslation();
  if (!findings.length) return null;
  return (
    <div className="fw-findings">
      {findings.slice(0, 6).map((f, i) => (
        <div key={`${f.rule}-${i}`} className="fw-finding" title={f.detail}>
          <span className={`pill tiny ${f.severity === "critical" ? "err" : f.severity === "high" ? "warn" : "plain"}`}>
            {t(`security.severity.${f.severity}`)}
          </span>
          <span>{t(`security.scanner.${f.scanner}.name`)}</span>
          <span className="mono muted">{f.rule}</span>
          {f.sample && <span className="mono muted">{f.sample}</span>}
        </div>
      ))}
      {findings.length > 6 && <span className="muted">{t("security.moreFindings", { n: findings.length - 6 })}</span>}
    </div>
  );
}

// API keys, from the machine that actually holds one.
//
// The web console can do everything here except the one thing that matters
// locally: writing a key into `~/.claude/settings.json`, `~/.codex/auth.json`
// and the rest. So this page carries two ideas the web page cannot —
//
//   * **in use** — which row the local proxy is holding right now, and
//   * **apply** — put that row's key into every CLI that is buying.
//
// Moving the account's default key is where the two meet. The default is what a
// fresh install picks up, so changing it while three CLIs are pointed at the
// old key leaves those three about to fail. The page asks before it rewrites
// anybody's config, and names the tools it would touch — a yes/no with no list
// is not a question anyone can answer.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, type ApiKeyListResp, type ApiKeyRow } from "../lib";
import {
  COMPAT,
  COMPAT_LANGS,
  COMPAT_LANG_LABELS,
  COMPAT_MODES,
  compatSample,
  type CompatLang,
  type CompatMode,
} from "@shared/api-compat";
import { GATEWAY_URL } from "../links";
import { Card, Empty, Err, Ok, PageHead, Section, Skeleton } from "../ui";
import {
  IconAlert, IconCheck, IconCopy, IconKey, IconPencil, IconRefresh, IconTrash, IconX,
} from "../icons";

/** Expiries offered at creation, and by the "extend" action. */
const TTL_CHOICES: (number | null)[] = [30, 90, 180, 365, null];

export function ApiKeys() {
  const { t, i18n } = useTranslation();
  const [data, setData] = useState<ApiKeyListResp | null>(null);
  const [err, setErr] = useState("");
  const [ok, setOk] = useState("");
  const [busy, setBusy] = useState(false);

  const [creating, setCreating] = useState(false);
  const [newLabel, setNewLabel] = useState("");
  const [newTtl, setNewTtl] = useState<number | null>(null);
  const [fresh, setFresh] = useState<{ id: number; key: string } | null>(null);
  const [shown, setShown] = useState<Record<number, string>>({});

  /** The "also re-key your tools?" question, parked until it is answered. */
  const [ask, setAsk] = useState<{ row: ApiKeyRow; kind: "default" | "apply" } | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await invoke<ApiKeyListResp>("list_api_keys"));
      setErr("");
    } catch (e) {
      setErr(String(e));
      setData((d) => d ?? { keys: [], max_keys: 50, buying_tools: [], has_local_key: false });
    }
  }, []);

  useEffect(() => {
    load();
  }, [load]);

  const run = useCallback(
    async (fn: () => Promise<unknown>, done?: string) => {
      setBusy(true);
      setErr("");
      setOk("");
      try {
        await fn();
        if (done) setOk(done);
        await load();
      } catch (e) {
        setErr(String(e));
      } finally {
        setBusy(false);
      }
    },
    [load],
  );

  const keys = data?.keys ?? [];
  const buying = data?.buying_tools ?? [];
  const def = keys.find((k) => k.is_default);
  const noDefault = !!data && keys.length > 0 && !def;
  const badDefault = !!def && !def.usable;
  const atMax = !!data && keys.length >= data.max_keys;

  const toolNames = buying.map((b) => b.label).join(t("common.listSep"));

  async function create() {
    setBusy(true);
    setErr("");
    try {
      const r = await invoke<{ id: number; key: string }>("create_api_key", {
        label: newLabel.trim(),
        expiresInDays: newTtl,
      });
      setFresh({ id: r.id, key: r.key });
      setCreating(false);
      setNewLabel("");
      setNewTtl(null);
      await load();
    } catch (e) {
      setErr(String(e));
    } finally {
      setBusy(false);
    }
  }

  async function reveal(row: ApiKeyRow) {
    if (shown[row.id]) {
      setShown((s) => {
        const next = { ...s };
        delete next[row.id];
        return next;
      });
      return;
    }
    try {
      const r = await invoke<{ key: string }>("reveal_api_key", { id: row.id });
      setShown((s) => ({ ...s, [row.id]: r.key }));
    } catch (e) {
      setErr(String(e));
    }
  }

  /** Set default / apply. Asks first when there is anything to break. */
  function intend(row: ApiKeyRow, kind: "default" | "apply") {
    if (buying.length > 0) {
      setAsk({ row, kind });
      return;
    }
    commit(row, kind, false);
  }

  function commit(row: ApiKeyRow, kind: "default" | "apply", apply: boolean) {
    setAsk(null);
    if (kind === "apply") {
      run(
        () => invoke("apply_api_key", { id: row.id }),
        t("apikeys.appliedTools", { tools: toolNames || t("apikeys.noTools") }),
      );
      return;
    }
    run(
      () => invoke("update_api_key", { id: row.id, setDefault: true, applyToTools: apply }),
      apply && buying.length
        ? t("apikeys.defaultMovedApplied", { tools: toolNames })
        : t("apikeys.defaultMoved"),
    );
  }

  const fmtDate = (iso: string) =>
    new Date(iso).toLocaleDateString(i18n.language, { year: "numeric", month: "short", day: "numeric" });

  return (
    <>
      <PageHead
        title={t("apikeys.title")}
        sub={t("apikeys.subtitle")}
        actions={
          <button className="btn ghost sm" onClick={load} disabled={busy}>
            <IconRefresh />
            {t("common.refresh")}
          </button>
        }
      />

      <Err>{err}</Err>
      <Ok>{ok}</Ok>

      {noDefault && (
        <p className="feedback err">
          <IconAlert />
          <span>{t("apikeys.noDefaultWarn")}</span>
        </p>
      )}
      {badDefault && (
        <p className="feedback err">
          <IconAlert />
          <span>{def.expired ? t("apikeys.defaultExpiredWarn") : t("apikeys.defaultDisabledWarn")}</span>
        </p>
      )}

      {/* Shown once, above everything: it is the only thing on this page that
          is gone if it is missed — though `reveal` can bring it back. */}
      {fresh && (
        <Card icon={<IconKey />} title={t("apikeys.createdTitle")} desc={t("apikeys.createdDesc")}>
          <KeyValue value={fresh.key} />
          <div className="btn-row" style={{ marginTop: "var(--s3)" }}>
            <button className="btn subtle sm" onClick={() => setFresh(null)}>
              <IconCheck />
              {t("apikeys.gotIt")}
            </button>
          </div>
        </Card>
      )}

      <Card
        icon={<IconKey />}
        title={t("apikeys.listTitle")}
        desc={t("apikeys.listDesc", { max: data?.max_keys ?? 50 })}
        right={
          !creating && (
            <button
              className="btn sm"
              disabled={busy || atMax}
              title={atMax ? t("apikeys.atMax", { max: data?.max_keys ?? 50 }) : undefined}
              onClick={() => setCreating(true)}
            >
              {t("apikeys.newKey")}
            </button>
          )
        }
      >
        {creating && (
          <Section title={t("apikeys.newKey")} first>
            <div className="btn-row" style={{ alignItems: "flex-end" }}>
              <div className="field" style={{ flex: 1, minWidth: 200, marginBottom: 0 }}>
                <label htmlFor="ak-name">{t("apikeys.name")}</label>
                <input
                  id="ak-name"
                  className="input"
                  value={newLabel}
                  maxLength={64}
                  autoFocus
                  placeholder={t("apikeys.namePlaceholder")}
                  onChange={(e) => setNewLabel(e.target.value)}
                  onKeyDown={(e) => e.key === "Enter" && create()}
                />
              </div>
              <div className="field" style={{ width: 170, marginBottom: 0 }}>
                <label htmlFor="ak-ttl">{t("apikeys.expiry")}</label>
                <select
                  id="ak-ttl"
                  className="input"
                  value={newTtl === null ? "never" : String(newTtl)}
                  onChange={(e) => setNewTtl(e.target.value === "never" ? null : Number(e.target.value))}
                >
                  {TTL_CHOICES.map((d) => (
                    <option key={String(d)} value={d === null ? "never" : String(d)}>
                      {d === null ? t("apikeys.expiryNever") : t("apikeys.expiryDays", { days: d })}
                    </option>
                  ))}
                </select>
              </div>
              <button className="btn sm" disabled={busy} onClick={create}>
                {t("apikeys.create")}
              </button>
              <button
                className="btn subtle sm"
                onClick={() => {
                  setCreating(false);
                  setNewLabel("");
                }}
              >
                {t("common.cancel")}
              </button>
            </div>
          </Section>
        )}

        {!data ? (
          <div style={{ display: "grid", gap: 10, marginTop: "var(--s3)" }}>
            <Skeleton h={34} />
            <Skeleton h={34} />
          </div>
        ) : keys.length === 0 ? (
          <Empty icon={<IconKey />} title={t("apikeys.empty")} desc={t("apikeys.emptyDesc")} />
        ) : (
          <div className="table-wrap">
            <table className="tbl">
              <thead>
                <tr>
                  <th>{t("apikeys.colName")}</th>
                  <th>{t("apikeys.colKey")}</th>
                  <th>{t("apikeys.colStatus")}</th>
                  <th>{t("apikeys.colExpiry")}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                {keys.map((k) => (
                  <Row
                    key={k.id}
                    row={k}
                    busy={busy}
                    plaintext={fresh?.id === k.id ? fresh.key : shown[k.id]}
                    fmtDate={fmtDate}
                    onReveal={() => reveal(k)}
                    onRename={(label) => run(() => invoke("update_api_key", { id: k.id, label }))}
                    onToggle={() => run(() => invoke("update_api_key", { id: k.id, enabled: !k.enabled }))}
                    onRenew={() => run(() => invoke("update_api_key", { id: k.id, expiresInDays: 90 }))}
                    onDefault={() => intend(k, "default")}
                    onApply={() => intend(k, "apply")}
                    onDelete={() => run(() => invoke("delete_api_key", { id: k.id }))}
                  />
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Card>

      {ask && (
        <ApplyDialog
          tools={toolNames}
          onCancel={() => setAsk(null)}
          onAnswer={(apply) => commit(ask.row, ask.kind, apply)}
          kind={ask.kind}
        />
      )}

      <CompatCard sampleKey={fresh?.key ?? Object.values(shown)[0]} />
    </>
  );
}

/* ── One key ─────────────────────────────────────────────────────────── */

function Row({
  row, busy, plaintext, fmtDate, onReveal, onRename, onToggle, onRenew, onDefault, onApply, onDelete,
}: {
  row: ApiKeyRow;
  busy: boolean;
  plaintext?: string;
  fmtDate: (iso: string) => string;
  onReveal: () => void;
  onRename: (label: string) => void;
  onToggle: () => void;
  onRenew: () => void;
  onDefault: () => void;
  onApply: () => void;
  onDelete: () => void;
}) {
  const { t } = useTranslation();
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(row.label);
  const [confirming, setConfirming] = useState(false);

  const commit = () => {
    setEditing(false);
    const next = draft.trim();
    if (next !== row.label) onRename(next);
  };

  return (
    <tr>
      <td>
        {editing ? (
          <input
            className="input"
            style={{ width: 170 }}
            value={draft}
            maxLength={64}
            autoFocus
            onChange={(e) => setDraft(e.target.value)}
            onBlur={commit}
            onKeyDown={(e) => {
              if (e.key === "Enter") commit();
              if (e.key === "Escape") {
                setDraft(row.label);
                setEditing(false);
              }
            }}
          />
        ) : (
          <span style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
            <strong>{row.label || t("apikeys.unnamed")}</strong>
            <button
              className="btn subtle sm"
              title={t("apikeys.rename")}
              onClick={() => {
                setDraft(row.label);
                setEditing(true);
              }}
            >
              <IconPencil />
            </button>
            {row.is_default && <span className="pill accent tiny">{t("apikeys.default")}</span>}
            {/* Two different claims: "the account points new installs here" and
                "this machine's CLIs are holding it right now". They are usually
                the same key and the interesting case is when they are not. */}
            {row.in_use && <span className="pill on tiny">{t("apikeys.inUse")}</span>}
          </span>
        )}
      </td>
      <td>{plaintext ? <KeyValue value={plaintext} /> : <span className="mono">{row.key_preview}</span>}</td>
      <td>
        <span className={`pill tiny ${row.usable ? "on" : "off"}`}>
          {row.expired ? t("apikeys.statusExpired") : row.enabled ? t("apikeys.statusActive") : t("apikeys.statusDisabled")}
        </span>
      </td>
      <td className="nowrap">
        {row.expires_at ? (
          <span className={row.expired ? "pill err tiny" : undefined}>{fmtDate(row.expires_at)}</span>
        ) : (
          <span className="faint">{t("apikeys.expiryNever")}</span>
        )}
      </td>
      <td>
        {confirming ? (
          <span className="btn-row nowrap">
            <span className="faint">{t("apikeys.confirmDelete")}</span>
            <button className="btn danger sm" disabled={busy} onClick={onDelete}>
              {t("apikeys.delete")}
            </button>
            <button className="btn subtle sm" onClick={() => setConfirming(false)}>
              <IconX />
            </button>
          </span>
        ) : (
          <span className="btn-row nowrap" style={{ justifyContent: "flex-end" }}>
            {!row.is_default && (
              <button
                className="btn subtle sm"
                disabled={busy || !row.usable}
                title={row.usable ? undefined : t("apikeys.setDefaultNeedsUsable")}
                onClick={onDefault}
              >
                {t("apikeys.setDefault")}
              </button>
            )}
            {!row.in_use && (
              <button
                className="btn subtle sm"
                disabled={busy || !row.usable || !row.revealable}
                title={t("apikeys.applyHint")}
                onClick={onApply}
              >
                {t("apikeys.apply")}
              </button>
            )}
            <button
              className="btn subtle sm"
              disabled={!row.revealable}
              title={row.revealable ? undefined : t("apikeys.notRevealable")}
              onClick={onReveal}
            >
              {plaintext ? t("apikeys.hide") : t("apikeys.reveal")}
            </button>
            {row.expired && (
              <button className="btn subtle sm" disabled={busy} onClick={onRenew}>
                {t("apikeys.renew")}
              </button>
            )}
            <button className="btn subtle sm" disabled={busy} onClick={onToggle}>
              {row.enabled ? t("apikeys.disable") : t("apikeys.enable")}
            </button>
            <button className="btn subtle sm" title={t("apikeys.delete")} onClick={() => setConfirming(true)}>
              <IconTrash />
            </button>
          </span>
        )}
      </td>
    </tr>
  );
}

/** The question asked before any tool config is rewritten. */
function ApplyDialog({
  tools, kind, onAnswer, onCancel,
}: {
  tools: string;
  kind: "default" | "apply";
  onAnswer: (apply: boolean) => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div
      className="modal-backdrop"
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) onCancel();
      }}
    >
      <div className="modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{kind === "default" ? t("apikeys.askDefaultTitle") : t("apikeys.askApplyTitle")}</h3>
          <button type="button" className="modal-x" onClick={onCancel}>
            <IconX />
          </button>
        </div>
        <div style={{ padding: "0 var(--s5) var(--s5)" }}>
          <p className="sub">
            {kind === "default" ? t("apikeys.askDefaultBody", { tools }) : t("apikeys.askApplyBody", { tools })}
          </p>
          <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
            <button className="btn" onClick={() => onAnswer(true)}>
              {t("apikeys.askApplyYes")}
            </button>
            {/* "Apply" has no meaningful no-branch: it is the action. Only the
                default move can sensibly be made without touching the tools. */}
            {kind === "default" && (
              <button className="btn ghost" onClick={() => onAnswer(false)}>
                {t("apikeys.askApplyNo")}
              </button>
            )}
            <button className="btn subtle" onClick={onCancel}>
              {t("common.cancel")}
            </button>
          </div>
        </div>
      </div>
    </div>
  );
}

/* ── Compatible APIs ─────────────────────────────────────────────────── */

function CompatCard({ sampleKey }: { sampleKey?: string }) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<CompatMode>("openai");
  const [lang, setLang] = useState<CompatLang>("curl");
  const spec = COMPAT[mode];
  const code = compatSample(mode, lang, { gateway: GATEWAY_URL, key: sampleKey });

  return (
    <Card title={t("apikeys.compatTitle")} desc={t("apikeys.compatDesc")}>
      <div className="segmented sm" style={{ marginBottom: "var(--s3)" }}>
        {COMPAT_MODES.map((m) => (
          <button key={m} className={m === mode ? "active" : ""} onClick={() => setMode(m)}>
            {COMPAT[m].name}
          </button>
        ))}
      </div>

      <div className="table-wrap">
        <table className="tbl">
          <tbody>
            <Fact k={t("apikeys.baseUrl")} v={spec.baseUrl(GATEWAY_URL)} />
            <Fact k={t("apikeys.authHeader")} v={spec.authHeader} />
            <Fact k={t("apikeys.envVar")} v={spec.envVar} />
            <Fact k={t("apikeys.paths")} v={spec.paths.join("\n")} pre />
          </tbody>
        </table>
      </div>

      <div className="segmented sm" style={{ margin: "var(--s4) 0 var(--s2)" }}>
        {COMPAT_LANGS.map((l) => (
          <button key={l} className={l === lang ? "active" : ""} onClick={() => setLang(l)}>
            {COMPAT_LANG_LABELS[l]}
          </button>
        ))}
      </div>
      <CodeBlock code={code} />
      <p className="faint" style={{ marginTop: "var(--s2)", fontSize: "var(--fs-meta)" }}>
        {sampleKey ? t("apikeys.sampleUsesRealKey") : t("apikeys.sampleUsesPlaceholder")}
      </p>
    </Card>
  );
}

function Fact({ k, v, pre }: { k: string; v: string; pre?: boolean }) {
  return (
    <tr>
      <td className="nowrap faint">{k}</td>
      <td>
        <span className="mono" style={pre ? { whiteSpace: "pre-line" } : { wordBreak: "break-all" }}>
          {v}
        </span>
      </td>
    </tr>
  );
}

function CodeBlock({ code }: { code: string }) {
  const [done, setDone] = useState(false);
  return (
    <pre className="codeblock notes">
      {code}
      <button
        className="code-copy"
        type="button"
        onClick={() =>
          navigator.clipboard?.writeText(code).then(
            () => {
              setDone(true);
              setTimeout(() => setDone(false), 1600);
            },
            () => {},
          )
        }
      >
        {done ? <IconCheck /> : <IconCopy />}
      </button>
    </pre>
  );
}

/** A plaintext key with a copy button. */
function KeyValue({ value }: { value: string }) {
  const { t } = useTranslation();
  const [done, setDone] = useState(false);
  return (
    <span className="copychip wrap">
      <span className="cc-val">{value}</span>
      <button
        type="button"
        className={`cc-btn${done ? " ok" : ""}`}
        onClick={() =>
          navigator.clipboard?.writeText(value).then(
            () => {
              setDone(true);
              setTimeout(() => setDone(false), 1600);
            },
            () => {},
          )
        }
      >
        {done ? <IconCheck /> : <IconCopy />}
        {done ? t("common.copied") : t("common.copy")}
      </button>
    </span>
  );
}

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
//
// The row itself follows the web console, so the two consoles are one product:
//
//   * The eye lives on the key, not in a menu — "which key is this one" is the
//     question that cell exists to answer.
//   * "Use here" stays inline, because it is the reason this page exists on a
//     desktop at all. Everything else folds into one overflow menu; a strip of
//     six identical ghost buttons made delete look as routine as rename.
//   * Colour appears only when something is wrong. Expired is the only red.

import { useCallback, useEffect, useRef, useState } from "react";
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
  IconAlert, IconCheck, IconCopy, IconDots, IconEye, IconEyeOff, IconInfo, IconKey,
  IconLock, IconPencil, IconPlus, IconRefresh, IconStar, IconTrash, IconX, IconZap,
} from "../icons";

/** Expiries offered at creation, and by the "extend" action. */
const TTL_CHOICES: (number | null)[] = [30, 90, 180, 365, null];

/** Inside this many days, the expiry cell starts saying so in warning ink. */
const EXPIRY_SOON_DAYS = 14;

/** Does this preview actually identify its key, or is it all bullets?
 *
 *  Keys minted before the server stored previews come back as `sk-asale-••••••••`
 *  — the plaintext is gone, so there is no tail to print. Read it off the string
 *  rather than off `revealable`: the daemon fills in a real preview for the key
 *  this machine is holding, and that row stays unrevealable. */
function previewHasTail(preview: string): boolean {
  return /[^•]/.test(preview.replace(/^sk-asale-/, ""));
}

/** Whole days from now until `iso`, rounded up. */
function daysLeft(iso: string): number {
  return Math.ceil((new Date(iso).getTime() - Date.now()) / 86_400_000);
}

export function ApiKeys() {
  const { t, i18n } = useTranslation();
  const [data, setData] = useState<ApiKeyListResp | null>(null);
  const [err, setErr] = useState("");
  const [ok, setOk] = useState("");
  const [busy, setBusy] = useState(false);

  const [creating, setCreating] = useState(false);
  const [newLabel, setNewLabel] = useState("");
  const [newTtl, setNewTtl] = useState<number | null>(null);
  const nameRef = useRef<HTMLInputElement>(null);
  const [fresh, setFresh] = useState<{ id: number; key: string } | null>(null);
  const [shown, setShown] = useState<Record<number, string>>({});

  /** Which row is being renamed. Hoisted out of the row because the rename is
   *  started from that row's overflow menu. */
  const [editingId, setEditingId] = useState<number | null>(null);

  /** The "also re-key your tools?" question, parked until it is answered. */
  const [ask, setAsk] = useState<{ row: ApiKeyRow; kind: "default" | "apply" } | null>(null);

  const load = useCallback(async () => {
    try {
      setData(await invoke<ApiKeyListResp>("list_api_keys"));
      setErr("");
    } catch (e) {
      setErr(String(e));
      setData((d) => d ?? {
        keys: [], max_keys: 10, unlimited: false,
        buying_tools: [], has_local_key: false, proxy_base: "",
      });
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
  const maxKeys = data?.max_keys ?? 10;
  /** Administrators are told the cap and not held to it. */
  const unlimited = !!data?.unlimited;
  const buying = data?.buying_tools ?? [];
  const def = keys.find((k) => k.is_default);
  const noDefault = !!data && keys.length > 0 && !def;
  const badDefault = !!def && !def.usable;
  const atMax = !!data && !unlimited && keys.length >= maxKeys;

  const toolNames = buying.map((b) => b.label).join(t("common.listSep"));

  /** Open the form — or, if it is already open, put the cursor back in it, so
   *  the header button never looks like it did nothing. */
  const openCreate = useCallback(() => {
    setCreating(true);
    requestAnimationFrame(() => nameRef.current?.focus());
  }, []);

  const closeCreate = useCallback(() => {
    setCreating(false);
    setNewLabel("");
    setNewTtl(null);
  }, []);

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

  /** Everything a row is and can do, built once per key. */
  const rowProps = (k: ApiKeyRow): RowProps => ({
    row: k,
    busy,
    plaintext: fresh?.id === k.id ? fresh.key : shown[k.id],
    editing: editingId === k.id,
    fmtDate,
    onEdit: () => setEditingId(k.id),
    onEditDone: () => setEditingId(null),
    onReveal: () => reveal(k),
    onRename: (label) => run(() => invoke("update_api_key", { id: k.id, label })),
    onToggle: () => run(() => invoke("update_api_key", { id: k.id, enabled: !k.enabled })),
    onRenew: () => run(() => invoke("update_api_key", { id: k.id, expiresInDays: 90 })),
    onDefault: () => intend(k, "default"),
    onApply: () => intend(k, "apply"),
    onDelete: () => run(() => invoke("delete_api_key", { id: k.id })),
  });

  return (
    <>
      <PageHead
        title={t("apikeys.title")}
        sub={t("apikeys.subtitle")}
        actions={
          <>
            <button className="btn ghost sm" onClick={load} disabled={busy}>
              <IconRefresh />
              {t("common.refresh")}
            </button>
            <button
              className="btn sm"
              disabled={busy || atMax}
              aria-expanded={creating}
              title={atMax ? t("apikeys.atMax", { max: maxKeys }) : undefined}
              onClick={openCreate}
            >
              <IconPlus />
              {t("apikeys.newKey")}
            </button>
          </>
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
          <div className="btn-row">
            <KeyValue value={fresh.key} />
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
        desc={unlimited ? t("apikeys.listDescUnlimited") : t("apikeys.listDesc", { max: maxKeys })}
        right={
          /* How much of the allowance is spent — a footnote to the list, and
             the only warning before "new key" stops working. */
          !!data && keys.length > 0 && !unlimited ? (
            <div className={`meter${atMax ? " near" : ""}`}>
              <div className="meter-head">
                <span>{t("apikeys.quota")}</span>
                <b>{t("apikeys.quotaUsed", { used: keys.length, max: maxKeys })}</b>
              </div>
              <div className="bar">
                <span style={{ width: `${Math.min(100, (keys.length / maxKeys) * 100)}%` }} />
              </div>
            </div>
          ) : undefined
        }
      >
        {creating && (
          <Section title={t("apikeys.newKey")} first>
            <CreateForm
              nameRef={nameRef}
              label={newLabel}
              ttl={newTtl}
              busy={busy}
              onLabel={setNewLabel}
              onTtl={setNewTtl}
              onSubmit={create}
              onCancel={closeCreate}
            />
          </Section>
        )}

        {!data ? (
          <div style={{ display: "grid", gap: 10, marginTop: "var(--s3)" }}>
            <Skeleton h={34} />
            <Skeleton h={34} />
          </div>
        ) : keys.length === 0 ? (
          <Empty
            icon={<IconKey />}
            title={t("apikeys.empty")}
            desc={t("apikeys.emptyDesc")}
            action={
              !creating && (
                <button className="btn sm" disabled={busy} onClick={openCreate}>
                  <IconPlus />
                  {t("apikeys.emptyCta")}
                </button>
              )
            }
          />
        ) : (
          /* No scroll box: it would clip the row menus, and the five columns
             fit the desktop window this app owns. */
          <table className="tbl">
            <thead>
              <tr>
                <th>{t("apikeys.colName")}</th>
                <th>{t("apikeys.colKey")}</th>
                <th>{t("apikeys.colStatus")}</th>
                <th>{t("apikeys.colExpiry")}</th>
                <th aria-label={t("apikeys.rowActions")} />
              </tr>
            </thead>
            <tbody>
              {keys.map((k) => (
                <Row key={k.id} {...rowProps(k)} />
              ))}
            </tbody>
          </table>
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

      <CompatCard
        sampleKey={fresh?.key ?? Object.values(shown)[0]}
        proxyBase={data?.proxy_base}
      />
    </>
  );
}

/* ── Creation ────────────────────────────────────────────────────────── */

function CreateForm({
  nameRef, label, ttl, busy, onLabel, onTtl, onSubmit, onCancel,
}: {
  nameRef: React.RefObject<HTMLInputElement>;
  label: string;
  ttl: number | null;
  busy: boolean;
  onLabel: (v: string) => void;
  onTtl: (v: number | null) => void;
  onSubmit: () => void;
  onCancel: () => void;
}) {
  const { t } = useTranslation();
  return (
    // The expiry is a row of chips rather than a `<select>`: there are five
    // choices, they all fit on one line, and a menu that has to be opened to
    // find out what is in it is a worse trade than that line of height.
    <div
      onKeyDown={(e) => {
        if (e.key === "Escape") onCancel();
      }}
    >
      <div className="grid2">
        <div className="field" style={{ marginBottom: 0 }}>
          <label htmlFor="ak-name">{t("apikeys.name")}</label>
          <input
            id="ak-name"
            ref={nameRef}
            className="input"
            value={label}
            maxLength={64}
            autoFocus
            placeholder={t("apikeys.namePlaceholder")}
            onChange={(e) => onLabel(e.target.value)}
            onKeyDown={(e) => e.key === "Enter" && onSubmit()}
          />
          <div className="hint">{t("apikeys.nameHint")}</div>
        </div>
        <div className="field" style={{ marginBottom: 0 }}>
          <label>{t("apikeys.expiry")}</label>
          <div className="band-presets" role="radiogroup" aria-label={t("apikeys.expiry")}>
            {TTL_CHOICES.map((d) => (
              <button
                key={String(d)}
                type="button"
                role="radio"
                aria-checked={ttl === d}
                className={`chip${ttl === d ? " on" : ""}`}
                onClick={() => onTtl(d)}
              >
                {d === null ? t("apikeys.expiryNever") : t("apikeys.expiryDays", { days: d })}
              </button>
            ))}
          </div>
          <div className="hint">{t("apikeys.expiryHint")}</div>
        </div>
      </div>

      <div className="btn-row" style={{ marginTop: "var(--s4)" }}>
        <button className="btn sm" disabled={busy} onClick={onSubmit}>
          {t("apikeys.create")}
        </button>
        <button className="btn subtle sm" onClick={onCancel}>
          {t("common.cancel")}
        </button>
      </div>
    </div>
  );
}

/* ── One key ─────────────────────────────────────────────────────────── */

type RowProps = {
  row: ApiKeyRow;
  busy: boolean;
  plaintext?: string;
  editing: boolean;
  fmtDate: (iso: string) => string;
  onEdit: () => void;
  onEditDone: () => void;
  onReveal: () => void;
  onRename: (label: string) => void;
  onToggle: () => void;
  onRenew: () => void;
  onDefault: () => void;
  onApply: () => void;
  onDelete: () => void;
};

function Row(p: RowProps) {
  const { t } = useTranslation();
  const { row } = p;
  return (
    <tr>
      <td>
        <KeyName {...p} />
      </td>
      <td>
        <KeyCell {...p} />
      </td>
      <td>
        <StatusPill row={row} />
      </td>
      <td className="nowrap">
        <Expiry row={row} fmtDate={p.fmtDate} />
      </td>
      <td>
        <span className="btn-row nowrap" style={{ justifyContent: "flex-end", flexWrap: "nowrap" }}>
          {/* The one action the web console cannot offer stays visible: it is
              the reason this page exists on a desktop. Rendered only when it
              would do something — a disabled "use here" on every expired and
              disabled row is four greyed buttons explaining nothing. */}
          {!row.in_use && row.usable && row.revealable && (
            <button
              className="btn subtle sm"
              disabled={p.busy}
              title={t("apikeys.applyHint")}
              onClick={p.onApply}
            >
              <IconZap />
              {t("apikeys.apply")}
            </button>
          )}
          <RowMenu {...p} menuLabel={t("apikeys.rowActions")} />
        </span>
      </td>
    </tr>
  );
}

/** The name, its badges, and the pencil that edits it in place. */
function KeyName({ row, editing, onEdit, onEditDone, onRename }: RowProps) {
  const { t } = useTranslation();

  // The editor is a separate component, mounted only while the edit is open,
  // so its draft is seeded by mounting rather than by an effect watching a flag.
  if (editing) {
    return (
      <NameEditor
        initial={row.label}
        onCancel={onEditDone}
        onCommit={(next) => {
          onEditDone();
          if (next !== row.label) onRename(next);
        }}
      />
    );
  }

  return (
    <span style={{ display: "flex", alignItems: "center", gap: 6, flexWrap: "wrap" }}>
      <strong className={row.label ? undefined : "faint"}>{row.label || t("apikeys.unnamed")}</strong>
      <button
        className="icon-btn sm onhover"
        title={t("apikeys.rename")}
        aria-label={t("apikeys.rename")}
        onClick={onEdit}
      >
        <IconPencil />
      </button>
      {/* Two different claims: "the account points new installs here" and
          "this machine's CLIs are holding it right now". They are usually the
          same key and the interesting case is when they are not. */}
      {row.is_default && (
        <span className="pill accent tiny" title={t("apikeys.defaultTip")}>
          {t("apikeys.default")}
        </span>
      )}
      {row.in_use && <span className="pill on tiny">{t("apikeys.inUse")}</span>}
    </span>
  );
}

function NameEditor({
  initial, onCommit, onCancel,
}: { initial: string; onCommit: (next: string) => void; onCancel: () => void }) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState(initial);
  // Enter and Escape both unmount this input, and an unmount that happens to
  // fire a blur first would otherwise turn a cancel into a save.
  const settled = useRef(false);
  const finish = (save: boolean) => {
    if (settled.current) return;
    settled.current = true;
    if (save) onCommit(draft.trim());
    else onCancel();
  };
  return (
    <input
      className="input"
      style={{ width: 170 }}
      value={draft}
      maxLength={64}
      autoFocus
      aria-label={t("apikeys.name")}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={() => finish(true)}
      onKeyDown={(e) => {
        if (e.key === "Enter") finish(true);
        if (e.key === "Escape") finish(false);
      }}
    />
  );
}

/** The key, masked until asked for — with the toggle attached to the value. */
function KeyCell({ row, plaintext, onReveal }: RowProps) {
  const { t } = useTranslation();

  if (plaintext) {
    return (
      <span className="keycell">
        <KeyValue value={plaintext} />
        <button
          className="icon-btn sm"
          aria-label={t("apikeys.hideKey")}
          title={t("apikeys.hideKey")}
          onClick={onReveal}
        >
          <IconEyeOff />
        </button>
      </span>
    );
  }

  // All bullets and no tail: say why, or it reads as a rendering bug.
  if (!previewHasTail(row.key_preview)) {
    return (
      <span className="keycell faint" title={t("apikeys.previewUnknown")}>
        <span className="mono">{row.key_preview}</span>
        <IconInfo width={12} height={12} />
      </span>
    );
  }

  return (
    <span className="keycell">
      <span className="mono">{row.key_preview}</span>
      <button
        className="icon-btn sm"
        disabled={!row.revealable}
        aria-label={t("apikeys.showKey")}
        title={row.revealable ? t("apikeys.showKey") : t("apikeys.notRevealable")}
        onClick={onReveal}
      >
        <IconEye />
      </button>
    </span>
  );
}

/** Active is a quiet pill; only a broken key gets colour. */
function StatusPill({ row }: { row: ApiKeyRow }) {
  const { t } = useTranslation();
  if (row.expired) return <span className="pill err tiny">{t("apikeys.statusExpired")}</span>;
  if (!row.enabled) return <span className="pill off tiny">{t("apikeys.statusDisabled")}</span>;
  return <span className="pill on tiny">{t("apikeys.statusActive")}</span>;
}

/** The date, plus how long is left once that starts to matter. */
function Expiry({ row, fmtDate }: { row: ApiKeyRow; fmtDate: (iso: string) => string }) {
  const { t } = useTranslation();
  if (!row.expires_at) return <span className="faint">{t("apikeys.expiryNever")}</span>;
  if (row.expired) return <span style={{ color: "var(--danger)" }}>{fmtDate(row.expires_at)}</span>;

  const days = daysLeft(row.expires_at);
  return (
    <>
      <div>{fmtDate(row.expires_at)}</div>
      {days <= EXPIRY_SOON_DAYS && (
        <div className="td-note" style={{ color: "var(--warning)" }}>
          {days <= 0 ? t("apikeys.expiresToday") : t("apikeys.expiresIn", { days })}
        </div>
      )}
    </>
  );
}

/** Everything else a key can be done to, behind one glyph — with the
 *  destructive one behind a second click inside it. */
function RowMenu({
  row, busy, menuLabel, onEdit, onToggle, onRenew, onDefault, onDelete,
}: RowProps & { menuLabel: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [confirming, setConfirming] = useState(false);
  const ref = useRef<HTMLDivElement>(null);

  /** Closing always drops the confirmation: reopening the menu on a delete
   *  that was walked away from must not land on the red button. */
  const close = useCallback(() => {
    setOpen(false);
    setConfirming(false);
  }, []);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) close();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") close();
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, close]);

  const pick = (fn: () => void) => () => {
    close();
    fn();
  };

  return (
    <div className="rowmenu" ref={ref}>
      <button
        className="icon-btn sm"
        aria-label={menuLabel}
        aria-haspopup="menu"
        aria-expanded={open}
        onClick={() => (open ? close() : setOpen(true))}
      >
        <IconDots />
      </button>

      {open && (
        <div className="menu" role="menu">
          {confirming ? (
            <div className="menu-confirm">
              <div className="mc-t">{t("apikeys.deleteTitle")}</div>
              <div className="mc-d">{t("apikeys.deleteDesc")}</div>
              <div className="mc-a">
                <button className="btn danger sm" disabled={busy} onClick={pick(onDelete)}>
                  <IconTrash />
                  {t("apikeys.delete")}
                </button>
                <button className="btn subtle sm" onClick={() => setConfirming(false)}>
                  {t("common.cancel")}
                </button>
              </div>
            </div>
          ) : (
            <>
              {!row.is_default && (
                <button
                  role="menuitem"
                  className="menu-item"
                  disabled={busy || !row.usable}
                  title={row.usable ? t("apikeys.defaultTip") : t("apikeys.setDefaultNeedsUsable")}
                  onClick={pick(onDefault)}
                >
                  <IconStar />
                  {t("apikeys.setDefault")}
                </button>
              )}
              <button role="menuitem" className="menu-item" onClick={pick(onEdit)}>
                <IconPencil />
                {t("apikeys.rename")}
              </button>
              {/* Renewing a key that never expires would give it an expiry —
                  the opposite of what the row currently promises. */}
              {row.expires_at && (
                <button role="menuitem" className="menu-item" disabled={busy} onClick={pick(onRenew)}>
                  <IconRefresh />
                  {t("apikeys.renew")}
                </button>
              )}
              <button role="menuitem" className="menu-item" disabled={busy} onClick={pick(onToggle)}>
                {row.enabled ? <IconLock /> : <IconCheck />}
                {row.enabled ? t("apikeys.disable") : t("apikeys.enable")}
              </button>
              <div className="menu-sep" />
              <button
                role="menuitem"
                className="menu-item danger"
                disabled={busy}
                onClick={() => setConfirming(true)}
              >
                <IconTrash />
                {t("apikeys.delete")}
              </button>
            </>
          )}
        </div>
      )}
    </div>
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

/** Which address the reader is being shown: the platform gateway, or the proxy
 *  this machine is already running. */
type Target = "cloud" | "local";

function CompatCard({ sampleKey, proxyBase }: { sampleKey?: string; proxyBase?: string }) {
  const { t } = useTranslation();
  const [mode, setMode] = useState<CompatMode>("openai");
  const [lang, setLang] = useState<CompatLang>("curl");
  // The platform address leads: it is the one that works from anywhere, and the
  // one somebody pasting into a CI job or a server needs. The local proxy is
  // offered second because on *this* machine it is the shorter path — it holds
  // the credential itself, and it is the only address that can be served out of
  // a subscription imported here.
  const [target, setTarget] = useState<Target>("cloud");

  const local = target === "local" && !!proxyBase;
  const base = local ? (proxyBase as string) : GATEWAY_URL;
  const spec = COMPAT[mode];
  const code = compatSample(mode, lang, { gateway: base, key: sampleKey });

  return (
    <Card title={t("apikeys.compatTitle")} desc={t("apikeys.compatDesc")}>
      {/* Dialect first: it decides every other value in this card. */}
      <div className="segmented sm" style={{ marginBottom: "var(--s3)" }}>
        {COMPAT_MODES.map((m) => (
          <button key={m} className={m === mode ? "active" : ""} onClick={() => setMode(m)}>
            {COMPAT[m].name}
          </button>
        ))}
      </div>

      <div className="section-title" style={{ fontSize: "var(--fs-sub)" }}>
        {t("apikeys.connection")}
      </div>
      {/* Two addresses answer the same three dialects, and which one to use is
          a property of where the code will run — so it is a switch over the
          block below rather than a second Base URL row nobody could choose
          between. Only offered once the daemon has said where its proxy is. */}
      {proxyBase && (
        <>
          <div className="segmented sm" style={{ marginBottom: "var(--s2)" }}>
            <button className={target === "cloud" ? "active" : ""} onClick={() => setTarget("cloud")}>
              {t("apikeys.targetCloud")}
            </button>
            <button className={target === "local" ? "active" : ""} onClick={() => setTarget("local")}>
              {t("apikeys.targetLocal")}
            </button>
          </div>
          <p className="faint" style={{ margin: "0 0 var(--s3)", fontSize: "var(--fs-meta)", lineHeight: 1.6 }}>
            {local ? t("apikeys.targetLocalHint") : t("apikeys.targetCloudHint")}
          </p>
        </>
      )}
      {/* Three of these four go straight into a config file, so all three carry
          their own copy button. The endpoint list does not — it is a reference
          for reading, not a value to paste. */}
      <table className="tbl">
        <tbody>
          <Fact k={t("apikeys.baseUrl")} v={spec.baseUrl(base)} copyable />
          <Fact k={t("apikeys.authHeader")} v={spec.authHeader} copyable />
          <Fact k={t("apikeys.envVar")} v={spec.envVar} copyable />
          <Fact k={t("apikeys.paths")} v={spec.paths.join("\n")} pre />
        </tbody>
      </table>

      <div className="section-title" style={{ fontSize: "var(--fs-sub)", margin: "var(--s5) 0 var(--s2)" }}>
        {t("apikeys.sample")}
      </div>
      <CodePanel code={code} lang={lang} onLang={setLang} />
      <p className="faint" style={{ marginTop: "var(--s2)", fontSize: "var(--fs-meta)" }}>
        {local
          ? t("apikeys.sampleUsesLocalProxy")
          : sampleKey
            ? t("apikeys.sampleUsesRealKey")
            : t("apikeys.sampleUsesPlaceholder")}
      </p>
    </Card>
  );
}

function Fact({ k, v, pre, copyable }: { k: string; v: string; pre?: boolean; copyable?: boolean }) {
  const { t } = useTranslation();
  const [done, setDone] = useState(false);
  return (
    <tr>
      <td className="nowrap faint">{k}</td>
      <td>
        <span style={{ display: "flex", alignItems: "flex-start", gap: "var(--s2)" }}>
          {/* A URL has no word boundaries worth keeping, so it breaks anywhere;
              the endpoint list does, and breaking `generateContent` mid-word
              made it unreadable. */}
          <span className="mono" style={pre ? { whiteSpace: "pre-line" } : { wordBreak: "break-all" }}>
            {v}
          </span>
          {copyable && (
            <button
              className="icon-btn sm onhover"
              title={t("common.copy")}
              aria-label={t("common.copy")}
              onClick={() =>
                navigator.clipboard?.writeText(v).then(
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
          )}
        </span>
      </td>
    </tr>
  );
}

/** The sample, with the language switch and the copy button attached to it. */
function CodePanel({
  code, lang, onLang,
}: { code: string; lang: CompatLang; onLang: (l: CompatLang) => void }) {
  const { t } = useTranslation();
  const [done, setDone] = useState(false);
  return (
    <div className="codepanel">
      <div className="codepanel-head">
        <div className="segmented sm">
          {COMPAT_LANGS.map((l) => (
            <button key={l} className={l === lang ? "active" : ""} onClick={() => onLang(l)}>
              {COMPAT_LANG_LABELS[l]}
            </button>
          ))}
        </div>
        <button
          className="btn subtle sm"
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
          {done ? t("common.copied") : t("common.copy")}
        </button>
      </div>
      <pre className="codeblock notes">{code}</pre>
    </div>
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

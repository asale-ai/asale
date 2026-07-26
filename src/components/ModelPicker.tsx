// Reusable model chooser.
//
// The market catalog can grow long, so picking a model is always done in a
// dialog with a search box and a vendor filter rather than an inline list.
// Two field widgets open that same dialog:
//   - `ModelMultiSelect`  — chips of the current picks + an "add model" button.
//   - `ModelSingleSelect` — a select-looking button holding one model.
// Any page that needs the user to choose from the catalog should use these
// instead of rolling its own list.
import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { IconSearch, IconCheck, IconX, IconPlus, IconChip } from "../icons";

export interface ModelOption {
  /** Value stored when this model is picked — the id the proxy routes on. */
  id: string;
  /** Display name; falls back to `id`. */
  label?: string;
  /** Vendor bucket for the filter dropdown (the catalog's `provider`). */
  vendor?: string;
  /** Secondary line under the name (price, context window, …). */
  meta?: string;
  /** Short right-hand tag, e.g. a discount. */
  tag?: string;
}

const UNKNOWN_VENDOR = "__other__";
const vendorOf = (o: ModelOption) => o.vendor || UNKNOWN_VENDOR;

/* ── The dialog ──────────────────────────────────────────────────────────── */

function ModelDialog({
  open,
  options,
  value,
  multiple,
  title,
  onClose,
  onApply,
}: {
  open: boolean;
  options: ModelOption[];
  value: string[];
  multiple: boolean;
  title?: string;
  onClose: () => void;
  onApply: (next: string[]) => void;
}) {
  const { t } = useTranslation();
  const [q, setQ] = useState("");
  const [vendor, setVendor] = useState("all");
  const [pickedOnly, setPickedOnly] = useState(false);
  const [draft, setDraft] = useState<string[]>(value);
  const searchRef = useRef<HTMLInputElement>(null);

  // Reseed every time the dialog opens — an edit made while it is open must not
  // be clobbered by the parent's `value` prop changing underneath it.
  useEffect(() => {
    if (!open) return;
    setDraft(value);
    setQ("");
    setVendor("all");
    setPickedOnly(false);
    const id = setTimeout(() => searchRef.current?.focus(), 30);
    return () => clearTimeout(id);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open]);

  // Escape closes; the page behind must not scroll while the dialog is up.
  useEffect(() => {
    if (!open) return;
    const onKey = (e: KeyboardEvent) => { if (e.key === "Escape") onClose(); };
    window.addEventListener("keydown", onKey);
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      window.removeEventListener("keydown", onKey);
      document.body.style.overflow = prev;
    };
  }, [open, onClose]);

  /** Vendors actually present, biggest first — the catalog spans dozens. */
  const vendors = useMemo(() => {
    const counts = new Map<string, number>();
    for (const o of options) counts.set(vendorOf(o), (counts.get(vendorOf(o)) ?? 0) + 1);
    return [...counts.entries()]
      .map(([id, n]) => ({ id, n, label: id === UNKNOWN_VENDOR ? t("modelPicker.vendorOther") : id }))
      .sort((a, b) => b.n - a.n || a.label.localeCompare(b.label));
  }, [options, t]);

  const list = useMemo(() => {
    // Every space-separated term must match, so "claude opus" narrows the way
    // people expect rather than needing the exact id order.
    const terms = q.trim().toLowerCase().split(/\s+/).filter(Boolean);
    return options.filter((o) => {
      if (vendor !== "all" && vendorOf(o) !== vendor) return false;
      if (pickedOnly && !draft.includes(o.id)) return false;
      const hay = `${o.id} ${o.label ?? ""} ${o.vendor ?? ""}`.toLowerCase();
      return terms.every((needle) => hay.includes(needle));
    });
  }, [options, q, vendor, pickedOnly, draft]);

  if (!open) return null;

  function pick(id: string) {
    if (!multiple) { onApply([id]); onClose(); return; }
    setDraft((d) => (d.includes(id) ? d.filter((x) => x !== id) : [...d, id]));
  }

  const body = (
    <div className="modal-backdrop" onMouseDown={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className="modal" role="dialog" aria-modal="true">
        <div className="modal-head">
          <h3>{title ?? t("modelPicker.title")}</h3>
          <button type="button" className="modal-x" onClick={onClose} title={t("modelPicker.cancel")}>
            <IconX />
          </button>
        </div>

        <div className="modal-tools">
          <div className="search-field">
            <IconSearch />
            <input
              ref={searchRef}
              className="input"
              value={q}
              onChange={(e) => setQ(e.target.value)}
              placeholder={t("modelPicker.search")}
              spellCheck={false}
            />
            {q && (
              <button type="button" className="search-x" onClick={() => setQ("")} title={t("modelPicker.clearSearch")}>
                <IconX />
              </button>
            )}
          </div>
          <div className="modal-filters">
            <select className="input" value={vendor} onChange={(e) => setVendor(e.target.value)}>
              <option value="all">{t("modelPicker.allVendors", { n: options.length })}</option>
              {vendors.map((v) => (
                <option key={v.id} value={v.id}>{v.label} ({v.n})</option>
              ))}
            </select>
            {multiple && draft.length > 0 && (
              <button
                type="button"
                className={`chip ${pickedOnly ? "on" : ""}`}
                onClick={() => setPickedOnly((v) => !v)}
              >
                {pickedOnly && <IconCheck />}
                {t("modelPicker.pickedOnly", { n: draft.length })}
              </button>
            )}
            <span className="modal-spacer" />
            <span className="modal-count">{t("modelPicker.shown", { n: list.length })}</span>
          </div>
        </div>

        <div className="modal-list">
          {list.length === 0 ? (
            <div className="opt-empty">
              <IconChip />
              <div>{options.length === 0 ? t("modelPicker.empty") : t("modelPicker.noMatch")}</div>
            </div>
          ) : (
            list.map((o) => {
              const on = draft.includes(o.id);
              return (
                <button key={o.id} type="button" className={`opt ${on ? "on" : ""}`} onClick={() => pick(o.id)}>
                  <span className={`opt-box ${multiple ? "" : "radio"} ${on ? "on" : ""}`}><IconCheck /></span>
                  <span className="opt-main">
                    <span className="opt-name" title={o.id}>{o.label ?? o.id}</span>
                    <span className="opt-meta mono">{o.id}{o.meta && ` · ${o.meta}`}</span>
                  </span>
                  {o.tag && <span className="opt-tag">{o.tag}</span>}
                </button>
              );
            })
          )}
        </div>

        <div className="modal-foot">
          {multiple ? (
            <>
              <button
                type="button"
                className="btn sm subtle"
                onClick={() => setDraft((d) => [...new Set([...d, ...list.map((o) => o.id)])])}
                disabled={list.length === 0}
              >
                {t("modelPicker.selectAll")}
              </button>
              <button type="button" className="btn sm subtle" onClick={() => setDraft([])} disabled={draft.length === 0}>
                {t("modelPicker.clear")}
              </button>
              <span className="modal-spacer" />
              <button type="button" className="btn sm ghost" onClick={onClose}>{t("modelPicker.cancel")}</button>
              <button type="button" className="btn sm" onClick={() => { onApply(draft); onClose(); }}>
                {t("modelPicker.confirm", { n: draft.length })}
              </button>
            </>
          ) : (
            <>
              <button
                type="button"
                className="btn sm subtle"
                onClick={() => { onApply([]); onClose(); }}
                disabled={draft.length === 0}
              >
                {t("modelPicker.clear")}
              </button>
              <span className="modal-spacer" />
              <button type="button" className="btn sm ghost" onClick={onClose}>{t("modelPicker.cancel")}</button>
            </>
          )}
        </div>
      </div>
    </div>
  );

  return createPortal(body, document.body);
}

/* ── Field widgets ───────────────────────────────────────────────────────── */

const labelOf = (options: ModelOption[], id: string) => options.find((o) => o.id === id)?.label ?? id;

/** Multi-select: the picks as removable chips, plus a button opening the dialog. */
export function ModelMultiSelect({
  options,
  value,
  onChange,
  disabled = false,
  title,
  addLabel,
}: {
  options: ModelOption[];
  value: string[];
  onChange: (next: string[]) => void;
  disabled?: boolean;
  title?: string;
  addLabel?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <div className="chips">
        {value.map((id) => (
          <span key={id} className="chip on" title={id}>
            {labelOf(options, id)}
            <button
              type="button"
              className="chip-x"
              onClick={() => onChange(value.filter((x) => x !== id))}
              disabled={disabled}
              title={t("modelPicker.remove")}
            >
              <IconX />
            </button>
          </span>
        ))}
        <button type="button" className="chip add" onClick={() => setOpen(true)} disabled={disabled}>
          <IconPlus />
          {addLabel ?? t("modelPicker.add")}
        </button>
      </div>
      <ModelDialog
        open={open}
        options={options}
        value={value}
        multiple
        title={title}
        onClose={() => setOpen(false)}
        onApply={onChange}
      />
    </>
  );
}

/** Single-select: a select-looking trigger holding at most one model. */
export function ModelSingleSelect({
  options,
  value,
  onChange,
  disabled = false,
  title,
  placeholder,
}: {
  options: ModelOption[];
  value: string | null;
  onChange: (next: string | null) => void;
  disabled?: boolean;
  title?: string;
  placeholder?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  return (
    <>
      <button type="button" className="model-select" onClick={() => setOpen(true)} disabled={disabled}>
        <span className={value ? "mono" : "muted"}>
          {value ? labelOf(options, value) : (placeholder ?? t("modelPicker.choose"))}
        </span>
        <IconSearch />
      </button>
      <ModelDialog
        open={open}
        options={options}
        value={value ? [value] : []}
        multiple={false}
        title={title}
        onClose={() => setOpen(false)}
        onApply={(next) => onChange(next[0] ?? null)}
      />
    </>
  );
}

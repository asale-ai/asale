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
import { groupModelsByFamily } from "@shared/model-groups";
import { IconSearch, IconCheck, IconX, IconPlus, IconChip, IconChevronDown } from "../icons";

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
  /**
   * Why this model cannot be picked right now (e.g. nobody is supplying it).
   * Set = the row is hidden while the "in supply" filter is on (the default),
   * and shown greyed with this label once the filter is off. A model that is
   * *already* picked is never hidden and stays clickable, so a pick made when
   * supply existed can always be seen and removed.
   */
  unavailable?: string;
}

const UNKNOWN_VENDOR = "__other__";
const vendorOf = (o: ModelOption) => o.vendor || UNKNOWN_VENDOR;
/** Vendor-qualified id, so two vendors' same-named models never share a family. */
const keyOf = (o: ModelOption) => `${o.vendor ?? ""}/${o.id}`;

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
  /** On by default: of ~350 priced models only a dozen have a seller at any
   *  moment, and a list that is 97% unpickable is a list nobody reads. */
  const [supplyOnly, setSupplyOnly] = useState(true);
  /** Families whose older versions the user asked to see, by family key. */
  const [expanded, setExpanded] = useState<string[]>([]);
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
    setSupplyOnly(true);
    setExpanded([]);
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
      // A pick already made is never filtered away, whatever the filters say:
      // it has to stay visible to be removable, and supply it once had can go
      // away between two openings of this dialog.
      const picked = draft.includes(o.id);
      if (vendor !== "all" && vendorOf(o) !== vendor) return false;
      if (pickedOnly && !picked) return false;
      if (supplyOnly && o.unavailable && !picked) return false;
      const hay = `${o.id} ${o.label ?? ""} ${o.vendor ?? ""}`.toLowerCase();
      return terms.every((needle) => hay.includes(needle));
    });
  }, [options, q, vendor, pickedOnly, supplyOnly, draft]);

  // Families are formed from what survived the filters, not from the whole
  // catalog: searching "opus 4.1" should surface 4.1 itself rather than hide it
  // under a 4.8 the search ruled out.
  const families = useMemo(() => groupModelsByFamily(list, keyOf), [list]);

  /**
   * The list exactly as rendered — the one place that decides which rows exist,
   * so the count in the toolbar and "select shown" cannot drift from the screen.
   *
   * A collapsed family shows its newest version plus any older one already
   * picked: a pick has to stay visible to be removable, even when the version
   * it names has been superseded.
   */
  const view = useMemo(
    () =>
      families.map((family) => {
        const unfolded = expanded.includes(family.key);
        const shown = unfolded
          ? family.all
          : [family.latest, ...family.older.filter((o) => draft.includes(o.id))];
        return { family, unfolded, shown, buried: family.all.length - shown.length };
      }),
    [families, expanded, draft],
  );
  const shownOptions = useMemo(() => view.flatMap((v) => v.shown), [view]);

  if (!open) return null;

  function pick(id: string) {
    if (!multiple) { onApply([id]); onClose(); return; }
    setDraft((d) => (d.includes(id) ? d.filter((x) => x !== id) : [...d, id]));
  }

  function toggleFamily(key: string) {
    setExpanded((e) => (e.includes(key) ? e.filter((x) => x !== key) : [...e, key]));
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
            <button
              type="button"
              className={`chip ${supplyOnly ? "on" : ""}`}
              onClick={() => setSupplyOnly((v) => !v)}
              title={t("modelPicker.supplyOnlyHint")}
            >
              {supplyOnly && <IconCheck />}
              {t("modelPicker.supplyOnly")}
            </button>
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
            <span className="modal-count">{t("modelPicker.shown", { n: shownOptions.length })}</span>
          </div>
        </div>

        <div className="modal-list">
          {view.length === 0 ? (
            <div className="opt-empty">
              <IconChip />
              <div>{options.length === 0 ? t("modelPicker.empty") : t("modelPicker.noMatch")}</div>
              {/* The filter that empties this list most often is the supply one,
                  and it is on before the user has touched anything — so say so
                  rather than let the catalog look empty. */}
              {options.length > 0 && supplyOnly && (
                <button type="button" className="btn sm subtle" onClick={() => setSupplyOnly(false)}>
                  {t("modelPicker.showUnavailable")}
                </button>
              )}
            </div>
          ) : (
            view.map(({ family, unfolded, shown, buried }) => (
              <div key={family.key} className="opt-family">
                {shown.map((o) => {
                  const on = draft.includes(o.id);
                  const blocked = !!o.unavailable && !on;
                  return (
                    <button
                      key={o.id}
                      type="button"
                      className={`opt ${on ? "on" : ""} ${blocked ? "off" : ""} ${o === family.latest ? "" : "sub"}`}
                      aria-disabled={blocked}
                      onClick={() => { if (!blocked) pick(o.id); }}
                    >
                      <span className={`opt-box ${multiple ? "" : "radio"} ${on ? "on" : ""}`}><IconCheck /></span>
                      <span className="opt-main">
                        <span className="opt-name" title={o.id}>{o.label ?? o.id}</span>
                        <span className="opt-meta mono">{o.id}{o.meta && ` · ${o.meta}`}</span>
                      </span>
                      {o.unavailable && <span className="opt-tag muted">{o.unavailable}</span>}
                      {o.tag && <span className="opt-tag">{o.tag}</span>}
                    </button>
                  );
                })}
                {/* A sibling of the rows, not a child: each row is a button. */}
                {(unfolded || buried > 0) && (
                  <button
                    type="button"
                    className={`opt-more ${unfolded ? "on" : ""}`}
                    aria-expanded={unfolded}
                    onClick={() => toggleFamily(family.key)}
                  >
                    <IconChevronDown />
                    {unfolded ? t("modelPicker.hideOlder") : t("modelPicker.showOlder", { n: buried })}
                  </button>
                )}
              </div>
            ))
          )}
        </div>

        <div className="modal-foot">
          {multiple ? (
            <>
              {/* "Select shown" means the rows on screen — a collapsed family's
                  older versions are not among them, so it never picks a model
                  the user has not seen. */}
              <button
                type="button"
                className="btn sm subtle"
                onClick={() =>
                  setDraft((d) => [
                    ...new Set([...d, ...shownOptions.filter((o) => !o.unavailable).map((o) => o.id)]),
                  ])
                }
                disabled={shownOptions.every((o) => !!o.unavailable)}
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

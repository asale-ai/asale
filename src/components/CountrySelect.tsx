// The country picker used at sign-up and in the profile centre.
//
// A native `<select>` over 250 options is a scroll, not a choice: the browser's
// type-ahead only matches from the first letter, and the row gives you a name
// with nothing to recognise it by. This is a combobox instead — flag, name and
// ISO code per row, with a search box that matches any part of either name or
// the code, so "jap", "日本" and "JP" all land on the same row.
//
// The panel renders through a portal, positioned against the trigger's screen
// rect: the profile centre keeps its picker inside a card that would clip a
// dropdown that was merely absolutely positioned.
//
// KEEP IN SYNC with asale-web/src/components/CountrySelect.tsx — same
// component, same `.cs-*` class names, different i18n and icon imports.
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useTranslation } from "react-i18next";
import { countryMatches, flagEmoji, type CountryOption } from "@shared/countries";
import { IconCheck, IconChevronDown, IconSearch } from "../icons";

/** Height the panel takes when there is room for it; the list scrolls inside. */
const PANEL_MAX_H = 340;
/** Below this much room, a side is too cramped to open into at all. */
const PANEL_MIN_H = 240;
const GAP = 6;

export function CountrySelect({
  value,
  onChange,
  countries,
  placeholder,
  disabled = false,
  autoFocus = false,
  invalid = false,
}: {
  /** ISO code, or "" for nothing chosen yet. */
  value: string;
  onChange: (code: string) => void;
  countries: CountryOption[];
  placeholder: string;
  disabled?: boolean;
  autoFocus?: boolean;
  /** Draws the trigger in the error state, for a form that submitted empty. */
  invalid?: boolean;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [q, setQ] = useState("");
  /** Row the keyboard is on. Kept as an index into `list`, reset on every
   *  keystroke, because the list it indexes into is rebuilt on every keystroke. */
  const [active, setActive] = useState(0);
  const triggerRef = useRef<HTMLButtonElement>(null);
  const panelRef = useRef<HTMLDivElement>(null);
  const searchRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const [rect, setRect] = useState<
    { top: number; left: number; width: number; above: boolean; maxH: number } | null
  >(null);

  const selected = useMemo(() => countries.find((c) => c.code === value), [countries, value]);
  const list = useMemo(() => countries.filter((c) => countryMatches(c, q)), [countries, q]);

  const place = useCallback(() => {
    const el = triggerRef.current;
    if (!el) return;
    const r = el.getBoundingClientRect();
    const below = window.innerHeight - r.bottom - GAP;
    const above = r.top - GAP;
    // Downwards is the default and it takes real cramping to give that up: the
    // trigger is the last field of a form, and a panel opening upwards covers
    // the fields the user just filled in. So flip only when below cannot hold a
    // panel worth reading, and then only if above can hold more.
    const flip = below < PANEL_MIN_H && above > below;
    setRect({
      top: flip ? r.top - GAP : r.bottom + GAP,
      left: r.left,
      width: r.width,
      above: flip,
      // Whatever the chosen side actually has. Without this the panel keeps its
      // full height and runs off the bottom of the window.
      maxH: Math.min(PANEL_MAX_H, Math.max(flip ? above : below, PANEL_MIN_H)),
    });
  }, []);

  // Before paint, so the panel never shows for a frame at the wrong place.
  useLayoutEffect(() => {
    if (open) place();
  }, [open, place]);

  useEffect(() => {
    if (!open) return;
    // `true` for the capture phase: the panel has to follow the trigger when
    // any scroll container between them moves, not just the window.
    const onScroll = () => place();
    window.addEventListener("scroll", onScroll, true);
    window.addEventListener("resize", onScroll);
    return () => {
      window.removeEventListener("scroll", onScroll, true);
      window.removeEventListener("resize", onScroll);
    };
  }, [open, place]);

  // Close on a click anywhere outside the pair of elements that make up the
  // control. `mousedown`, not `click`: a click that starts on the page and ends
  // on the panel should still close it.
  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (panelRef.current?.contains(target) || triggerRef.current?.contains(target)) return;
      setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    return () => document.removeEventListener("mousedown", onDown);
  }, [open]);

  // Reset in the handler rather than in an effect keyed on `open`: the state
  // this seeds is state the *act of opening* decides, and doing it in an effect
  // would render the panel once with the previous search still in it.
  function openPanel() {
    setQ("");
    // Start on the chosen country, so ↓ from a picker showing Japan goes to the
    // country after Japan rather than to Afghanistan.
    const at = countries.findIndex((c) => c.code === value);
    setActive(at < 0 ? 0 : at);
    setOpen(true);
  }

  // Focus once the panel exists to be focused. Not the input's own `autoFocus`,
  // which fires before the portal has been positioned.
  useEffect(() => {
    if (!open) return;
    const id = setTimeout(() => searchRef.current?.focus(), 20);
    return () => clearTimeout(id);
  }, [open]);

  // Keep the keyboard row on screen, by moving the list's own scrollTop and
  // nothing else. `scrollIntoView` would have done it in one line, but it also
  // scrolls every scrollable ancestor — including the page, which drags the
  // trigger out from under the panel anchored to it. Depends on `rect` too:
  // the panel does not exist to be scrolled until the first placement lands.
  useEffect(() => {
    const box = listRef.current;
    const row = box?.querySelector<HTMLElement>('[data-active="1"]');
    if (!box || !row) return;
    const top = row.offsetTop;
    const bottom = top + row.offsetHeight;
    if (top < box.scrollTop) box.scrollTop = top;
    else if (bottom > box.scrollTop + box.clientHeight) box.scrollTop = bottom - box.clientHeight;
  }, [open, rect, active, q]);

  function choose(code: string) {
    onChange(code);
    setOpen(false);
    triggerRef.current?.focus();
  }

  function onKey(e: React.KeyboardEvent) {
    if (e.key === "Escape") {
      e.preventDefault();
      setOpen(false);
      triggerRef.current?.focus();
      return;
    }
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (list.length === 0) return;
      const step = e.key === "ArrowDown" ? 1 : -1;
      setActive((i) => (i + step + list.length) % list.length);
      return;
    }
    if (e.key === "Home" || e.key === "End") {
      e.preventDefault();
      setActive(e.key === "Home" ? 0 : list.length - 1);
      return;
    }
    if (e.key === "Enter") {
      e.preventDefault(); // Inside a <form> this would submit it.
      const hit = list[active];
      if (hit) choose(hit.code);
    }
  }

  const panel = open && rect && (
    <div
      ref={panelRef}
      className="cs-panel"
      style={{
        top: rect.above ? undefined : rect.top,
        bottom: rect.above ? window.innerHeight - rect.top : undefined,
        left: rect.left,
        width: Math.max(rect.width, 260),
        maxHeight: rect.maxH,
      }}
      onKeyDown={onKey}
    >
      <div className="cs-search">
        <IconSearch />
        <input
          ref={searchRef}
          className="input"
          value={q}
          placeholder={t("countryPicker.search")}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            setQ(e.target.value);
            setActive(0);
          }}
        />
      </div>
      <div className="cs-list" ref={listRef} role="listbox" aria-label={placeholder}>
        {list.length === 0 ? (
          <div className="cs-empty">{t("countryPicker.noMatch")}</div>
        ) : (
          list.map((c, i) => (
            <div
              key={c.code}
              role="option"
              aria-selected={c.code === value}
              data-active={i === active ? "1" : undefined}
              className={`cs-opt${i === active ? " active" : ""}${c.code === value ? " on" : ""}`}
              // `mousedown`, not `click`: the search box has focus, and letting
              // the blur land first closes the panel out from under the click.
              onMouseDown={(e) => {
                e.preventDefault();
                choose(c.code);
              }}
              onMouseEnter={() => setActive(i)}
            >
              <span className="cs-flag" aria-hidden>{c.flag}</span>
              <span className="cs-name">{c.name}</span>
              <span className="cs-code">{c.code}</span>
              {c.code === value && <IconCheck />}
            </div>
          ))
        )}
      </div>
    </div>
  );

  return (
    <>
      <button
        ref={triggerRef}
        type="button"
        className={`cs-trigger${invalid ? " invalid" : ""}`}
        disabled={disabled}
        autoFocus={autoFocus}
        aria-haspopup="listbox"
        aria-expanded={open}
        onClick={() => (open ? setOpen(false) : openPanel())}
        onKeyDown={(e) => {
          if (!open && (e.key === "ArrowDown" || e.key === "Enter" || e.key === " ")) {
            e.preventDefault();
            openPanel();
          }
        }}
      >
        {selected ? (
          <>
            <span className="cs-flag" aria-hidden>{selected.flag || flagEmoji(selected.code)}</span>
            <span className="cs-name">{selected.name}</span>
            <span className="cs-code">{selected.code}</span>
          </>
        ) : (
          <span className="cs-placeholder">{placeholder}</span>
        )}
        <IconChevronDown />
      </button>
      {createPortal(panel, document.body)}
    </>
  );
}

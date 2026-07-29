// Small reusable UI primitives shared across pages.
import { useState, type ReactNode, type CSSProperties } from "react";
import { useTranslation } from "react-i18next";
import { IconCopy, IconCheck, IconAlert } from "./icons";

/* Copy-to-clipboard hook: returns [copied, copy]. */
export function useCopy(resetMs = 1600): [boolean, (text: string) => void] {
  const [copied, setCopied] = useState(false);
  const copy = (text: string) => {
    navigator.clipboard?.writeText(text).then(
      () => {
        setCopied(true);
        setTimeout(() => setCopied(false), resetMs);
      },
      () => {},
    );
  };
  return [copied, copy];
}

/* Inline value with a copy button. */
export function CopyChip({ value, wrap = false }: { value: string; wrap?: boolean }) {
  const { t } = useTranslation();
  const [copied, copy] = useCopy();
  return (
    <span className={`copychip${wrap ? " wrap" : ""}`}>
      <span className="cc-val">{value}</span>
      <button className={`cc-btn${copied ? " ok" : ""}`} onClick={() => copy(value)} type="button">
        {copied ? <IconCheck /> : <IconCopy />}
        {copied ? t("common.copied") : t("common.copy")}
      </button>
    </span>
  );
}

/* Multi-line code block with a copy button. */
export function CodeBlock({ text }: { text: string }) {
  const { t } = useTranslation();
  const [copied, copy] = useCopy();
  return (
    <pre className="codeblock">
      <button className="code-copy" onClick={() => copy(text)} type="button" title={t("common.copy")}>
        {copied ? <IconCheck /> : <IconCopy />}
        {copied ? t("common.copied") : t("common.copy")}
      </button>
      {text}
    </pre>
  );
}

/* Shimmering placeholder block shown while data is loading. */
export function Skeleton({
  w = "100%", h = 14, r = 8, style,
}: { w?: number | string; h?: number | string; r?: number; style?: CSSProperties }) {
  return <span className="skel" style={{ display: "block", width: w, height: h, borderRadius: r, ...style }} />;
}

/* A vertical stack of skeleton list rows (avatar + two text lines). */
export function SkeletonRows({ rows = 2 }: { rows?: number }) {
  return (
    <div className="list">
      {Array.from({ length: rows }, (_, i) => (
        <div key={i} className="list-row">
          <div className="lr-main">
            <Skeleton w={36} h={36} r={10} />
            <div style={{ flex: 1 }}>
              <Skeleton w="40%" h={13} style={{ marginBottom: 6 }} />
              <Skeleton w="60%" h={11} />
            </div>
          </div>
        </div>
      ))}
    </div>
  );
}

/* The whole content column as placeholders, shown while the app is still
   reaching its daemon and no page has been mounted yet.
   Deliberately shaped like the page that follows (heading, a row of stat
   tiles, two cards) so the swap to real content shifts nothing. */
export function PageSkeleton() {
  return (
    <div className="page-skeleton" aria-busy="true">
      <div className="page-head">
        <div className="ph-text" style={{ flex: 1 }}>
          <Skeleton w={168} h={26} r={9} style={{ marginBottom: 10 }} />
          <Skeleton w={260} h={13} />
        </div>
      </div>
      <div className="stat-grid">
        {[0, 1, 2].map((i) => (
          <div key={i} className="stat-tile">
            <Skeleton w={86} h={12} style={{ marginBottom: 14 }} />
            <Skeleton w={120} h={26} r={9} style={{ marginBottom: 10 }} />
            <Skeleton w={70} h={11} />
          </div>
        ))}
      </div>
      {[0, 1].map((i) => (
        <div key={i} className="card">
          <div className="card-head">
            <Skeleton w={132} h={15} />
          </div>
          <SkeletonRows rows={2} />
        </div>
      ))}
    </div>
  );
}

/* Identity mark for a provider / CLI (Claude, Codex, Gemini …).
   Deliberately monochrome and shared by every page: a screenful of vendor
   colours is the loudest thing on the screen and says nothing the label next
   to it does not already say. The tile it sits in carries the state instead. */
const MARK_LETTER: Record<string, string> = {
  claude: "C", claude_work: "C", codex: "O", gemini: "G",
  kimi: "K", kimi_api: "K", xai: "X", xai_api: "X",
};
export function Mark({ id, size = "md" }: { id: string; size?: "md" | "sm" }) {
  // Exact id first — some ids are prefixes of others (`kimi`/`kimi_api`).
  const key = id in MARK_LETTER ? id : Object.keys(MARK_LETTER).find((k) => id.startsWith(k));
  return (
    <span className={`mark ${size}`} aria-hidden>
      {key ? MARK_LETTER[key] : id.charAt(0).toUpperCase()}
    </span>
  );
}

/* A card with an optional icon + title header. */
export function Card({
  icon,
  title,
  desc,
  right,
  children,
  style,
  className = "",
}: {
  icon?: ReactNode;
  title?: string;
  desc?: string;
  right?: ReactNode;
  children: ReactNode;
  style?: CSSProperties;
  className?: string;
}) {
  return (
    <div className={`card ${className}`.trim()} style={style}>
      {(title || icon) && (
        <div className="card-head">
          {icon && <div className="card-ico">{icon}</div>}
          {title && <h3>{title}</h3>}
          {right}
        </div>
      )}
      {desc && <p className="card-desc">{desc}</p>}
      {children}
    </div>
  );
}

/* Every page opens the same way: title, one line of context, and — on the
   right — whatever acts on the whole page (refresh, rescan, sign out). Pages
   used to hand-roll this with inline flex styles and drifted apart; this is the
   one place that decides the rhythm. */
export function PageHead({
  title, sub, actions,
}: { title: string; sub?: string; actions?: ReactNode }) {
  return (
    <div className="page-head">
      <div className="ph-text">
        <h1>{title}</h1>
        {sub && <p className="sub">{sub}</p>}
      </div>
      {actions && <div className="ph-actions">{actions}</div>}
    </div>
  );
}

/* An icon-only header action (refresh and friends), with its label as tooltip
   so a row of them stays quiet. `spinning` drives the loading affordance. */
export function IconAction({
  icon, label, onClick, disabled, spinning,
}: { icon: ReactNode; label: string; onClick: () => void; disabled?: boolean; spinning?: boolean }) {
  return (
    <button
      className={`icon-btn${spinning ? " busy" : ""}`}
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
      type="button"
    >
      {icon}
    </button>
  );
}

/* A headline number. `primary` gets the brand wash — one per row at most,
   otherwise nothing stands out. */
export function StatTile({
  icon, label, value, sub, primary = false, loading = false,
}: {
  icon?: ReactNode; label: string; value: ReactNode; sub?: ReactNode;
  primary?: boolean; loading?: boolean;
}) {
  return (
    <div className={`stat-tile${primary ? " primary" : ""}`}>
      <div className="st-label">{icon}<span>{label}</span></div>
      <div className="st-value">{loading ? <Skeleton w={96} h={28} r={9} /> : value}</div>
      {sub !== undefined && <div className="st-sub">{loading ? <Skeleton w={64} h={12} /> : sub}</div>}
    </div>
  );
}

/* Key/value strip stating the rules or facts of whatever sits below it. */
export function FactGrid({ facts }: { facts: { k: string; v: ReactNode }[] }) {
  return (
    <div className="fact-grid">
      {facts.map((f) => (
        <div key={f.k} className="fact">
          <span className="fact-k">{f.k}</span>
          <span className="fact-v">{f.v}</span>
        </div>
      ))}
    </div>
  );
}

/* Nothing here — say what would be here, and offer the way to get it. */
export function Empty({
  icon, title, desc, action,
}: { icon: ReactNode; title: string; desc?: string; action?: ReactNode }) {
  return (
    <div className="empty">
      <div className="empty-ico">{icon}</div>
      <div className="empty-title">{title}</div>
      {desc && <div className="empty-desc">{desc}</div>}
      {action && <div className="empty-action">{action}</div>}
    </div>
  );
}

/* A labelled block inside a card — the level between "card" and "field". */
export function Section({
  title, desc, right, children, first = false,
}: { title: string; desc?: string; right?: ReactNode; children: ReactNode; first?: boolean }) {
  return (
    <div className={`section${first ? " first" : ""}`}>
      <div className="section-head">
        <h4>{title}</h4>
        {right}
      </div>
      {desc && <p className="section-desc">{desc}</p>}
      {children}
    </div>
  );
}

/* Inline error / success feedback with an icon. */
export function Err({ children }: { children: ReactNode }) {
  return children ? (
    <p className="feedback err">
      <IconAlert />
      <span>{children}</span>
    </p>
  ) : null;
}
export function Ok({ children }: { children: ReactNode }) {
  return children ? (
    <p className="feedback ok">
      <IconCheck />
      <span>{children}</span>
    </p>
  ) : null;
}

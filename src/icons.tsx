// Inline stroke-icon set (lucide-style). One <svg> wrapper, consistent 24-grid,
// currentColor stroke — sized via CSS (width/height on the parent or the svg).
import type { ReactNode, SVGProps } from "react";

type P = SVGProps<SVGSVGElement> & { size?: number };

function Svg({ size, className, style, children, ...rest }: P & { children: ReactNode }) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.8}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...rest}
      /* `.ico` is the floor, not the size: every rule that sizes icons by their
         container (`.btn svg`, `.navitem svg`, …) outranks it. It exists so an
         icon dropped somewhere without such a rule renders as an icon rather
         than at the SVG default of 300×150. */
      className={className ? `ico ${className}` : "ico"}
      style={size == null ? style : { width: size, height: size, ...style }}
    >
      {children}
    </svg>
  );
}

/* ── Navigation ──────────────────────────────────────────────────────────── */
export const IconDashboard = (p: P) => (
  <Svg {...p}><rect x="3" y="3" width="7" height="9" rx="1.5" /><rect x="14" y="3" width="7" height="5" rx="1.5" /><rect x="14" y="12" width="7" height="9" rx="1.5" /><rect x="3" y="16" width="7" height="5" rx="1.5" /></Svg>
);
/* Sell and buy are one pair, not two icons: the same tray, the arrow leaving it
   or arriving in it. They sit next to each other in the sidebar and on the
   dashboard, so the only thing that should differ between them is the direction
   — which is the only thing that differs between the two actions. */
export const IconPublish = (p: P) => (
  <Svg {...p}><path d="M12 15V3" /><path d="m7.5 7.5 4.5-4.5 4.5 4.5" /><path d="M4 16v2.5A2.5 2.5 0 0 0 6.5 21h11a2.5 2.5 0 0 0 2.5-2.5V16" /></Svg>
);
export const IconConsume = (p: P) => (
  <Svg {...p}><path d="M12 3v12" /><path d="m7.5 10.5 4.5 4.5 4.5-4.5" /><path d="M4 16v2.5A2.5 2.5 0 0 0 6.5 21h11a2.5 2.5 0 0 0 2.5-2.5V16" /></Svg>
);
export const IconWallet = (p: P) => (
  <Svg {...p}><path d="M3 7.5A2.5 2.5 0 0 1 5.5 5H18a1 1 0 0 1 1 1v1" /><path d="M3 7.5V17a2 2 0 0 0 2 2h14a1 1 0 0 0 1-1v-3" /><path d="M21 10h-4a2 2 0 0 0 0 4h4a1 1 0 0 0 1-1v-2a1 1 0 0 0-1-1Z" /></Svg>
);
/* The card rail's tab, beside the two wallets. A plain card outline with its
   magnetic stripe — the shape every payment sheet uses, so it needs no label to
   be read as "pay with a card". */
export const IconCard = (p: P) => (
  <Svg {...p}><rect x="2" y="5" width="20" height="14" rx="2.5" /><path d="M2 10h20" /><path d="M6 15h4" /></Svg>
);
export const IconRecords = (p: P) => (
  <Svg {...p}><path d="M6 2h8l4 4v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2Z" /><path d="M14 2v4a2 2 0 0 0 2 2h4" /><path d="M8 13h8M8 17h5" /></Svg>
);
export const IconUsage = (p: P) => (
  <Svg {...p}><path d="M3 3v18h18" /><rect x="7" y="12" width="3" height="5" rx="0.6" /><rect x="12" y="8" width="3" height="9" rx="0.6" /><rect x="17" y="4" width="3" height="13" rx="0.6" /></Svg>
);
export const IconGauge = (p: P) => (
  <Svg {...p}><path d="M12 14 8.5 8.5" /><path d="M3.5 16a9 9 0 1 1 17 0" /><circle cx="12" cy="14" r="1.4" fill="currentColor" stroke="none" /></Svg>
);
export const IconBell = (p: P) => (
  <Svg {...p}><path d="M10.3 21a1.94 1.94 0 0 0 3.4 0" /><path d="M6 8a6 6 0 0 1 12 0c0 7 3 9 3 9H3s3-2 3-9" /></Svg>
);
export const IconBellOff = (p: P) => (
  <Svg {...p}><path d="M8.7 3A6 6 0 0 1 18 8c0 3 .6 5 1.4 6.4M6 8a6 6 0 0 0-.4 8M10.3 21a1.94 1.94 0 0 0 3.4 0M3 3l18 18" /></Svg>
);
export const IconShare = (p: P) => (
  <Svg {...p}><path d="M4 12v7a1 1 0 0 0 1 1h14a1 1 0 0 0 1-1v-7" /><path d="M16 6l-4-4-4 4" /><path d="M12 2v13" /></Svg>
);
export const IconExpand = (p: P) => (
  <Svg {...p}><path d="M8 3H5a2 2 0 0 0-2 2v3M21 8V5a2 2 0 0 0-2-2h-3M16 21h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3" /></Svg>
);
export const IconAccount = (p: P) => (
  <Svg {...p}><circle cx="12" cy="8" r="4" /><path d="M4 21a8 8 0 0 1 16 0" /></Svg>
);
export const IconSettings = (p: P) => (
  <Svg {...p}><circle cx="12" cy="12" r="3" /><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.65 1.65 0 0 0 .33-1.82 1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.65 1.65 0 0 0 1.82.33H9a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.65 1.65 0 0 0-.33 1.82V9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1Z" /></Svg>
);

/* ── UI ──────────────────────────────────────────────────────────────────── */
export const IconCopy = (p: P) => (
  <Svg {...p}><rect x="9" y="9" width="12" height="12" rx="2" /><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1" /></Svg>
);
export const IconCheck = (p: P) => (<Svg {...p}><path d="m5 12 5 5L20 7" /></Svg>);
export const IconX = (p: P) => (<Svg {...p}><path d="M18 6 6 18M6 6l12 12" /></Svg>);
export const IconAlert = (p: P) => (<Svg {...p}><path d="M10.3 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.7 3.86a2 2 0 0 0-3.42 0Z" /><path d="M12 9v4M12 17h.01" /></Svg>);
export const IconInfo = (p: P) => (<Svg {...p}><circle cx="12" cy="12" r="9" /><path d="M12 16v-4M12 8h.01" /></Svg>);
export const IconRefresh = (p: P) => (<Svg {...p}><path d="M21 12a9 9 0 1 1-2.64-6.36" /><path d="M21 3v6h-6" /></Svg>);
export const IconPlus = (p: P) => (<Svg {...p}><path d="M12 5v14M5 12h14" /></Svg>);
export const IconPencil = (p: P) => (<Svg {...p}><path d="M4 20h4l10-10a2.5 2.5 0 0 0-3.5-3.5L4.5 16.5 4 20Z" /><path d="m13.5 7 3.5 3.5" /></Svg>);
export const IconTrash = (p: P) => (<Svg {...p}><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M6 6l1 14a2 2 0 0 0 2 2h6a2 2 0 0 0 2-2l1-14" /></Svg>);
export const IconExternal = (p: P) => (<Svg {...p}><path d="M15 3h6v6M10 14 21 3M18 13v6a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V8a2 2 0 0 1 2-2h6" /></Svg>);
/* Read a credential back, and hide it again. */
export const IconEye = (p: P) => (<Svg {...p}><path d="M2.5 12S6 5.5 12 5.5 21.5 12 21.5 12 18 18.5 12 18.5 2.5 12 2.5 12Z" /><circle cx="12" cy="12" r="3" /></Svg>);
export const IconEyeOff = (p: P) => (<Svg {...p}><path d="M4 4.5 20 20M9.9 5.9A9.6 9.6 0 0 1 12 5.5c6 0 9.5 6.5 9.5 6.5a17 17 0 0 1-3 3.9M6.5 8.1A17 17 0 0 0 2.5 12S6 18.5 12 18.5c.85 0 1.64-.13 2.37-.35" /><path d="M9.9 9.95a3 3 0 0 0 4.2 4.2" /></Svg>);
/* A row's overflow. Vertical, so it reads as "this row" rather than as a
   toolbar of its own. */
export const IconDots = (p: P) => (<Svg {...p}><circle cx="12" cy="5" r="1.2" fill="currentColor" stroke="none" /><circle cx="12" cy="12" r="1.2" fill="currentColor" stroke="none" /><circle cx="12" cy="19" r="1.2" fill="currentColor" stroke="none" /></Svg>);
export const IconClock = (p: P) => (<Svg {...p}><circle cx="12" cy="12" r="9" /><path d="M12 7v5.2l3.2 2" /></Svg>);
export const IconLock = (p: P) => (<Svg {...p}><rect x="4" y="10" width="16" height="11" rx="2" /><path d="M8 10V7a4 4 0 0 1 8 0v3" /></Svg>);
export const IconStar = (p: P) => (<Svg {...p}><path d="m12 3.5 2.6 5.35 5.9.83-4.25 4.15 1 5.87L12 16.9l-5.25 2.8 1-5.87L3.5 9.68l5.9-.83L12 3.5Z" /></Svg>);
export const IconKey = (p: P) => (<Svg {...p}><circle cx="7.5" cy="15.5" r="4.5" /><path d="m10.5 12.5 8-8M17 4l3 3M15 6l2 2" /></Svg>);
export const IconZap = (p: P) => (<Svg {...p}><path d="M13 2 4 14h7l-1 8 9-12h-7l1-8Z" /></Svg>);
export const IconShield = (p: P) => (<Svg {...p}><path d="M12 22s8-4 8-10V5l-8-3-8 3v7c0 6 8 10 8 10Z" /></Svg>);
export const IconPower = (p: P) => (<Svg {...p}><path d="M12 3v9M6.4 6.4a8 8 0 1 0 11.2 0" /></Svg>);
export const IconLoader = (p: P) => (<Svg {...p}><path d="M12 3v3M12 18v3M5.6 5.6l2.1 2.1M16.3 16.3l2.1 2.1M3 12h3M18 12h3M5.6 18.4l2.1-2.1M16.3 7.7l2.1-2.1" /></Svg>);
export const IconArrowLeft = (p: P) => (<Svg {...p}><path d="M19 12H5M12 19l-7-7 7-7" /></Svg>);
export const IconArrowRight = (p: P) => (<Svg {...p}><path d="M5 12h14M12 5l7 7-7 7" /></Svg>);
export const IconChevronDown = (p: P) => (<Svg {...p}><path d="m6 9 6 6 6-6" /></Svg>);
export const IconDownload = (p: P) => (<Svg {...p}><path d="M12 3v12M7 10l5 5 5-5M5 21h14" /></Svg>);
export const IconLink = (p: P) => (<Svg {...p}><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5" /><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5" /></Svg>);
export const IconSearch = (p: P) => (<Svg {...p}><circle cx="11" cy="11" r="7" /><path d="m21 21-4.3-4.3" /></Svg>);
export const IconRoute = (p: P) => (<Svg {...p}><circle cx="6" cy="19" r="2.5" /><circle cx="18" cy="5" r="2.5" /><path d="M8.5 19H14a4 4 0 0 0 0-8h-4a4 4 0 0 1 0-8h5.5" /></Svg>);
export const IconServer = (p: P) => (<Svg {...p}><rect x="3" y="4" width="18" height="7" rx="2" /><rect x="3" y="13" width="18" height="7" rx="2" /><path d="M7 7.5h.01M7 16.5h.01" /></Svg>);
export const IconTerminal = (p: P) => (<Svg {...p}><rect x="3" y="4" width="18" height="16" rx="2" /><path d="m7 9 3 3-3 3M13 15h4" /></Svg>);
export const IconWifi = (p: P) => (<Svg {...p}><path d="M5 12.5a10 10 0 0 1 14 0M8.5 16a5 5 0 0 1 7 0M12 19.5h.01" /></Svg>);
export const IconChip = (p: P) => (<Svg {...p}><rect x="6" y="6" width="12" height="12" rx="2" /><path d="M9 2v2M15 2v2M9 20v2M15 20v2M2 9h2M2 15h2M20 9h2M20 15h2" /></Svg>);
export const IconStore = (p: P) => (<Svg {...p}><path d="M4 9h16l-1-5H5L4 9Z" /><path d="M4 9v9a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V9M9 20v-6h6v6" /></Svg>);
export const IconMoon = (p: P) => (<Svg {...p}><path d="M21 12.8A9 9 0 1 1 11.2 3a7 7 0 0 0 9.8 9.8Z" /></Svg>);
export const IconSun = (p: P) => (<Svg {...p}><circle cx="12" cy="12" r="4" /><path d="M12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4" /></Svg>);
export const IconGlobe = (p: P) => (<Svg {...p}><circle cx="12" cy="12" r="9" /><path d="M3 12h18M12 3a14 14 0 0 1 0 18M12 3a14 14 0 0 0 0 18" /></Svg>);
export const IconRocket = (p: P) => (<Svg {...p}><path d="M5 14c-1.5 1.3-2 5-2 5s3.7-.5 5-2c.7-.9.7-2.2-.1-3-.8-.8-2.1-.8-2.9 0Z" /><path d="M9 12c0-4 2-8 9-9 1 7-3 9-7 9M9 12l3 3M14 8.5a1.5 1.5 0 1 0 3 0 1.5 1.5 0 0 0-3 0Z" /></Svg>);

/* The one mark in this file that is not drawn to the stroke grid above: it is
   GitHub's own logo, a solid shape, so it fills with currentColor instead. Kept
   here rather than in brand-marks.tsx because that file is the *vendor* table
   (the AI tools the product trades in), and GitHub is neither traded nor a
   subscription — it is where this client's source lives. */
export const IconGithub = ({ size, ...rest }: P) => (
  <svg viewBox="0 0 24 24" width={size} height={size} fill="currentColor" aria-hidden="true" {...rest}>
    <path d="M12 .3a12 12 0 0 0-3.79 23.39c.6.11.82-.26.82-.58v-2.03c-3.34.73-4.04-1.61-4.04-1.61-.55-1.39-1.33-1.76-1.33-1.76-1.09-.75.08-.73.08-.73 1.2.08 1.84 1.24 1.84 1.24 1.07 1.83 2.81 1.3 3.49 1 .11-.78.42-1.31.76-1.61-2.66-.3-5.47-1.33-5.47-5.93 0-1.31.47-2.38 1.24-3.22-.13-.3-.54-1.52.11-3.18 0 0 1.01-.32 3.3 1.23a11.5 11.5 0 0 1 6.01 0c2.29-1.55 3.3-1.23 3.3-1.23.65 1.66.24 2.88.12 3.18.77.84 1.23 1.91 1.23 3.22 0 4.61-2.81 5.62-5.49 5.92.43.37.82 1.1.82 2.22v3.29c0 .32.21.7.83.58A12 12 0 0 0 12 .3Z" />
  </svg>
);

/* Studio. A spark rather than a speech bubble: the tab is not "messages", it is
   the place where a model is put to work — chat, translation, drawing, notes. */
export const IconSparkle = (p: P) => (
  <Svg {...p}><path d="M12 3l1.9 5.1L19 10l-5.1 1.9L12 17l-1.9-5.1L5 10l5.1-1.9z" /><path d="M18 15.5l.8 2.2 2.2.8-2.2.8-.8 2.2-.8-2.2-2.2-.8 2.2-.8z" /></Svg>
);

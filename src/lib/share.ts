// Where a share can go, and the URL that takes it there.
//
// KEEP IN SYNC with asale-web/src/lib/share.ts. Duplicated for the same
// reason `brand-marks.tsx` is: this is runtime code, and `asale-client/shared/`
// only carries types.
//
// "Share" is not one mechanism, so a target is one of three kinds:
//
//   `intent` — the platform publishes a URL that opens its composer prefilled.
//              Most of the world works this way.
//   `qr`     — WeChat has no web composer at all. The only route from a desktop
//              browser into it is to get the link onto a phone, so the target
//              renders a code to scan instead of opening anything.
//   `manual` — Xiaohongshu is app-only and takes neither a composer URL nor a
//              web upload. The card image is saved and the caption copied, and
//              the user pastes both into the app.
//
// Note which fields each intent actually honours. Facebook and LinkedIn take
// the URL and nothing else — everything they display beside it is scraped from
// the page's Open Graph tags. That is what `opengraph-image.tsx` is for, and
// why a share to those two looks bare without it.

import { openExternal } from "../shell";

export type ShareKind = "intent" | "qr" | "manual";

/** Which half of the world a target is a first-class choice in. Only used to
 *  order the sheet — every target stays reachable in every locale, because
 *  "which platform do you actually use" does not follow the UI language. */
export type ShareScope = "cn" | "intl";

export interface SharePayload {
  /** The invite link. Every target receives it; several ignore the rest. */
  url: string;
  /** One line: the tweet, the Weibo body, the link title. */
  title: string;
  /** The longer caption. Targets with a single text field get `title`. */
  text: string;
}

export interface ShareTarget {
  id: string;
  kind: ShareKind;
  scope: ShareScope;
  /** Wash colour for the tile behind the mark. */
  tint: string;
  /** Composer URL. Absent on `qr` and `manual`, which have none. */
  href?: (p: SharePayload) => string;
}

const e = encodeURIComponent;

/** X and Threads are pure black in their brand systems, which is invisible as a
 *  wash on the dark theme and indistinguishable from a plain hover on the
 *  light one. They borrow X's own secondary grey so the tile still reads as
 *  tinted in both. */
const NEUTRAL_TINT = "#71767b";

export const SHARE_TARGETS: ShareTarget[] = [
  {
    id: "x",
    kind: "intent",
    scope: "intl",
    tint: NEUTRAL_TINT,
    href: (p) => `https://x.com/intent/post?text=${e(p.text)}&url=${e(p.url)}`,
  },
  {
    id: "facebook",
    kind: "intent",
    scope: "intl",
    tint: "#0866FF",
    href: (p) => `https://www.facebook.com/sharer/sharer.php?u=${e(p.url)}`,
  },
  {
    id: "linkedin",
    kind: "intent",
    scope: "intl",
    tint: "#0A66C2",
    href: (p) => `https://www.linkedin.com/sharing/share-offsite/?url=${e(p.url)}`,
  },
  {
    id: "reddit",
    kind: "intent",
    scope: "intl",
    tint: "#FF4500",
    href: (p) => `https://www.reddit.com/submit?url=${e(p.url)}&title=${e(p.title)}`,
  },
  {
    id: "telegram",
    kind: "intent",
    scope: "intl",
    tint: "#26A5E4",
    href: (p) => `https://t.me/share/url?url=${e(p.url)}&text=${e(p.text)}`,
  },
  {
    // One field only, and it is a plain message body — so the link is appended
    // rather than passed separately, or it would never appear.
    id: "whatsapp",
    kind: "intent",
    scope: "intl",
    tint: "#25D366",
    href: (p) => `https://api.whatsapp.com/send?text=${e(`${p.text}\n${p.url}`)}`,
  },
  {
    id: "threads",
    kind: "intent",
    scope: "intl",
    tint: NEUTRAL_TINT,
    href: (p) => `https://www.threads.net/intent/post?text=${e(`${p.text}\n${p.url}`)}`,
  },
  {
    // LINE carries the whole message in the *path*, not in a query parameter —
    // the encoded text goes after `text/`, and a `?text=` form silently opens
    // an empty composer.
    id: "line",
    kind: "intent",
    scope: "intl",
    tint: "#00C300",
    href: (p) => `https://line.me/R/msg/text/?${e(`${p.text}\n${p.url}`)}`,
  },
  {
    // `t` is the submission title, and HN truncates it hard — the headline
    // goes here, never the caption.
    id: "hackernews",
    kind: "intent",
    scope: "intl",
    tint: "#F0652F",
    href: (p) => `https://news.ycombinator.com/submitlink?u=${e(p.url)}&t=${e(p.title)}`,
  },
  { id: "wechat", kind: "qr", scope: "cn", tint: "#07C160" },
  {
    id: "weibo",
    kind: "intent",
    scope: "cn",
    tint: "#E6162D",
    href: (p) => `https://service.weibo.com/share/share.php?url=${e(p.url)}&title=${e(p.text)}`,
  },
  {
    id: "qq",
    kind: "intent",
    scope: "cn",
    tint: "#1EBAFC",
    href: (p) =>
      `https://connect.qq.com/widget/shareqq/index.html?url=${e(p.url)}&title=${e(p.title)}&desc=${e(p.text)}`,
  },
  {
    id: "qzone",
    kind: "intent",
    scope: "cn",
    tint: "#FECE00",
    href: (p) =>
      `https://sns.qzone.qq.com/cgi-bin/qzshare/cgi_qzshare_onekey?url=${e(p.url)}&title=${e(p.title)}&summary=${e(p.text)}`,
  },
  { id: "xiaohongshu", kind: "manual", scope: "cn", tint: "#FF2442" },
  {
    id: "douban",
    kind: "intent",
    scope: "cn",
    tint: "#2D963D",
    href: (p) => `https://www.douban.com/share/service?href=${e(p.url)}&name=${e(p.title)}&text=${e(p.text)}`,
  },
];

/**
 * Which targets lead, per locale.
 *
 * Not a `scope === "cn"` sort. Traditional Chinese is not WeChat territory —
 * Taiwan and Hong Kong run on LINE and Facebook — and Japanese is neither
 * bucket's default order either. So each locale names its own head and the
 * rest follow in declaration order.
 */
const LEAD: Record<string, string[]> = {
  zh: ["wechat", "weibo", "xiaohongshu", "qq", "qzone", "douban"],
  "zh-TW": ["line", "facebook", "threads", "x", "telegram"],
  ja: ["x", "line", "facebook", "threads", "telegram"],
  en: ["x", "reddit", "linkedin", "hackernews", "telegram", "facebook"],
};

/** Every target, ordered for this locale. */
export function shareTargets(locale: string): ShareTarget[] {
  const lead = LEAD[locale] ?? LEAD.en;
  const rank = (t: ShareTarget) => {
    const i = lead.indexOf(t.id);
    return i === -1 ? lead.length : i;
  };
  // Stable sort, so targets outside the lead keep declaration order.
  return [...SHARE_TARGETS].sort((a, b) => rank(a) - rank(b));
}

/**
 * Open a composer, in the OS browser rather than in the app.
 *
 * `openExternal` is not an optimisation here — it is the only correct move. The
 * desktop shell is a single webview with no chrome, so navigating it to X's
 * composer strands the user on a page with no way back to the app. In a plain
 * browser the same helper falls through to a `noopener` `window.open`, which
 * matters for its own reason: every one of these targets is a logged-in
 * session, and `window.opener` would hand it a live handle back to this page.
 *
 * Fire-and-forget. The shell resolves once the OS has accepted the URL, which
 * says nothing about whether the user went on to post, so there is nothing for
 * a caller to await.
 */
export function openComposer(href: string): void {
  void openExternal(href);
}

/** Whether the OS share sheet is reachable, and whether it will take the card
 *  image along. Desktop Chrome on Linux/Windows has `share` but rejects files;
 *  asking about the file is the only way to know. */
export function canNativeShare(file: File | null): boolean {
  if (typeof navigator === "undefined" || typeof navigator.share !== "function") return false;
  if (!file) return true;
  return typeof navigator.canShare === "function" && navigator.canShare({ files: [file] });
}

/**
 * Hand the payload to the OS share sheet.
 *
 * Returns false when the user dismissed it or the platform refused, so the
 * caller can leave the in-page sheet open rather than reporting a share that
 * never happened. A dismissal arrives as an `AbortError`, which is a normal
 * outcome and not worth surfacing as an error.
 */
export async function nativeShare(p: SharePayload, file: File | null): Promise<boolean> {
  try {
    const data: ShareData = { title: p.title, text: p.text, url: p.url };
    if (file && canNativeShare(file)) data.files = [file];
    await navigator.share(data);
    return true;
  } catch {
    return false;
  }
}

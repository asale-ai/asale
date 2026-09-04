// The two public addresses this client points at, in one place: they appear in
// the top bar of every page, and the site again on the upgrade banner.
//
// Both are opened with `openExternal` (shell.ts), never navigated to — the
// desktop webview has no way back.

import { DEFAULT_GATEWAY } from "@shared/api-compat";
import { LANGUAGES } from "./i18n";

export const SITE_URL = "https://asale.ai/";
export const REPO_URL = "https://github.com/asale-ai/asale";

/**
 * The compatible API gateway — where a key is presented, and **not** the REST
 * API the daemon talks to. Two hostnames, two services: this one takes
 * `sk-asale-…` keys and relays inference, that one takes session tokens.
 *
 * Shown on the API-keys page for code the user writes themselves, which is why
 * it is the platform address rather than the local proxy on 127.0.0.1: the
 * proxy exists to re-point installed CLIs, and it is not reachable from the CI
 * job or the server they are about to paste this into.
 */
export const GATEWAY_URL = (import.meta.env.VITE_ASALE_GATEWAY as string | undefined)?.replace(/\/+$/, "")
  || DEFAULT_GATEWAY;

/**
 * Where the Studio bundle is served from.
 *
 * Unlike everything else in this file, this one *is* navigated to — into an
 * iframe, not the main webview. Studio is a separate origin with a separate
 * store and no access to this app's daemon; what it gets is one API key, handed
 * over by `postMessage`. See `pages/Studio.tsx`.
 *
 * Overridable so a developer can point the tab at `pnpm dev` in `asale-studio`.
 */
export const STUDIO_URL = (import.meta.env.VITE_ASALE_STUDIO as string | undefined)?.replace(/\/+$/, "")
  || "https://studio.asale.ai";

/** Swarm's bundle, framed the same way and told the same thing. */
export const SWARM_URL = (import.meta.env.VITE_ASALE_SWARM as string | undefined)?.replace(/\/+$/, "")
  || "https://swarm.asale.ai";

/**
 * AEO — opened in the browser, not framed.
 *
 * It is a server with its own session rather than a bundle holding a key, and
 * it signs in by navigating itself to asale.ai's consent page, which allows
 * being framed by the website and by nothing else. In here that navigation
 * would land on a blank frame, so the card hands it to `openExternal` like
 * every other link in this file.
 */
export const AEO_URL = (import.meta.env.VITE_ASALE_AEO as string | undefined)?.replace(/\/+$/, "")
  || "https://aeo.asale.ai";

/**
 * A page on the site, in the reader's language.
 *
 * The web app prefixes every route with a locale and ships the same four ids
 * this client does (`LANGUAGES`), so the current language maps straight
 * through — a link followed from a Chinese UI does not land on an English
 * page. Anything else falls back to English rather than producing a 404.
 */
export function sitePage(path: string, lang: string): string {
  const locale = LANGUAGES.some((l) => l.id === lang) ? lang : "en";
  return `${SITE_URL}${locale}/${path}`;
}

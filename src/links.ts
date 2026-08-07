// The two public addresses this client points at, in one place: they appear in
// the top bar of every page, and the site again on the upgrade banner.
//
// Both are opened with `openExternal` (shell.ts), never navigated to — the
// desktop webview has no way back.

import { LANGUAGES } from "./i18n";

export const SITE_URL = "https://asale.ai/";
export const REPO_URL = "https://github.com/asale-ai/asale";

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

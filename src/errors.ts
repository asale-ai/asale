// Turning a thrown value into a sentence the user can read, in their language.
//
// The daemon (and, through it, the asale server) cannot localize its own
// failures: one binary serves the desktop shell, a local browser and a remote
// one, and the chosen locale lives here. So failures travel as a catalog key
// plus interpolation values, and this resolves them against the same `errors.*`
// section both frontends ship.

import i18n from "./i18n";
import { DaemonError } from "./lib";

/**
 * Best available text for `e`, in the current language.
 *
 * Falls back in order, so a message is never blank or a stack trace:
 *   1. the translated `key`, when the catalog has it;
 *   2. the daemon's English `message` — untranslated but readable, which is
 *      what a key added ahead of its translations looks like;
 *   3. a generic "request failed", for throwables that carry no sentence.
 */
export function errText(e: unknown): string {
  if (e instanceof DaemonError) {
    if (e.key && i18n.exists(e.key)) return i18n.t(e.key, e.params);
    if (e.message) return e.message;
  }
  const msg = e instanceof Error ? e.message : String(e ?? "");
  return msg || i18n.t("common.requestFailed");
}

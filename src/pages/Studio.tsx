// Studio, in the desktop app.
//
// The same bundle the website frames, in a webview tab, with the same
// handshake: Studio is a separate origin with a separate store, so it never
// sees this app's daemon token — it gets one API key and the two public base
// URLs, and does its own talking to the platform from there.
//
// Which key: the one this machine is already buying through, if there is one.
// A person who has set up their CLIs and then opens Studio expects the chat to
// bill the same way the CLIs do, and minting a second key for it would split
// one machine's spending across two rows on the usage page.

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, type ApiKeyListResp, type ApiKeyRow } from "../lib";
import { STUDIO_URL } from "../links";
import { Err, Empty } from "../ui";
import { IconSparkle } from "../icons";
import { errText } from "../errors";
import { useTheme } from "../theme";

const STUDIO_ORIGIN = (() => {
  try {
    return new URL(STUDIO_URL).origin;
  } catch {
    return "";
  }
})();

/** Label on a key this page mints when the account has none it can read. */
const STUDIO_LABEL = "Studio";

const usable = (k: ApiKeyRow) => k.usable && k.revealable;

/** The stack this daemon is on.
 *
 *  Asked, not compiled in. The daemon already decides which server and gateway
 *  this client talks to (`core/src/config.rs`, overridden by `dev:app`), and a
 *  second copy in the frontend would be a copy that is right until the day
 *  somebody runs a dev build — at which point Studio would spend a
 *  locally-minted key against the live market, or fail to, confusingly. */
async function resolveStack(): Promise<{ api: string; gateway: string }> {
  const cfg = await invoke<{ server_api_base: string; gateway_api_base: string }>("client_config");
  return { api: cfg.server_api_base, gateway: cfg.gateway_api_base };
}

/** The key Studio should spend, in the order a person would expect:
 *  the one this machine already holds, then the account default, then any other
 *  readable one, then a new one. */
async function resolveKey(): Promise<string> {
  const { keys } = await invoke<ApiKeyListResp>("list_api_keys");
  const row =
    keys.find((k) => k.held && usable(k)) ??
    keys.find((k) => k.label === STUDIO_LABEL && usable(k)) ??
    keys.find((k) => k.is_default && usable(k)) ??
    keys.find(usable);

  if (row) {
    const revealed = await invoke<{ key: string }>("reveal_api_key", { id: row.id });
    if (revealed.key) return revealed.key;
  }

  // Keys minted before the server kept a sealed copy cannot be read back at
  // all, so an account holding only those needs one made rather than a frame
  // that never connects.
  const made = await invoke<{ id: number; key: string }>("create_api_key", {
    label: STUDIO_LABEL,
    expiresInDays: null,
    maxRatioPct: 100,
  });
  return made.key;
}

export function Studio() {
  const { t, i18n } = useTranslation();
  const frame = useRef<HTMLIFrameElement>(null);
  const key = useRef("");
  const stack = useRef<{ api: string; gateway: string } | null>(null);
  const [err, setErr] = useState("");

  // Subscribed, not read once: the frame has to follow a theme change in the
  // app around it, and `dataset.theme` on its own never re-renders anything.
  // "system" is resolved here rather than passed through — studio would resolve
  // it against the *webview's* media query, which is the same answer today and
  // one more thing to keep in step for no reason.
  const [pref] = useTheme();
  const theme =
    pref === "system"
      ? document.documentElement.dataset.theme === "light" ? "light" : "dark"
      : pref;

  const post = useCallback(() => {
    const win = frame.current?.contentWindow;
    if (!win || !key.current || !stack.current || !STUDIO_ORIGIN) return;
    // The exact origin, never "*": an API key is a bearer credential and a
    // wildcard would hand it to whatever document occupies the frame.
    win.postMessage(
      {
        source: "asale-host",
        type: "credentials",
        key: key.current,
        api: stack.current.api,
        gateway: stack.current.gateway,
        locale: i18n.language,
        theme,
      },
      STUDIO_ORIGIN,
    );
  }, [i18n.language, theme]);

  useEffect(() => {
    let alive = true;
    Promise.all([resolveStack(), resolveKey()])
      .then(([s, k]) => {
        if (!alive) return;
        stack.current = s;
        key.current = k;
        post();
      })
      .catch((e) => {
        if (alive) setErr(errText(e));
      });
    return () => { alive = false; };
  }, [post]);

  /* Studio announces itself on every load — a reload inside the frame, a
     restored window — so the key is re-sent each time rather than once. */
  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      if (e.origin !== STUDIO_ORIGIN) return;
      if ((e.data as { source?: string })?.source !== "asale-studio") return;
      if ((e.data as { type?: string }).type === "ready") post();
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [post]);

  /* Language and theme follow the app they are framed by. */
  useEffect(post, [post]);

  if (!STUDIO_ORIGIN) {
    return <Empty icon={<IconSparkle />} title={t("studio.unavailable")} />;
  }

  return (
    <div className="studio-page">
      {/* No page heading: Studio draws its own rail, its own title and its own
          sections, and a second heading above the frame would be the app
          labelling a window that has already labelled itself. */}
      {err ? (
        <Err>{err}</Err>
      ) : (
        <iframe
          ref={frame}
          title={t("nav.studio")}
          src={STUDIO_URL}
          className="studio-frame"
          // Mounted straight away rather than held back until the key resolves.
          // Two things were racing anyway — the RPC and the frame's own boot —
          // and hiding the frame meant they ran in series instead of together,
          // for a spinner nobody needed: Studio draws its own skeleton while it
          // waits for the credential this page is fetching.
          //
          // First-party code on a sibling origin: it gets what it needs to run
          // and nothing that would let it navigate the shell around it.
          sandbox="allow-scripts allow-same-origin allow-forms allow-downloads allow-popups allow-modals"
          allow="clipboard-write"
        />
      )}
    </div>
  );
}

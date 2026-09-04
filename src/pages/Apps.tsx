// The applications, in the desktop app: a card each, and a frame when one is
// open.
//
// Studio and Swarm are the same static bundles the website frames, in a webview
// tab, and they sign in the same way they do there — over OAuth, with this
// shell doing the one leg a framed app cannot do for itself:
//
//   app   → here  { type: "ready", oauth: { state, code_challenge } }
//   here  → asale POST /api/v1/oauth/authorize  (daemon, with this device's session)
//   here  → app   { type: "oauth_code", code, state }
//
// It used to be an API key: this page read one off the account, posted it
// across, and the frame spent it. The code that replaces it is bound to a PKCE
// challenge whose verifier never left the frame, so it is redeemable by the
// frame and by nobody else — this app included. See `daemon/commands/app_auth.rs`
// and, for the identical half on the website, `asale-web/src/components/AppFrame.tsx`.
//
// AEO is framed too, but it needs nothing from this page: it is a server with
// its own session, and it signs in by navigating its own frame to asale.ai's
// consent page and back. What that costs is spelled out where it is paid — the
// consent page and AEO both had to add this shell to their `frame-ancestors`,
// and AEO's session cookie had to become `SameSite=None; Partitioned`, because
// a shell serving `tauri://localhost` is a cross-site parent for both.

import { useCallback, useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke } from "../lib";
import { AEO_URL, STUDIO_URL, SWARM_URL } from "../links";
import { Err, Empty, PageHead } from "../ui";
import { IconChat, IconEye, IconSparkle, IconUsers } from "../icons";
import { errText } from "../errors";
import { useTheme } from "../theme";

type AppId = "studio" | "swarm" | "aeo";

const APPS: Array<{ id: AppId; url: string; Icon: (p: { size?: number }) => JSX.Element }> = [
  { id: "studio", url: STUDIO_URL, Icon: IconChat },
  { id: "swarm", url: SWARM_URL, Icon: IconUsers },
  { id: "aeo", url: AEO_URL, Icon: IconEye },
];

/** What each framed app is registered as (`oauth_clients`). Pinned per app: the
 *  shell decides *which* of our apps its session may authorize, never the page
 *  that asks. */
const CLIENT_ID: Record<AppId, string> = {
  studio: "asale-studio",
  swarm: "asale-swarm",
  aeo: "asale-aeo",
};

/** The website's own origin — where the consent page lives. It is not the
 *  framed app's origin, which is why it needs a test of its own. Loopback is
 *  `pnpm dev`'s web server. */
const consentOrigin = (origin: string) =>
  origin === "https://asale.ai" ||
  origin === "https://www.asale.ai" ||
  /^https?:\/\/(localhost|127\.0\.0\.1)(:\d+)?$/.test(origin);

const originOf = (url: string) => {
  try {
    return new URL(url).origin;
  } catch {
    return "";
  }
};

/** The authorization request a framed app hands over. Public by construction —
 *  a hash and a nonce — which is why it can travel over `postMessage` at all. */
interface AuthRequest {
  state: string;
  code_challenge: string;
  code_challenge_method: string;
}

export function Apps({ open, onOpen }: { open: AppId | null; onOpen: (id: AppId | null) => void }) {
  const { t } = useTranslation();
  const app = APPS.find((a) => a.id === open);

  if (app) return <AppFrame id={app.id} url={app.url} />;

  return (
    <>
      <PageHead title={t("apps.title")} sub={t("apps.sub")} />
      <div className="app-cards">
        {APPS.map(({ id, Icon }) => (
          <button key={id} type="button" className="card app-card" onClick={() => onOpen(id)}>
            <span className="app-card-ico"><Icon size={20} /></span>
            <span className="app-card-name">{t(`apps.${id}`)}</span>
            <span className="app-card-sub">{t(`apps.${id}Sub`)}</span>
            <span className="app-card-go">{t("apps.open")}</span>
          </button>
        ))}
      </div>
    </>
  );
}

function AppFrame({ id, url }: { id: AppId; url: string }) {
  const { t, i18n } = useTranslation();
  const frame = useRef<HTMLIFrameElement>(null);
  const [err, setErr] = useState("");
  const origin = originOf(url);
  // The app's own root: what is registered as its redirect, and what a static
  // bundle with no router can be sure resolves.
  const redirect = origin ? `${origin}/` : "";

  // Subscribed, not read once: the frame has to follow a theme change in the
  // app around it, and `dataset.theme` on its own never re-renders anything.
  // "system" is resolved here rather than passed through — the frame would
  // resolve it against the *webview's* media query, which is the same answer
  // today and one more thing to keep in step for no reason.
  const [pref] = useTheme();
  const theme =
    pref === "system"
      ? document.documentElement.dataset.theme === "light" ? "light" : "dark"
      : pref;

  const post = useCallback(
    (payload: Record<string, unknown>) => {
      const win = frame.current?.contentWindow;
      if (!win || !origin) return;
      // The exact origin, never "*": a wildcard would hand an authorization
      // code to whatever document occupies the frame.
      win.postMessage({ source: "asale-host", ...payload }, origin);
    },
    [origin],
  );

  /* Which authorization requests have already been answered. The frame repeats
     its "ready" a few times (its own listener may not exist yet when the first
     one leaves), and each of those repeats carries the same request — one code
     per request, not one per ping. */
  const answered = useRef<Set<string>>(new Set());

  /* The frame announces itself on every load — a reload inside it, a restored
     window — so everything is re-sent each time rather than once. Each app
     names itself in that message; anything else on the origin is not ours.

     Two different frames talk to this listener. The app's own origin sends
     "ready" with an authorization request (studio, swarm). AEO's frame instead
     navigates itself to asale.ai's consent page, and *that* page asks — from
     the website's origin, not the app's — because inside this shell it finds no
     browser session to approve with. Same answer either way: the daemon signs
     the request with this device's account. */
  useEffect(() => {
    const onMessage = (e: MessageEvent) => {
      const msg = e.data as { source?: string; type?: string; query?: string };
      // Shape first, origin second: in `pnpm dev` the framed app is on loopback
      // too, so an origin test cannot tell the two senders apart.
      if (msg?.source === "asale-consent" && msg.type === "authorize_request" && msg.query) {
        if (!consentOrigin(e.origin)) return;
        const q = new URLSearchParams(msg.query);
        // The request has to be the one this frame is running: a consent page
        // asking to authorize some *other* client is not something this shell
        // has any business signing.
        if (q.get("client_id") !== CLIENT_ID[id]) return;
        invoke<{ redirect_to: string }>("app_authorize", {
          app: id,
          redirectUri: q.get("redirect_uri") || "",
          state: q.get("state") || "",
          codeChallenge: q.get("code_challenge") || "",
        })
          .then((r) => e.source && (e.source as Window).postMessage(
            { source: "asale-host", type: "authorize_result", redirect_to: r.redirect_to },
            e.origin,
          ))
          // Silence is an answer here: the consent page waits a beat and then
          // offers its own sign-in, which is the right fallback for a shell
          // that is signed out.
          .catch((err) => setErr(errText(err)));
        return;
      }
      if (e.origin !== origin) return;
      const d = e.data as { source?: string; type?: string; oauth?: AuthRequest };
      if (d?.source !== `asale-${id}` || d.type !== "ready") return;

      const req = d.oauth;
      const doOauth = !!(req?.state && req.code_challenge && redirect);
      // Said out loud so the app keeps waiting instead of drawing a sign-in
      // screen on top of an answer that is one round trip away.
      post({ type: "prefs", locale: i18n.language, theme, oauthPending: doOauth });
      if (!doOauth || answered.current.has(req!.state)) return;
      answered.current.add(req!.state);

      invoke<{ code: string; state: string; error: string }>("app_authorize", {
        app: id,
        redirectUri: redirect,
        state: req!.state,
        codeChallenge: req!.code_challenge,
      })
        .then((r) => post({ type: "oauth_code", ...r }))
        .catch((e) => {
          // Signed out, or a daemon that cannot reach the server: the frame's
          // own sign-in screen cannot help with either — its popup would ask
          // for the account this app is already the shell for — so this page
          // says what happened instead of leaving a button that does nothing.
          answered.current.delete(req!.state);
          setErr(errText(e));
        });
    };
    window.addEventListener("message", onMessage);
    return () => window.removeEventListener("message", onMessage);
  }, [post, origin, id, redirect, i18n.language, theme]);

  /* Language and theme follow the app they are framed by. */
  useEffect(() => {
    post({ type: "prefs", locale: i18n.language, theme });
  }, [post, i18n.language, theme]);

  if (!origin) {
    return <Empty icon={<IconSparkle />} title={t("apps.unavailable")} />;
  }

  return (
    // No "back" of its own: the open app draws to every edge, and a button
    // floating over its chrome reads as a bug. The rail item you came in
    // through is the way back — pressing "应用" with one open returns to the
    // list (see `App.tsx`), which is also how the website behaves, where the
    // frame sits under the site's own nav.
    <div className="app-page">
      {err ? (
        <Err>{err}</Err>
      ) : (
        <iframe
          ref={frame}
          title={t(`apps.${id}`)}
          src={url}
          className="app-frame"
          // First-party code on a sibling origin: it gets what it needs to run
          // and nothing that would let it navigate the shell around it.
          sandbox="allow-scripts allow-same-origin allow-forms allow-downloads allow-popups allow-modals"
          allow="clipboard-write"
        />
      )}
    </div>
  );
}

export type { AppId };

// The applications, in the desktop app: a card each, and the system browser
// when one is opened.
//
// They used to be framed here, in a webview tab, with this shell completing the
// one leg of their OAuth a framed app cannot do for itself (an authorization
// code relayed over `postMessage`, see `daemon/commands/app_auth.rs`). The
// frame is gone: it left each app with no URL, no title and no way back, and a
// reload inside it dropped the user at the app's entry screen. Standalone they
// sign themselves in — a top-level navigation to asale.ai's consent page and
// straight back, no click for a visitor already signed in there.
//
// `app_authorize` stays on the daemon: released builds still frame these apps
// and still ask for it.

import { useTranslation } from "react-i18next";
import { AEO_URL, STUDIO_URL, SWARM_URL } from "../links";
import { PageHead } from "../ui";
import { IconChat, IconEye, IconUsers } from "../icons";
import { openExternal } from "../shell";

const APPS = [
  { id: "studio", url: STUDIO_URL, Icon: IconChat },
  { id: "swarm", url: SWARM_URL, Icon: IconUsers },
  { id: "aeo", url: AEO_URL, Icon: IconEye },
] as const;

export function Apps() {
  const { t } = useTranslation();

  return (
    <>
      <PageHead title={t("apps.title")} sub={t("apps.sub")} />
      <div className="app-cards">
        {APPS.map(({ id, url, Icon }) => (
          // The system browser, not this webview: it has no tabs, no address
          // bar and no back button, so an app opened inside it would strand the
          // user in exactly the way the frame did.
          <button key={id} type="button" className="card app-card" onClick={() => openExternal(url)}>
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

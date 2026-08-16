import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { getVersion } from "@tauri-apps/api/app";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { apiBase, daemonToken, invoke, realTauri } from "../lib";
import { openExternal, shell } from "../shell";
import { checkForUpdate, useInstaller, useUpdateState } from "../lib/updates";
import { UpdateProgress } from "../components/UpdateProgress";
import { Card, CopyChip, Err, Ok, PageHead, FactGrid } from "../ui";
import { IconPower, IconDownload, IconRefresh, IconCheck, IconServer, IconGlobe, IconInfo, IconExternal, IconAlert } from "../icons";
import { errText } from "../errors";

interface DaemonInfo {
  name: string;
  version: string;
  data_dir: string;
  /** Where the daemon actually listens; null until its listener is up. */
  bind: string | null;
  port: number | null;
  /** True when that address is reachable from other machines — at which point
   *  the token in the URL below is the only thing protecting the account. */
  remote: boolean | null;
}

type ProxyMode = "auto" | "off" | "manual";

interface ProxySettings {
  mode: ProxyMode;
  url: string;
  /** What the daemon dials right now, however it was decided. */
  effective: string | null;
  /** Set in the environment, this outranks anything chosen here. */
  env_override: string | null;
  env_var: string;
}

interface ProxyTest {
  ok: boolean;
  detail: string;
  ms: number;
  proxy: string | null;
}

export function Settings() {
  const { t } = useTranslation();
  const [version, setVersion] = useState("");
  const [daemon, setDaemon] = useState<DaemonInfo | null>(null);
  const [daemonErr, setDaemonErr] = useState("");

  const [autostart, setAutostart] = useState<boolean | null>(null);
  const [autostartBusy, setAutostartBusy] = useState(false);
  const [autostartErr, setAutostartErr] = useState("");

  const [webUrl, setWebUrl] = useState("");
  const [webErr, setWebErr] = useState("");
  const [closeToTray, setCloseToTray] = useState<boolean | null>(null);

  const [proxy, setProxy] = useState<ProxySettings | null>(null);
  const [proxyMode, setProxyMode] = useState<ProxyMode>("auto");
  const [proxyUrl, setProxyUrl] = useState("");
  const [proxyBusy, setProxyBusy] = useState<"" | "save" | "test">("");
  const [proxySaved, setProxySaved] = useState(false);
  const [proxyTest, setProxyTest] = useState<ProxyTest | null>(null);
  const [proxyErr, setProxyErr] = useState("");

  // Shared with the sidebar marker and with the ten-minute background check, so
  // opening this page shows what the loop already found instead of asking again
  // — and a download started here survives leaving the page.
  const { phase, available, latest, page, error: updateErr } = useUpdateState();

  // How an update is applied — the same two-step the upgrade banner uses.
  const installer = useInstaller();

  useEffect(() => {
    // Daemon self-description — real data in every mode (shell or browser).
    invoke<DaemonInfo>("daemon_info")
      .then((d) => { setDaemon(d); setDaemonErr(""); })
      .catch((e) => setDaemonErr(errText(e)));
    invoke<ProxySettings>("proxy_settings").then(applyProxy).catch((e) => setProxyErr(errText(e)));

    // The URL that opens this daemon in a browser. In the shell the token comes
    // from the file the shell already read; in a browser we are *on* that URL,
    // so it is rebuilt from the origin plus the token this page authenticates
    // with — a link without the token gets the daemon's unlock page, not the app.
    if (realTauri) {
      shell.webUiUrl()
        .then((u) => setWebUrl(u ?? ""))
        .catch((e) => setWebErr(errText(e)));
    } else {
      const origin = apiBase() || window.location.origin;
      const tok = daemonToken();
      setWebUrl(tok ? `${origin}/#token=${encodeURIComponent(tok)}` : origin);
    }

    if (!realTauri) return;
    // Shell-only info: the desktop app's own version.
    getVersion().then(setVersion).catch(() => {});
    isEnabled().then(setAutostart).catch((e) => setAutostartErr(errText(e)));
    shell.getCloseToTray().then((v) => setCloseToTray(v ?? true)).catch(() => setCloseToTray(true));
  }, []);

  const applyProxy = (p: ProxySettings) => {
    setProxy(p);
    setProxyMode(p.mode);
    setProxyUrl(p.url);
    setProxyErr("");
  };

  const saveProxy = async () => {
    setProxyBusy("save"); setProxyErr(""); setProxySaved(false);
    try {
      applyProxy(await invoke<ProxySettings>("set_proxy_settings", { mode: proxyMode, url: proxyUrl }));
      setProxySaved(true);
    } catch (e) { setProxyErr(errText(e)); } finally { setProxyBusy(""); }
  };

  // Tests what is on screen, not what is saved, so a proxy can be verified
  // before committing to it.
  const runProxyTest = async () => {
    setProxyBusy("test"); setProxyErr(""); setProxyTest(null);
    try { setProxyTest(await invoke<ProxyTest>("test_proxy", { mode: proxyMode, url: proxyUrl })); }
    catch (e) { setProxyErr(errText(e)); } finally { setProxyBusy(""); }
  };

  const proxyDirty = proxy !== null && (proxyMode !== proxy.mode || (proxyMode === "manual" && proxyUrl !== proxy.url));

  // Two writes on purpose: the shell applies it to the *running* window
  // immediately, and the daemon store is what survives a restart — and what the
  // same switch reads when it is flipped from a browser instead.
  const toggleCloseToTray = async () => {
    if (closeToTray === null) return;
    const next = !closeToTray;
    setCloseToTray(next);
    await shell.setCloseToTray(next).catch(() => {});
    invoke("set_setting", { key: "close_to_tray", value: next ? "1" : "0" }).catch(() => {});
  };

  const toggleAutostart = async () => {
    if (autostart === null || autostartBusy) return;
    setAutostartBusy(true); setAutostartErr("");
    try { if (autostart) await disable(); else await enable(); setAutostart(await isEnabled()); }
    catch (e) { setAutostartErr(errText(e)); } finally { setAutostartBusy(false); }
  };

  return (
    <div>
      <PageHead title={t("settings.title")} sub={t("settings.sub")} />

      {/* The daemon — the real backend every frontend (shell/browser) talks to. */}
      <Card
        icon={<IconServer />}
        title={t("settings.daemonTitle")}
        desc={t("settings.daemonDesc")}
        right={daemon ? <span className="pill on plain">{t("settings.daemonRunning")}</span> : daemonErr ? <span className="pill err">{t("settings.daemonStopped")}</span> : null}
      >
        {daemon ? (
          <FactGrid facts={[
            { k: t("settings.daemonVersion"), v: <span className="mono">{daemon.name} v{daemon.version}</span> },
            { k: t("settings.daemonData"), v: <span className="mono" title={daemon.data_dir}>{daemon.data_dir}</span> },
          ]} />
        ) : daemonErr ? (
          <Err>{t("settings.daemonUnreachable", { msg: daemonErr })}</Err>
        ) : null}
      </Card>

      {/* Web mode. The daemon serves the whole app itself, so "open this in a
          browser" is not a lesser version of the desktop window — it is the
          only version on a machine with no desktop, and the way to check on a
          selling box from a phone. The URL carries the daemon token, which is
          why this card spends as many words on who can use it as on how. */}
      <Card
        icon={<IconGlobe />}
        title={t("settings.webTitle")}
        desc={t("settings.webDesc")}
        right={daemon?.remote ? <span className="pill warn">{t("settings.webRemotePill")}</span> : null}
      >
        {webUrl ? (
          <>
            <div className="field">
              <label>{t("settings.webUrl")}</label>
              <CopyChip value={webUrl} wrap />
              <div className="hint">{t("settings.webUrlHint")}</div>
            </div>

            {realTauri && (
              <div className="btn-row">
                <button className="btn" onClick={() => shell.openWebUi().catch((e) => setWebErr(errText(e)))}>
                  <IconExternal />
                  {t("settings.webOpen")}
                </button>
              </div>
            )}

            {daemon?.bind && (
              <div className="fact-grid tight card-foot">
                <div className="fact">
                  <span className="fact-k">{t("settings.webBind")}</span>
                  <span className="fact-v mono">{daemon.bind}</span>
                </div>
              </div>
            )}

            {/* Bound past loopback, the token is the entire access control, and
                it is sitting in a URL. Say so where the URL is. */}
            {daemon?.remote ? (
              <div className="callout warn card-foot">
                <IconAlert />
                <span>{t("settings.webRemoteWarn")}</span>
              </div>
            ) : (
              <div className="callout card-foot">
                <IconInfo />
                <span>{t("settings.webLocalHint")}</span>
              </div>
            )}
          </>
        ) : (
          <Err>{webErr || t("settings.webUnavailable")}</Err>
        )}
      </Card>

      {/* Upstream proxy. Provider endpoints are unreachable from some regions,
          and the daemon inherits no shell environment when the desktop shell
          launches it — this is where a user points us at their local proxy. */}
      <Card icon={<IconGlobe />} title={t("settings.proxyTitle")} desc={t("settings.proxyDesc")}>
        {proxy && (
          <>
            {proxy.env_override && (
              <div className="callout card-lead">
                <IconInfo />
                <span>{t("settings.proxyEnvOverride", { name: proxy.env_var, value: proxy.env_override })}</span>
              </div>
            )}

            <div className="field">
              <label htmlFor="proxy-mode">{t("settings.proxyMode")}</label>
              <select
                id="proxy-mode"
                className="input"
                value={proxyMode}
                onChange={(e) => { setProxyMode(e.target.value as ProxyMode); setProxySaved(false); setProxyTest(null); }}
              >
                <option value="auto">{t("settings.proxyModeAuto")}</option>
                <option value="off">{t("settings.proxyModeOff")}</option>
                <option value="manual">{t("settings.proxyModeManual")}</option>
              </select>
              <div className="hint">{t(`settings.proxyModeHint.${proxyMode}`)}</div>
            </div>

            {proxyMode === "manual" && (
              <div className="field">
                <label htmlFor="proxy-url">{t("settings.proxyUrl")}</label>
                <input
                  id="proxy-url"
                  className="input"
                  placeholder="http://127.0.0.1:7890"
                  value={proxyUrl}
                  onChange={(e) => { setProxyUrl(e.target.value); setProxySaved(false); setProxyTest(null); }}
                />
                <div className="hint">{t("settings.proxyUrlHint")}</div>
              </div>
            )}

            <div className="btn-row">
              <button className="btn" onClick={saveProxy} disabled={proxyBusy !== "" || !proxyDirty}>
                {proxyBusy === "save" ? <IconRefresh className="spin" /> : <IconCheck />}
                {t("settings.proxySave")}
              </button>
              <button className="btn ghost" onClick={runProxyTest} disabled={proxyBusy !== ""}>
                {proxyBusy === "test" ? <IconRefresh className="spin" /> : <IconRefresh />}
                {proxyBusy === "test" ? t("settings.proxyTesting") : t("settings.proxyTest")}
              </button>
              {proxySaved && !proxyDirty && <span className="pill on plain"><IconCheck />{t("settings.proxySaved")}</span>}
            </div>

            <div className="fact-grid tight card-foot">
              <div className="fact">
                <span className="fact-k">{t("settings.proxyEffective")}</span>
                <span className="fact-v mono">{proxy.effective ?? t("settings.proxyDirect")}</span>
              </div>
            </div>

            {proxyTest && (
              proxyTest.ok
                ? <Ok>{t("settings.proxyTestOk", { ms: proxyTest.ms })}</Ok>
                : <Err>{proxyTest.detail === "blocked"
                    ? t("settings.proxyTestBlocked")
                    : t("settings.proxyTestFailed", { msg: proxyTest.detail })}</Err>
            )}
          </>
        )}
        {proxyErr && <Err>{proxyErr}</Err>}
      </Card>

      {realTauri ? (
        <>
          {/* Closing the window keeps asale selling. That is the right default
              and the surprising one, so it is a switch rather than a footnote —
              and the tray panel it hides into is described next to it. */}
          <Card icon={<IconServer />} title={t("settings.trayTitle")} desc={t("settings.trayDesc")}>
            <div className="switch-row">
              <span className="switch-label">{t("settings.trayCloseLabel")}</span>
              <label className="switch">
                <input
                  type="checkbox"
                  checked={closeToTray === true}
                  disabled={closeToTray === null}
                  onChange={toggleCloseToTray}
                />
                <span className="track" />
              </label>
            </div>
            <div className="hint card-foot">
              {closeToTray === false ? t("settings.trayCloseQuits") : t("settings.trayCloseHides")}
            </div>
          </Card>

          <Card icon={<IconPower />} title={t("settings.autostartTitle")} desc={t("settings.autostartDesc")}>
            <div className="switch-row">
              <span className="switch-label">{t("settings.autostartLabel")}</span>
              <label className="switch">
                <input type="checkbox" checked={autostart === true} disabled={autostart === null || autostartBusy} onChange={toggleAutostart} />
                <span className="track" />
              </label>
            </div>
            {autostartErr && <Err>{t("settings.autostartError", { msg: autostartErr })}</Err>}
          </Card>

          <Card
            icon={<IconDownload />}
            title={t("settings.updateTitle")}
            desc={t("settings.updateDesc", { version: version || "…" })}
            right={available ? <span className="dot warn pulse" /> : null}
          >
            {/* The same check the sidebar marker reads, and the same one the
                background loop runs every ten minutes — pressing this only
                brings the next one forward. */}
            <div className="btn-row">
              <button className="btn ghost" onClick={() => void checkForUpdate()} disabled={phase === "checking"}>
                {phase === "checking" ? <IconRefresh className="spin" /> : <IconRefresh />}
                {phase === "checking" ? t("settings.updateChecking") : t("settings.updateCheck")}
              </button>
              {phase === "none" && <span className="pill on plain"><IconCheck />{t("settings.updateNone")}</span>}
              {available && (
                <>
                  <span className="pill warn">{t("settings.updateAvailable", { version: latest })}</span>
                  {page && (
                    <button type="button" className="btn ghost" onClick={() => openExternal(page)}>
                      <IconExternal />
                      {t("settings.updateNotes")}
                    </button>
                  )}
                </>
              )}
            </div>
            {phase === "error" && <Err>{t("settings.updateError", { msg: updateErr })}</Err>}

            {/* Applying it — one way, on purpose. The desktop app and the
                `asale` / `asaled` command line are two halves of one release
                that land on the machine separately, and this installer is the
                only thing that replaces both; an update that quietly fixed the
                window and left the terminal on last month's build is the
                failure this is meant to prevent.

                Everything it costs is said before it is pressed. It needs an
                administrator password (the command line lives in a root-owned
                directory), it closes the app, and it opens the app again when
                it is done — none of which should be a surprise mid-way. The
                command itself is shown too: nobody should have to take "an
                installer" on trust. */}
            <div className="card-foot">
              <div className="hint">{t("settings.reinstallDesc")}</div>
              {installer.command && <pre className="codeblock notes">{installer.command}</pre>}

              {/* Three states, one sequence — the same one the forced-upgrade
                  dialog runs, from the same shared state, so an update started
                  in either place is watched from both. */}
              {installer.running ? (
                <>
                  <div className="btn-row">
                    <button className="btn" disabled>
                      <IconRefresh className="spin" />
                      {t("settings.reinstallRunning")}
                    </button>
                  </div>
                  <UpdateProgress installer={installer} />
                </>
              ) : !installer.confirming ? (
                <div className="btn-row">
                  {/* Primary only when there is something to install; the rest
                      of the time this is a repair tool, not the thing to press. */}
                  <button className={available ? "btn" : "btn ghost"} onClick={installer.ask}>
                    <IconRefresh />
                    {t("settings.reinstallButton")}
                  </button>
                </div>
              ) : (
                <>
                  <div className="callout warn">
                    <IconAlert />
                    <span>{t("settings.reinstallConfirm")}</span>
                  </div>
                  <div className="btn-row">
                    <button className="btn" onClick={installer.run}>
                      <IconDownload />
                      {t("settings.reinstallGo")}
                    </button>
                    <button className="btn ghost" onClick={installer.cancel}>
                      {t("settings.reinstallCancel")}
                    </button>
                  </div>
                </>
              )}
              {installer.error && <Err>{t("settings.reinstallError", { msg: installer.error })}</Err>}
            </div>
          </Card>
        </>
      ) : (
        <div className="callout">
          <IconPower />
          <span>{t("settings.shellOnlyNote")}</span>
        </div>
      )}
    </div>
  );
}

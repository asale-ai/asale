import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { getVersion } from "@tauri-apps/api/app";
import { disable, enable, isEnabled } from "@tauri-apps/plugin-autostart";
import { check, type Update } from "@tauri-apps/plugin-updater";
import { relaunch } from "@tauri-apps/plugin-process";
import { invoke, realTauri } from "../lib";
import { Card, Err, Ok, PageHead, FactGrid } from "../ui";
import { IconPower, IconDownload, IconRefresh, IconCheck, IconServer, IconGlobe, IconInfo } from "../icons";

type UpdatePhase = "idle" | "checking" | "none" | "available" | "downloading" | "ready" | "error";

interface DaemonInfo {
  name: string;
  version: string;
  data_dir: string;
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

  const [proxy, setProxy] = useState<ProxySettings | null>(null);
  const [proxyMode, setProxyMode] = useState<ProxyMode>("auto");
  const [proxyUrl, setProxyUrl] = useState("");
  const [proxyBusy, setProxyBusy] = useState<"" | "save" | "test">("");
  const [proxySaved, setProxySaved] = useState(false);
  const [proxyTest, setProxyTest] = useState<ProxyTest | null>(null);
  const [proxyErr, setProxyErr] = useState("");

  const [phase, setPhase] = useState<UpdatePhase>("idle");
  const [update, setUpdate] = useState<Update | null>(null);
  const [progress, setProgress] = useState(-1);
  const [updateErr, setUpdateErr] = useState("");

  useEffect(() => {
    // Daemon self-description — real data in every mode (shell or browser).
    invoke<DaemonInfo>("daemon_info")
      .then((d) => { setDaemon(d); setDaemonErr(""); })
      .catch((e) => setDaemonErr(String((e as Error).message)));
    invoke<ProxySettings>("proxy_settings").then(applyProxy).catch((e) => setProxyErr(String((e as Error).message)));
    if (!realTauri) return;
    // Shell-only info: the desktop app's own version (updater target).
    getVersion().then(setVersion).catch(() => {});
    isEnabled().then(setAutostart).catch((e) => setAutostartErr(String(e)));
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
    } catch (e) { setProxyErr(String((e as Error).message)); } finally { setProxyBusy(""); }
  };

  // Tests what is on screen, not what is saved, so a proxy can be verified
  // before committing to it.
  const runProxyTest = async () => {
    setProxyBusy("test"); setProxyErr(""); setProxyTest(null);
    try { setProxyTest(await invoke<ProxyTest>("test_proxy", { mode: proxyMode, url: proxyUrl })); }
    catch (e) { setProxyErr(String((e as Error).message)); } finally { setProxyBusy(""); }
  };

  const proxyDirty = proxy !== null && (proxyMode !== proxy.mode || (proxyMode === "manual" && proxyUrl !== proxy.url));

  const toggleAutostart = async () => {
    if (autostart === null || autostartBusy) return;
    setAutostartBusy(true); setAutostartErr("");
    try { if (autostart) await disable(); else await enable(); setAutostart(await isEnabled()); }
    catch (e) { setAutostartErr(String(e)); } finally { setAutostartBusy(false); }
  };

  const checkForUpdate = async () => {
    setPhase("checking"); setUpdateErr(""); setUpdate(null);
    try {
      const u = await check();
      if (u) { setUpdate(u); setPhase("available"); } else setPhase("none");
    } catch (e) { setUpdateErr(String(e)); setPhase("error"); }
  };

  const downloadAndInstall = async () => {
    if (!update) return;
    setPhase("downloading"); setProgress(-1); setUpdateErr("");
    try {
      let total = 0, received = 0;
      await update.downloadAndInstall((ev) => {
        if (ev.event === "Started") { total = ev.data.contentLength ?? 0; setProgress(total > 0 ? 0 : -1); }
        else if (ev.event === "Progress") { received += ev.data.chunkLength; if (total > 0) setProgress(Math.min(100, Math.round((received / total) * 100))); }
        else if (ev.event === "Finished") setProgress(100);
      });
      setPhase("ready");
    } catch (e) { setUpdateErr(String(e)); setPhase("error"); }
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

          <Card icon={<IconDownload />} title={t("settings.updateTitle")} desc={t("settings.updateDesc", { version: version || "…" })}>
            <div className="btn-row">
              <button className="btn ghost" onClick={checkForUpdate} disabled={phase === "checking" || phase === "downloading"}>
                {phase === "checking" ? <IconRefresh className="spin" /> : <IconRefresh />}
                {phase === "checking" ? t("settings.updateChecking") : t("settings.updateCheck")}
              </button>
              {phase === "none" && <span className="pill on plain"><IconCheck />{t("settings.updateNone")}</span>}
              {phase === "available" && update && (
                <>
                  <span className="pill warn">{t("settings.updateAvailable", { version: update.version })}</span>
                  <button className="btn" onClick={downloadAndInstall}><IconDownload />{t("settings.updateInstall")}</button>
                </>
              )}
              {phase === "ready" && (
                <>
                  <span className="pill on plain"><IconCheck />{t("settings.updateReady")}</span>
                  <button className="btn" onClick={() => relaunch()}>{t("settings.updateRestart")}</button>
                </>
              )}
            </div>

            {phase === "downloading" && (
              <div className="card-foot">
                <div className="micro-label">
                  {progress >= 0 ? t("settings.updateDownloading", { pct: progress }) : t("settings.updateDownloadingIndeterminate")}
                </div>
                <div className="bar"><span style={{ width: progress >= 0 ? `${progress}%` : "40%" }} className={progress < 0 ? "skel" : ""} /></div>
              </div>
            )}

            {phase === "error" && <Err>{t("settings.updateError", { msg: updateErr })}</Err>}
            {update?.body && (phase === "available" || phase === "downloading" || phase === "ready") && (
              <div className="card-foot">
                <div className="micro-label">{t("settings.updateNotes")}</div>
                <pre className="codeblock notes">{update.body}</pre>
              </div>
            )}
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

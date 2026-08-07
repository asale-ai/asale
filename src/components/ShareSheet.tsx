// The share sheet: every route out of the app, in one grid.
//
// KEEP IN SYNC with asale-web/src/components/ShareSheet.tsx.
//
// The grid is ordered per locale (`shareTargets`), but it is never *filtered*
// by locale — someone reading the Japanese UI may well be sharing to WeChat,
// and hiding the target because of the language they picked would be the
// product deciding who their friends are.
//
// Three of the buttons do not open anything. WeChat has no web composer, so it
// drops a QR under the grid. Xiaohongshu is app-only, so it saves the image and
// copies the caption and says so. Both are dead ends if the sheet pretends they
// behave like the other twelve, which is why the outcome is announced in the
// status line rather than left to a tab that never opened.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { invoke, realTauri } from "../lib";
import {
  canNativeShare,
  nativeShare,
  openComposer,
  shareTargets,
  type SharePayload,
  type ShareTarget,
} from "../lib/share";
import { shareMark } from "./share-marks";
import { PayQr } from "./PayQr";
import { IconCheck, IconCopy, IconDownload, IconLink, IconShare } from "../icons";

/** What the sheet just did. Cleared on the next action, never auto-dismissed —
 *  the Xiaohongshu one is an instruction, and instructions that vanish on a
 *  timer get missed. */
type Notice = { text: string; tone: "ok" | "info" } | null;

export function ShareSheet({
  payload,
  file,
  fileName,
}: {
  payload: SharePayload;
  /** The rendered card. Null until the canvas has produced it, which is one
   *  frame — the image-bearing actions stay disabled until then rather than
   *  silently sharing text only. */
  file: File | null;
  fileName: string;
}) {
  const { t, i18n } = useTranslation();
  const [notice, setNotice] = useState<Notice>(null);
  const [qrOpen, setQrOpen] = useState(false);

  const targets = shareTargets(i18n.language);
  const caption = `${payload.text}\n${payload.url}`;

  async function copy(text: string, done: string) {
    try {
      await navigator.clipboard.writeText(text);
      setNotice({ text: done, tone: "ok" });
      return true;
    } catch {
      setNotice({ text: t("share.copyFailed"), tone: "info" });
      return false;
    }
  }

  /**
   * Put the PNG on disk.
   *
   * Two paths, because the desktop shell has no working one. WKWebView and
   * WebView2 both ignore `<a download>` on a `blob:` URL unless the host app
   * implements a download handler — the click does nothing, silently. So inside
   * Tauri the bytes go to the daemon, which has a filesystem; in any browser
   * (including Chrome against this same daemon) the anchor is right, because
   * the browser owns the download folder and the daemon may not even be on the
   * same machine.
   */
  async function saveImage(): Promise<string | null> {
    if (!file) return null;
    if (realTauri) {
      try {
        const buf = new Uint8Array(await file.arrayBuffer());
        let bin = "";
        for (const byte of buf) bin += String.fromCharCode(byte);
        const { path } = await invoke<{ path: string }>("save_image", {
          name: fileName,
          data: btoa(bin),
        });
        return path;
      } catch {
        return null;
      }
    }
    const url = URL.createObjectURL(file);
    const a = document.createElement("a");
    a.href = url;
    a.download = fileName;
    a.click();
    // Revoked on a delay, not immediately: Safari cancels a download whose
    // object URL is released in the same frame as the click.
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
    return fileName;
  }

  async function pick(target: ShareTarget) {
    setQrOpen(false);
    if (target.kind === "intent" && target.href) {
      openComposer(target.href(payload));
      setNotice(null);
      return;
    }
    if (target.kind === "qr") {
      setQrOpen(true);
      setNotice(null);
      return;
    }
    // `manual`: the app takes an image and a pasted caption, and nothing else.
    const path = await saveImage();
    await copy(caption, path ? t("share.manualDone", { app: t(`share.p.${target.id}`) }) : t("share.manualCopied"));
  }

  return (
    <div className="sharesheet">
      <div className="ss-grid">
        {targets.map((target) => (
          <button
            key={target.id}
            type="button"
            className="ss-tile"
            style={{ "--tint": target.tint } as React.CSSProperties}
            onClick={() => void pick(target)}
            title={t(`share.p.${target.id}`)}
          >
            <span className="ss-mark">{shareMark(target.id)}</span>
            <span className="ss-name">{t(`share.p.${target.id}`)}</span>
          </button>
        ))}
      </div>

      {qrOpen && (
        <div className="ss-qr">
          <PayQr payload={payload.url} alt={t("share.qrAlt")} size={140} />
          <div>
            <div className="ss-qr-title">{t("share.qrTitle")}</div>
            <p className="ss-qr-hint">{t("share.qrHint")}</p>
          </div>
        </div>
      )}

      <div className="ss-actions">
        <button type="button" className="btn ghost sm" onClick={() => void copy(caption, t("share.copiedText"))}>
          <IconCopy />
          {t("share.copyText")}
        </button>
        <button type="button" className="btn ghost sm" onClick={() => void copy(payload.url, t("share.copiedLink"))}>
          <IconLink />
          {t("share.copyLink")}
        </button>
        <button
          type="button"
          className="btn ghost sm"
          disabled={!file}
          onClick={() =>
            void saveImage().then((path) =>
              setNotice(
                path
                  ? { text: t("share.savedTo", { path }), tone: "ok" }
                  : { text: t("share.saveFailed"), tone: "info" },
              ),
            )
          }
        >
          <IconDownload />
          {t("share.saveImage")}
        </button>
        {/* Only where the OS actually has a sheet. The desktop webview does not
            expose one, and a button that resolves to nothing is worse than no
            button. */}
        {canNativeShare(file) && (
          <button
            type="button"
            className="btn ghost sm"
            onClick={() => void nativeShare(payload, file).then((ok) => ok && setNotice(null))}
          >
            <IconShare />
            {t("share.systemShare")}
          </button>
        )}
      </div>

      {notice && (
        <p className={`ss-notice${notice.tone === "ok" ? " ok" : ""}`}>
          {notice.tone === "ok" && <IconCheck />}
          <span>{notice.text}</span>
        </p>
      )}
    </div>
  );
}

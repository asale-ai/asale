// A QR code rendered as inline SVG, synchronously.
//
// `QRCode.toDataURL` is async, which costs two things that matter here. The
// code on screen lags the wallet the user just picked by a frame or more, so
// for that moment the visible code is the *previous* one — and the whole point
// of the picker is that different wallets get different payloads. And a raster
// PNG at a fixed pixel width is soft on a high-DPI screen, where a QR is
// exactly the kind of hard-edged art that suffers.
//
// `QRCode.create` is synchronous, so the code is derived during render and can
// never disagree with the payload it came from. Drawing the modules as one SVG
// path is cheap: a few hundred rects merged into a single `d`.
//
// Kept in sync with asale-web's `src/components/PayQr.tsx`.

import { useMemo } from "react";
import QRCode from "qrcode";

/** Quiet zone in modules. The spec says 4, and scanners genuinely need it —
 *  a code butted against its container is the classic "won't scan" bug. */
const QUIET = 4;

export function PayQr({ payload, alt, size = 220 }: { payload: string; alt: string; size?: number }) {
  const qr = useMemo(() => {
    if (!payload) return null;
    try {
      // Medium recovery: a payment URI is longer than a bare address, and the
      // higher levels grow the symbol enough to hurt scanning on a phone held
      // at arm's length. M is what the library defaults to for good reason.
      const { modules } = QRCode.create(payload, { errorCorrectionLevel: "M" });
      const n = modules.size;
      let d = "";
      for (let y = 0; y < n; y++) {
        for (let x = 0; x < n; x++) {
          if (modules.data[y * n + x]) d += `M${x} ${y}h1v1h-1z`;
        }
      }
      return { d, n };
    } catch {
      // Only reachable if the payload exceeds QR capacity, which an address or
      // a payment URI never does. Render nothing rather than a broken code.
      return null;
    }
  }, [payload]);

  if (!qr) return null;
  const span = qr.n + QUIET * 2;
  return (
    <svg
      className="pay-qr"
      width={size}
      height={size}
      viewBox={`${-QUIET} ${-QUIET} ${span} ${span}`}
      role="img"
      aria-label={alt}
      shapeRendering="crispEdges"
    >
      {/* The plate is white in both themes: inverting a QR breaks scanners
          that assume dark modules on a light field. */}
      <rect x={-QUIET} y={-QUIET} width={span} height={span} fill="#fff" />
      <path d={qr.d} fill="#000" />
    </svg>
  );
}

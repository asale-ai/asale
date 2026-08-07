//! The share card's one native need: putting a PNG somewhere the user can find
//! it again.
//!
//! Why this is not `<a download>` in the frontend. In a browser it would be —
//! and is; the frontend only calls this from inside the Tauri shell. There the
//! webview is WKWebView (macOS) or WebView2 (Windows), neither of which honours
//! a download from a `blob:` URL without the host application implementing a
//! download handler. The click does nothing at all, with no error to show. The
//! daemon already has a filesystem and the shell does not, so the bytes come
//! here instead.

use base64::Engine;
use serde_json::{json, Value};

use super::{err, R};
use crate::cmd_err;

/// Refuse anything that is not plausibly a share card. The frontend renders
/// 1200×630 and 1080×1350 PNGs, which land around 200–600 KB; this is loose
/// enough to never bite and tight enough that a malformed call cannot ask the
/// daemon to write a gigabyte.
const MAX_BYTES: usize = 12 * 1024 * 1024;

/// Write a base64 PNG into the user's downloads folder, and answer with the
/// path it landed on so the UI can name it.
///
/// `name` is treated as a filename and nothing else: any directory part is
/// dropped rather than honoured. The command is reachable from any page the
/// daemon serves, and "the frontend would never send a path" is not a property
/// worth relying on when the alternative costs one line.
pub async fn save_image(name: String, data: String) -> R<Value> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| cmd_err!("errors.share.badImage", format!("not base64: {e}")))?;
    if bytes.is_empty() || bytes.len() > MAX_BYTES {
        return Err(cmd_err!(
            "errors.share.badImage",
            format!("image is {} bytes, which is not a share card", bytes.len())
        ));
    }
    // PNG magic. The only writer is our own canvas, so this is a sanity check
    // rather than a security boundary — but it keeps a stray call from leaving
    // something that is not an image behind a `.png` name.
    if !bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Err(cmd_err!("errors.share.badImage", "payload is not a PNG"));
    }

    let file = safe_name(&name);
    let dir = downloads_dir();
    let path = dir.join(&file);

    tokio::fs::create_dir_all(&dir).await.map_err(err)?;
    tokio::fs::write(&path, &bytes).await.map_err(|e| {
        cmd_err!(
            "errors.share.saveFailed",
            format!("could not write {}: {e}", path.to_string_lossy())
        )
    })?;

    Ok(json!({ "path": path.to_string_lossy() }))
}

/// The basename, with anything that could steer it out of the folder removed
/// and a `.png` guaranteed.
fn safe_name(raw: &str) -> String {
    let base = raw
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('.');
    let cleaned: String = base
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        .take(80)
        .collect();
    if cleaned.len() > 4 && cleaned.to_ascii_lowercase().ends_with(".png") {
        cleaned
    } else if cleaned.is_empty() {
        "asale-card.png".into()
    } else {
        format!("{cleaned}.png")
    }
}

/// Where a user looks for something they just saved.
///
/// `~/Downloads` when it exists — which is every desktop this ships to, and the
/// folder the browser path writes to as well, so the two entry points agree.
/// Home is the fallback rather than a created `Downloads`: on a box where it
/// does not exist the convention is not in use, and inventing the folder is
/// more surprising than the file simply being in the home directory.
fn downloads_dir() -> std::path::PathBuf {
    let home = crate::state::home_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let downloads = home.join("Downloads");
    if downloads.is_dir() {
        downloads
    } else {
        home
    }
}

#[cfg(test)]
mod tests {
    use super::safe_name;

    #[test]
    fn strips_directories_and_forces_png() {
        assert_eq!(safe_name("asale-k7m2qp9x-wide.png"), "asale-k7m2qp9x-wide.png");
        assert_eq!(safe_name("../../etc/passwd"), "passwd.png");
        assert_eq!(safe_name("C:\\Windows\\system32\\x"), "x.png");
        assert_eq!(safe_name("   "), "asale-card.png");
        assert_eq!(safe_name("card"), "card.png");
        // A name that is only separators must not become a dotfile.
        assert_eq!(safe_name("..."), "asale-card.png");
    }
}

//! Tracing setup for both entry points — `asaled` and the Tauri shell.
//!
//! Lives here rather than in each `main` because the two used to configure
//! logging independently, and only one of them is how the app actually runs: the
//! desktop shell owns the daemon in-process, and its stdout goes wherever the
//! GUI was launched from — which, for a windowed build, is nowhere. That made
//! the publisher's record of *why* an upstream rejected a sale
//! (`executor::execute` logs the upstream's own words at WARN before mapping it
//! to a code) unreadable after the fact: the only copy went to a stream nobody
//! keeps. A sale that fails for `upstream_400` was diagnosable from the log line
//! and from nothing else.
//!
//! So both entry points now also append to `{data_dir}/asale.log`. The upstream's
//! words still never leave this process — the error frame the gateway receives
//! carries the status and nothing more — they are just kept where the operator
//! can read them.

use std::fs::OpenOptions;
use std::path::PathBuf;
use std::sync::Arc;
use tracing_subscriber::fmt::writer::MakeWriterExt;

/// Rotate at 8 MiB, keeping one previous file. Enough history to cover a
/// session's worth of failures without an unbounded file in the user's home.
const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub fn log_path() -> PathBuf {
    PathBuf::from(crate::state::data_dir()).join("asale.log")
}

/// Install the process-wide subscriber: stdout plus `{data_dir}/asale.log`.
///
/// Falls back to stdout alone if the file cannot be opened — a read-only or
/// missing data dir must not stop the daemon from starting.
pub fn init() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info".into());

    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Rotate before opening, so the check runs once per start rather than per
    // line: a long-lived session can overshoot the cap, which is fine.
    if std::fs::metadata(&path).map(|m| m.len() > MAX_LOG_BYTES).unwrap_or(false) {
        let _ = std::fs::rename(&path, path.with_extension("log.1"));
    }

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(file) => {
            // No ANSI in the file: it is read with a text editor, not a terminal.
            tracing_subscriber::fmt()
                .with_env_filter(filter)
                .with_ansi(false)
                .with_writer(Arc::new(file).and(std::io::stdout))
                .init();
            tracing::info!(path = %path.display(), "logging to file");
        }
        Err(e) => {
            tracing_subscriber::fmt().with_env_filter(filter).init();
            tracing::warn!(path = %path.display(), "cannot open log file, stdout only: {e}");
        }
    }
}

//! Which buy-side CLIs are running on this machine right now.
//!
//! Flipping a buy switch rewrites a tool's config, and every one of these tools
//! reads its config once at startup — so the change reaches the next session,
//! not the one already up. The page has always said so in words. This turns that
//! sentence into a fact: *these two* Claude Code processes are still on the old
//! config, here are their pids.
//!
//! # Why it only ever looks
//!
//! It would be a small step from here to a "restart it for me" button, and that
//! button cannot be built honestly. These are interactive processes attached to
//! a pty owned by a terminal or an editor's terminal pane; nothing outside can
//! hand a fresh process that same pane. All an outsider can do is kill one, which
//! throws away the session's context and whatever it was in the middle of — and
//! one of the processes on the list is quite possibly the session that is driving
//! asale. So this module enumerates and never signals.
//!
//! # How the enumeration works
//!
//! Shelling out to the platform's own process lister, for the reason
//! `core::http::system_proxy` shells out to `reg.exe`: it keeps a platform
//! process-table dependency out of a crate every target has to build. Windows
//! goes through PowerShell + CIM (the only way to get a command line without a
//! native API), everything else through `ps`.
//!
//! Failure is not an error. A machine where the lister is missing, locked down or
//! slow answers `None`, the frontend simply does not draw the extra line, and the
//! restart advice reads exactly as it did before.

use serde::Serialize;

/// One process from the platform's lister, before we decide what it is.
struct Proc {
    pid: u32,
    parent: u32,
    /// Full command line where the platform gives us one, the executable's name
    /// where it does not (a Windows process can refuse to report its own).
    cmd: String,
}

/// One running instance of a buy-side CLI.
#[derive(Debug, Clone, Serialize)]
pub struct Running {
    pub pid: u32,
    /// The command line it was started with — the only thing that tells two
    /// sessions of the same tool apart on screen.
    pub cmd: String,
}

/// How to recognise one tool among a few hundred processes.
struct Spec {
    tool: &'static str,
    /// Executable name, without extension: the reliable signal, and the only
    /// one for a tool shipped as a native binary.
    bin: &'static str,
    /// Package paths that identify a tool launched through a runtime, where the
    /// executable is `node` and says nothing. Matched against the whole command
    /// line with separators normalised to `/`.
    markers: &'static [&'static str],
}

/// Kept in the same order as [`crate::tool_config::TOOLS`], and covering the
/// same set — a tool with a buy switch and no entry here would silently report
/// "nothing running" forever. `specs_cover_every_tool` holds the two together.
const SPECS: &[Spec] = &[
    Spec {
        tool: "claude",
        bin: "claude",
        // macOS/Linux installs run `node …/@anthropic-ai/claude-code/cli.js`;
        // the Windows package ships a `claude.exe` shim, which `bin` catches.
        markers: &["@anthropic-ai/claude-code", "claude-code/cli.js"],
    },
    Spec { tool: "codex", bin: "codex", markers: &["@openai/codex"] },
    Spec { tool: "gemini", bin: "gemini", markers: &["@google/gemini-cli"] },
    // No markers for these two: their names are ordinary words (React Native
    // ships a `hermes` engine binary), so only an executable actually *called*
    // `openclaw`/`hermes` counts. A missed instance costs a line on screen; a
    // false one tells the user to restart something unrelated.
    Spec { tool: "openclaw", bin: "openclaw", markers: &[] },
    Spec { tool: "hermes", bin: "hermes", markers: &[] },
    // opencode ships a compiled binary (`opencode.exe` under the npm package),
    // so the name is the whole signal; the package path is a marker anyway for
    // installs that go through the JS entry point.
    Spec { tool: "opencode", bin: "opencode", markers: &["opencode-ai/bin"] },
];

/// Every running buy-side CLI, as `(tool, instance)` pairs.
///
/// `None` means the process table could not be read at all — which the caller
/// must not report as "nothing is running".
pub fn scan() -> Option<Vec<(&'static str, Running)>> {
    let procs = processes()?;
    let me = std::process::id();

    let mut hits: Vec<(&'static str, u32, u32, String)> = procs
        .iter()
        // Anything we started ourselves is our own business, not a session the
        // user has to restart: `codex_catalog` shells out to `codex`.
        .filter(|p| p.pid != me && p.parent != me)
        .filter_map(|p| tool_of(p).map(|tool| (tool, p.pid, p.parent, p.cmd.clone())))
        .collect();

    // An npm shim launching the real binary is one session, not two. Drop a hit
    // whose parent is also a hit for the same tool and keep the child, which is
    // the process actually running the CLI.
    let parents: Vec<(&str, u32)> = hits.iter().map(|(tool, pid, _, _)| (*tool, *pid)).collect();
    hits.retain(|(tool, _, parent, _)| !parents.iter().any(|(t, pid)| t == tool && pid == parent));

    Some(
        hits
            .into_iter()
            .map(|(tool, pid, _, cmd)| (tool, Running { pid, cmd }))
            .collect(),
    )
}

/// Which tool, if any, this process is an instance of.
fn tool_of(p: &Proc) -> Option<&'static str> {
    let hay = p.cmd.replace('\\', "/").to_lowercase();
    let base = exe_name(&p.cmd).to_lowercase();
    SPECS
        .iter()
        .find(|s| base == s.bin || s.markers.iter().any(|m| hay.contains(m)))
        .map(|s| s.tool)
}

/// The executable's bare name, from a command line: first argument, last path
/// segment, no extension. A quoted first argument keeps its spaces — the usual
/// Windows shape is `"C:\Program Files\…\claude.exe" --flag`.
fn exe_name(cmd: &str) -> String {
    let cmd = cmd.trim();
    let argv0 = if let Some(rest) = cmd.strip_prefix('"') {
        rest.split('"').next().unwrap_or(rest)
    } else {
        cmd.split_whitespace().next().unwrap_or(cmd)
    };
    let base = argv0.rsplit(['/', '\\']).next().unwrap_or(argv0);
    match base.rsplit_once('.') {
        // Only the launcher extensions Windows uses; a name like `gemini.v2`
        // must not lose its tail.
        Some((stem, ext)) if matches!(ext.to_ascii_lowercase().as_str(), "exe" | "cmd" | "bat" | "ps1") => {
            stem.to_string()
        }
        _ => base.to_string(),
    }
}

/// Every process on the machine, or `None` if the platform would not say.
#[cfg(target_os = "windows")]
fn processes() -> Option<Vec<Proc>> {
    use std::os::windows::process::CommandExt;
    /// Without it the scan flashes a console window on the GUI build.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    // CIM is the only way to a command line without a native API. The explicit
    // output encoding matters: PowerShell 5.1 otherwise writes in the console's
    // ANSI codepage, which mangles every non-ASCII path (a Chinese user name is
    // enough) into replacement characters.
    let script = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; \
         Get-CimInstance Win32_Process | \
         Select-Object ProcessId,ParentProcessId,Name,CommandLine | \
         ConvertTo-Json -Compress";
    let out = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    // A single row comes back as an object rather than a one-element array.
    let rows = match &v {
        serde_json::Value::Array(rows) => rows.clone(),
        serde_json::Value::Object(_) => vec![v],
        _ => return None,
    };
    Some(
        rows.iter()
            .filter_map(|r| {
                let pid = r.get("ProcessId")?.as_u64()? as u32;
                let parent = r.get("ParentProcessId").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
                // A process can decline to report its command line (another
                // user's, or a protected one); its name still identifies it.
                let cmd = r
                    .get("CommandLine")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| r.get("Name").and_then(|x| x.as_str()))?
                    .to_string();
                Some(Proc { pid, parent, cmd })
            })
            .collect(),
    )
}

#[cfg(not(target_os = "windows"))]
fn processes() -> Option<Vec<Proc>> {
    // `args=` last and with an empty header, so the command line keeps its own
    // spaces and everything before it parses by position.
    let out = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,args="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|line| {
                let mut it = line.trim_start().splitn(3, char::is_whitespace);
                let pid = it.next()?.parse().ok()?;
                let parent = it.next()?.trim().parse().unwrap_or(0);
                let cmd = it.next()?.trim().to_string();
                if cmd.is_empty() {
                    return None;
                }
                Some(Proc { pid, parent, cmd })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_cover_every_tool() {
        let specced: Vec<&str> = SPECS.iter().map(|s| s.tool).collect();
        assert_eq!(specced, crate::tool_config::TOOLS.to_vec());
    }

    #[test]
    fn a_windows_shim_is_recognised_by_its_name() {
        let p = Proc {
            pid: 1,
            parent: 0,
            cmd: r#""C:\Users\u\AppData\Roaming\npm\node_modules\@anthropic-ai\claude-code\bin\claude.exe" --resume"#
                .into(),
        };
        assert_eq!(exe_name(&p.cmd), "claude");
        assert_eq!(tool_of(&p), Some("claude"));
    }

    #[test]
    fn a_node_hosted_cli_is_recognised_by_its_package() {
        // The executable is `node`, which names no tool: only the package path
        // in the rest of the command line does.
        let p = Proc {
            pid: 1,
            parent: 0,
            cmd: "/usr/local/bin/node /usr/local/lib/node_modules/@google/gemini-cli/dist/index.js".into(),
        };
        assert_eq!(exe_name(&p.cmd), "node");
        assert_eq!(tool_of(&p), Some("gemini"));
    }

    #[test]
    fn a_word_in_a_path_is_not_a_running_tool() {
        // The failure this guards: matching `hermes` anywhere in the command
        // line puts React Native's engine — or a directory that happens to be
        // called that — on the list, and tells the user to restart it.
        for cmd in [
            "/usr/bin/node /home/u/hermes/build.js",
            "grep -r claude /src",
            "/opt/rn/hermesc -emit-binary app.js",
        ] {
            let p = Proc { pid: 1, parent: 0, cmd: cmd.into() };
            assert_eq!(tool_of(&p), None, "{cmd}");
        }
    }

    #[test]
    fn an_editor_is_not_the_cli_it_hosts() {
        // A terminal's own process carries the shell, not the tool; only the
        // child running the CLI should be listed.
        let p = Proc { pid: 1, parent: 0, cmd: "/bin/zsh -l".into() };
        assert_eq!(tool_of(&p), None);
    }
}

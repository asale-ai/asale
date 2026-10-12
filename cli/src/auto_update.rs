//! One-time registration of a privileged updater, rather than prompting after
//! every desktop exit. The job runs a protected copy of asaled, not a script or
//! executable writable by an unprivileged account.
use crate::paths;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
#[cfg(any(target_os = "linux", windows))]
use std::{path::Path, process::Command};

pub fn configure(args: &[&str]) -> Result<u8> {
    let action = args.first().copied().unwrap_or("status");
    let value = |key: &str| args.windows(2).find(|p| p[0] == key).map(|p| p[1]);
    let data = value("--data-dir")
        .map(PathBuf::from)
        .unwrap_or_else(paths::data_dir);
    #[cfg(not(windows))]
    let package = args.contains(&"--system-package");
    let source = paths::find_asaled().context("install the matching daemon first")?;
    let prefix = source.parent().context("invalid daemon path")?;
    #[cfg(target_os = "linux")]
    {
        if action == "status" {
            Command::new("systemctl")
                .args(["status", "asale-update.timer", "--no-pager"])
                .status()?;
            return Ok(0);
        }
        if action == "disable" {
            anyhow::ensure!(
                Command::new("systemctl")
                    .args(["disable", "--now", "asale-update.timer"])
                    .status()?
                    .success(),
                "administrator authorization is required once"
            );
            return Ok(0);
        }
        if action != "enable" {
            bail!("use asale autoupdate enable|disable|status");
        }
        anyhow::ensure!(
            Command::new("id").arg("-u").output()?.stdout == b"0\n",
            "run this setup once as administrator (sudo); ordinary updates will not prompt"
        );
        let directory = Path::new("/usr/local/lib/asale-updater");
        std::fs::create_dir_all(directory)?;
        std::fs::copy(&source, directory.join("asaled"))?;
        let esc = |s: &Path| -> Result<String> {
            let v = s.to_str().context("non-UTF8 path")?;
            anyhow::ensure!(!v.contains(['\n', '\r']), "invalid path");
            Ok(format!(
                "\"{}\"",
                v.replace('\\', "\\\\")
                    .replace('"', "\\\"")
                    .replace('%', "%%")
            ))
        };
        let service=format!("[Unit]\nDescription=Asale unattended update supervisor\n[Service]\nType=oneshot\nExecStart=/usr/local/lib/asale-updater/asaled --update-once {}\nEnvironment={}\nEnvironment={}\nTimeoutStartSec=15min\n",if package {"--system-package"} else {""},esc(Path::new(&format!("ASALE_DATA_DIR={}",data.display())))?,esc(Path::new(&format!("ASALE_UPDATE_PREFIX={}",prefix.display())))?);
        std::fs::write("/etc/systemd/system/asale-update.service", service)?;
        std::fs::write("/etc/systemd/system/asale-update.timer","[Unit]\nDescription=Check Asale updates every ten minutes\n[Timer]\nOnBootSec=2min\nOnUnitInactiveSec=10min\nPersistent=true\n[Install]\nWantedBy=timers.target\n")?;
        anyhow::ensure!(
            Command::new("systemctl")
                .arg("daemon-reload")
                .status()?
                .success(),
            "could not reload systemd"
        );
        anyhow::ensure!(
            Command::new("systemctl")
                .args(["enable", "--now", "asale-update.timer"])
                .status()?
                .success(),
            "could not enable update timer"
        );
        println!("Unattended system updates enabled; administrator authorization will not be requested during updates.");
        return Ok(0);
    }
    #[cfg(windows)]
    {
        let task = "AsaleAutomaticUpdate";
        if action == "status" {
            Command::new("schtasks.exe")
                .args(["/Query", "/TN", task])
                .status()?;
            return Ok(0);
        }
        if action == "disable" {
            anyhow::ensure!(
                Command::new("schtasks.exe")
                    .args(["/Delete", "/TN", task, "/F"])
                    .status()?
                    .success(),
                "could not disable task"
            );
            return Ok(0);
        }
        if action != "enable" {
            bail!("use asale autoupdate enable|disable|status");
        }
        let program_files = PathBuf::from(std::env::var("ProgramFiles")?);
        let protected = program_files.join("AsaleUpdater");
        std::fs::create_dir_all(&protected)
            .context("run this one-time setup in an administrator terminal")?;
        std::fs::copy(&source, protected.join("asaled.exe"))?;
        let q = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
        let desktop = value("--desktop-target").map(PathBuf::from);
        let body = format!(
            "$env:ASALE_DATA_DIR={}; $env:ASALE_UPDATE_PREFIX={}; {}; & {} --update-once",
            q(&data),
            q(prefix),
            desktop
                .as_ref()
                .map(|p| format!("$env:ASALE_UPDATE_DESKTOP={}", q(p)))
                .unwrap_or_default(),
            q(&protected.join("asaled.exe"))
        );
        let script = protected.join("update.ps1");
        std::fs::write(&script, body)?;
        // Install the task only after the protected worker and its configuration
        // exist. The current user's interactive token keeps GUI relaunches in
        // that user's session; /RL HIGHEST is authorized at setup time.
        let command = format!(
            "powershell.exe -NoProfile -ExecutionPolicy Bypass -File \"{}\"",
            script.display()
        );
        anyhow::ensure!(
            Command::new("schtasks.exe")
                .args([
                    "/Create", "/TN", task, "/SC", "MINUTE", "/MO", "10", "/TR", &command, "/RL",
                    "HIGHEST", "/IT", "/F"
                ])
                .status()?
                .success(),
            "run setup once in an administrator terminal"
        );
        return Ok(0);
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    {
        let _ = (action, data, package, prefix);
        bail!("Standalone user installs update automatically; privileged job registration is available on Linux and Windows");
    }
}

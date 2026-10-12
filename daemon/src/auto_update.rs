//! Standalone daemon updates. Download and validate before touching a live
//! executable; a detached supervisor survives the service it is restarting.
use crate::state::AppState;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

const MANIFEST: &str = "https://asale.ai/dl/manifest.json";
#[derive(Deserialize)]
struct Manifest {
    version: String,
    builds: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    id: String,
    name: String,
    url: String,
    #[serde(default)]
    mirror: String,
    sha256: String,
}

pub fn newer(latest: &str, current: &str) -> bool {
    fn parse(v: &str) -> Option<(Vec<u64>, bool)> {
        let (numbers, prerelease) = v
            .trim_start_matches('v')
            .split_once('-')
            .map_or((v.trim_start_matches('v'), false), |(n, _)| (n, true));
        let mut parts: Vec<u64> = numbers
            .split('.')
            .map(str::parse)
            .collect::<std::result::Result<_, _>>()
            .ok()?;
        if parts.len() != 3 {
            return None;
        }
        while parts.last() == Some(&0) {
            parts.pop();
        }
        Some((parts, prerelease))
    }
    match (parse(latest), parse(current)) {
        (Some((a, ar)), Some((b, br))) => a > b || (a == b && br && !ar),
        _ => false,
    }
}

pub fn watch(state: Arc<AppState>, bind: std::net::SocketAddr) {
    if cfg!(debug_assertions) { return; }
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(30)).await;
        loop {
            match state.store.get_setting("auto_update").await {
                Ok(value) if value.as_deref() != Some("0") => {
                    let result = update_once(bind, false).await;
                    let error = result.err().map(|e| format!("{e:#}")).unwrap_or_default();
                    if !error.is_empty() {
                        tracing::warn!("automatic update deferred: {error}");
                    }
                    let _ = state.store.set_setting("auto_update_error", &error).await;
                }
                _ => {}
            }
            tokio::time::sleep(Duration::from_secs(600)).await;
        }
    });
}

async fn download(http: &reqwest::Client, asset: &Asset, directory: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    if asset.name.is_empty()
        || Path::new(&asset.name).file_name().and_then(|n| n.to_str()) != Some(&asset.name)
    {
        bail!("invalid asset name");
    }
    if asset.sha256.len() != 64 || !asset.sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("release asset {} has no valid SHA-256", asset.name);
    }
    let mut last = String::new();
    for url in [&asset.url, &asset.mirror] {
        if !url.starts_with("https://") {
            continue;
        }
        let result = async {
            let bytes = http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            let hash = format!("{:x}", Sha256::digest(&bytes));
            anyhow::ensure!(
                hash.eq_ignore_ascii_case(&asset.sha256),
                "asset hash mismatch"
            );
            let path = directory.join(&asset.name);
            std::fs::write(&path, bytes)?;
            Ok::<_, anyhow::Error>(path)
        }
        .await;
        match result {
            Ok(path) => return Ok(path),
            Err(e) => last = e.to_string(),
        }
    }
    bail!("could not download {}: {last}", asset.name)
}

pub async fn update_once(bind: std::net::SocketAddr, system_package: bool) -> Result<()> {
    let http = asale_client_core::http::plain();
    let token =
        std::fs::read_to_string(PathBuf::from(crate::state::data_dir()).join("daemon.token"))?;
    let local = format!("http://127.0.0.1:{}", bind.port());
    let preference: serde_json::Value = http
        .post(format!("{local}/rpc/get_setting"))
        .header("X-Asale-Token", token.trim())
        .json(&serde_json::json!({"key":"auto_update"}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if preference.as_str() == Some("0") {
        return Ok(());
    }
    let info: serde_json::Value = http
        .post(format!("{local}/rpc/daemon_info"))
        .header("X-Asale-Token", token.trim())
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let daemon_pid = info["pid"]
        .as_u64()
        .and_then(|p| u32::try_from(p).ok())
        .unwrap_or(std::process::id());
    let manifest: Manifest = http
        .get(MANIFEST)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    if !newer(
        &manifest.version,
        info["version"]
            .as_str()
            .unwrap_or(env!("CARGO_PKG_VERSION")),
    ) {
        return Ok(());
    }
    let executable = std::env::current_exe()?.canonicalize()?;
    let update_prefix = std::env::var_os("ASALE_UPDATE_PREFIX").map(PathBuf::from);
    let prefix = update_prefix.as_deref().unwrap_or(
        executable
            .parent()
            .context("invalid executable directory")?,
    );
    let attempt = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let staging_parent = if update_prefix.is_some() {
        executable.parent().context("invalid updater path")?
    } else {
        prefix
    };
    let stage = staging_parent.join(format!(".asale-update-{attempt}"));
    // This checks ACLs and filesystem permissions before a download or restart.
    std::fs::create_dir(&stage).context(
        "installation is protected; enable the system update task once with administrator rights",
    )?;
    let result = async {
        let id = if cfg!(windows) {
            "cli-windows"
        } else if cfg!(target_os = "macos") {
            "cli-mac"
        } else {
            "cli-linux"
        };
        let asset = manifest
            .builds
            .iter()
            .find(|a| a.id == id)
            .context("release has no CLI archive for this platform")?;
        let archive = download(&http, asset, &stage).await?;
        let unpack = stage.join("new");
        std::fs::create_dir(&unpack)?;
        #[cfg(unix)]
        anyhow::ensure!(
            Command::new("tar")
                .arg("-xzf")
                .arg(&archive)
                .arg("-C")
                .arg(&unpack)
                .status()?
                .success(),
            "could not unpack CLI"
        );
        #[cfg(windows)]
        anyhow::ensure!(
            Command::new("powershell.exe")
                .args(["-NoProfile", "-Command"])
                .arg(format!(
                    "Expand-Archive -LiteralPath {} -DestinationPath {}",
                    ps(&archive),
                    ps(&unpack)
                ))
                .status()?
                .success(),
            "could not unpack CLI"
        );
        let cli_name = if cfg!(windows) { "asale.exe" } else { "asale" };
        let daemon_name = if cfg!(windows) {
            "asaled.exe"
        } else {
            "asaled"
        };
        anyhow::ensure!(
            unpack.join(cli_name).is_file() && unpack.join(daemon_name).is_file(),
            "CLI archive is incomplete"
        );
        let version = Command::new(unpack.join(cli_name))
            .arg("--version")
            .output()?;
        anyhow::ensure!(
            version.status.success()
                && String::from_utf8_lossy(&version.stdout).trim()
                    == format!("asale {}", manifest.version.trim_start_matches('v')),
            "staged CLI has the wrong version"
        );
        let package = if system_package && cfg!(target_os = "linux") {
            let asset = manifest
                .builds
                .iter()
                .find(|a| a.id == "linux-deb")
                .context("missing Debian package")?;
            Some(download(&http, asset, &stage).await?)
        } else {
            None
        };
        #[cfg(windows)]
        if std::env::var_os("ASALE_UPDATE_DESKTOP").is_some() {
            let desktop = manifest
                .builds
                .iter()
                .find(|a| a.id == "windows-portable")
                .context("missing silent Windows desktop asset")?;
            let path = download(&http, desktop, &stage).await?;
            std::fs::copy(path, unpack.join("desktop.exe"))?;
        }
        #[cfg(target_os = "linux")]
        if update_prefix.is_some() && !system_package {
            let status = Command::new("systemctl")
                .args(["is-active", "--quiet", "asaled.service"])
                .status()?;
            anyhow::ensure!(
                status.success(),
                "the privileged daemon updater requires a managed asaled.service"
            );
        }
        let script = prepare_supervisor(
            &stage,
            prefix,
            &unpack,
            bind,
            &manifest.version,
            package.as_deref(),
            daemon_pid,
            Path::new(&crate::state::data_dir()),
        )?;
        launch_supervisor(&script)?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}

#[cfg(unix)]
fn quote(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "'\\''"))
}
#[cfg_attr(not(windows), allow(dead_code))]
fn ps(p: &Path) -> String {
    format!("'{}'", p.to_string_lossy().replace('\'', "''"))
}

#[cfg(unix)]
fn prepare_supervisor(
    stage: &Path,
    prefix: &Path,
    unpack: &Path,
    bind: std::net::SocketAddr,
    version: &str,
    package: Option<&Path>,
    daemon_pid: u32,
    data_dir: &Path,
) -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let unit = if package.is_some() || std::env::var_os("ASALE_UPDATE_PREFIX").is_some() {
        let out = Command::new("systemctl")
            .args(["show", "--property=MainPID", "--value", "asaled.service"])
            .output()?;
        if String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .unwrap_or(0)
            > 0
        {
            Some(("asaled.service".into(), false))
        } else {
            None
        }
    } else {
        service_unit()
    };
    let package_only = package.is_some() && unit.is_none();
    let pid = if let Some((unit, user)) = &unit {
        let mut cmd = Command::new("systemctl");
        if *user {
            cmd.arg("--user");
        }
        let out = cmd
            .args(["show", "--property=MainPID", "--value", unit])
            .output()?;
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse::<u32>()
            .context("could not identify running daemon")?
    } else {
        daemon_pid
    };
    let restart = if package_only {
        ":".into()
    } else {
        match &unit {
            Some((unit, user)) => format!(
                "systemctl {} restart {}",
                if *user { "--user" } else { "" },
                quote(Path::new(unit))
            ),
            None => format!(
                "nohup {} --bind {} >>{} 2>&1 </dev/null &",
                quote(&prefix.join("asaled")),
                bind,
                quote(&data_dir.join("asaled.log"))
            ),
        }
    };
    #[cfg(target_os = "macos")]
    let restart = {
        let agent = PathBuf::from(std::env::var("HOME")?)
            .join("Library/LaunchAgents/ai.asale.asaled.plist");
        if agent.exists() {
            format!("launchctl load -w {}", quote(&agent))
        } else {
            restart
        }
    };
    let stop = if package_only {
        ":".into()
    } else {
        match &unit {
            Some((unit, user)) => format!(
                "systemctl {} stop {}",
                if *user { "--user" } else { "" },
                quote(Path::new(unit))
            ),
            None => format!("kill -TERM {pid} 2>/dev/null || true"),
        }
    };
    #[cfg(target_os = "macos")]
    let stop = {
        let agent = PathBuf::from(std::env::var("HOME")?)
            .join("Library/LaunchAgents/ai.asale.asaled.plist");
        if agent.exists() {
            format!("launchctl unload -w {}", quote(&agent))
        } else {
            stop
        }
    };
    // A system package uses dpkg under a preauthorized root task. Never invoke
    // sudo with a prompt after the service stops.
    let package_install = if let Some(package) = package {
        anyhow::ensure!(
            Command::new("id").arg("-u").output()?.stdout == b"0\n",
            "system package update requires the authorized root update task"
        );
        let dry_run = Command::new("dpkg")
            .args(["--dry-run", "--install"])
            .arg(package)
            .status()?;
        anyhow::ensure!(
            dry_run.success(),
            "package preflight failed; current service is untouched"
        );
        format!(
            "DEBIAN_FRONTEND=noninteractive dpkg --install {} || return 1",
            quote(package)
        )
    } else {
        String::new()
    };
    let package_rollback = if package.is_some() {
        backup_deb(stage)?;
        format!(
            "DEBIAN_FRONTEND=noninteractive dpkg --install {} || true",
            quote(&stage.join("previous.deb"))
        )
    } else {
        String::new()
    };
    let version = version.trim_start_matches('v');
    let worker = Path::new("/usr/local/lib/asale-updater/asaled");
    let is_worker = std::env::current_exe()?.canonicalize()? == worker;
    let worker_prepare = if is_worker {
        format!(
            "cp {}/asaled /usr/local/lib/asale-updater/asaled.new || return 1",
            quote(unpack)
        )
    } else {
        String::new()
    };
    let worker_finish = if is_worker {
        "mv -f /usr/local/lib/asale-updater/asaled.new /usr/local/lib/asale-updater/asaled"
    } else {
        ":"
    };
    let prefix = quote(prefix);
    let unpack = quote(unpack);
    let backup = quote(&stage.join("backup"));
    let url = format!("http://127.0.0.1:{}/healthz", bind.port());
    let body = format!(
        r#"#!/bin/sh
set -u
exec >>{log} 2>&1
export ASALE_DATA_DIR={data}
lock={lock}
if ! mkdir "$lock" 2>/dev/null; then
  owner=$(cat "$lock/owner" 2>/dev/null || true)
  case "$owner" in ''|*[!0-9]*) exit 1 ;; esac
  kill -0 "$owner" 2>/dev/null && exit 1
  rm -f "$lock/owner"; rmdir "$lock" || exit 1
  mkdir "$lock" || exit 1
fi
printf '%s\n' "$$" > "$lock/owner"
cleanup() {{ rm -f "$lock/owner"; rmdir "$lock"; }}
trap cleanup EXIT
mkdir -p {backup} || exit 1
# Copy before stopping; rollback requires both old executables.
for name in asale asaled; do cp -p {prefix}/$name {backup}/$name || exit 1; done
{stop} || exit 1
n=0
while [ "{package_only}" != "true" ] && kill -0 {pid} 2>/dev/null; do
  [ "$n" -lt 60 ] || {{
    {restart}
    exit 1
  }}
  sleep 1; n=$((n+1))
done
rollback() {{
  {package_rollback}
  for name in asale asaled; do cp -p {backup}/$name {prefix}/.$name.restore && mv -f {prefix}/.$name.restore {prefix}/$name; done
  {restart}
}}
apply() {{
  {package_install}
  {worker_prepare}
  for name in asale asaled; do mv -f {unpack}/$name {prefix}/$name || return 1; done
}}
if ! apply; then rollback; exit 1; fi
# Keep the protected worker current without overwriting a mapped executable.
{worker_finish}
[ "{package_only}" != "true" ] || exit 0
{restart}
new_pid=${{!:-}}
n=0
while [ "$n" -lt 60 ]; do
  if curl -fsS --max-time 2 '{url}' | tr -d ' ' | grep -F '"version":"{version}"' >/dev/null; then exit 0; fi
  sleep 1; n=$((n+1))
done
{stop} || true
[ -z "${{new_pid:-}}" ] || kill -TERM "$new_pid" 2>/dev/null || true
rollback
"#,
        log = quote(&stage.join("update.log")),
        data = quote(data_dir),
        lock = quote(
            &stage
                .parent()
                .context("invalid staging path")?
                .join(".asale-update-in-progress")
        )
    );
    let path = stage.join("supervisor.sh");
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

#[cfg(unix)]
fn backup_deb(stage: &Path) -> Result<()> {
    // Reconstruct the installed package so rollback restores dpkg metadata and
    // desktop files as well as the daemon binaries.
    let tree = stage.join("old-package");
    std::fs::create_dir_all(tree.join("DEBIAN"))?;
    let status = Command::new("dpkg-query").args(["-s", "asale"]).output()?;
    anyhow::ensure!(
        status.status.success(),
        "could not read installed package metadata"
    );
    let control = String::from_utf8(status.stdout)?
        .lines()
        .filter(|line| {
            !line.starts_with("Status:")
                && !line.starts_with("Conffiles:")
                && !line.starts_with(' ')
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(tree.join("DEBIAN/control"), control)?;
    let paths = Command::new("dpkg-query").args(["-L", "asale"]).output()?;
    anyhow::ensure!(
        paths.status.success(),
        "could not list installed package files"
    );
    for name in String::from_utf8(paths.stdout)?.lines() {
        let path = Path::new(name);
        if name == "/." || !path.is_absolute() || !path.exists() {
            continue;
        }
        let destination = tree.join(name.trim_start_matches('/'));
        if path.is_dir() {
            std::fs::create_dir_all(destination)?;
        } else {
            std::fs::create_dir_all(destination.parent().context("invalid package file")?)?;
            anyhow::ensure!(
                Command::new("cp")
                    .args(["-a", name])
                    .arg(destination)
                    .status()?
                    .success(),
                "could not back up package file"
            );
        }
    }
    for suffix in ["preinst", "postinst", "prerm", "postrm", "conffiles"] {
        let source = PathBuf::from(format!("/var/lib/dpkg/info/asale.{suffix}"));
        if source.exists() {
            std::fs::copy(source, tree.join("DEBIAN").join(suffix))?;
        }
    }
    anyhow::ensure!(
        Command::new("dpkg-deb")
            .args(["--build", "--root-owner-group"])
            .arg(&tree)
            .arg(stage.join("previous.deb"))
            .status()?
            .success(),
        "could not prepare package rollback"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn service_unit() -> Option<(String, bool)> {
    let raw = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let unit = raw
        .rsplit('/')
        .find(|part| part.trim().ends_with(".service"))?
        .trim()
        .to_string();
    if unit.starts_with("user@") { return None; }
    let user = raw.contains("user.slice");
    Some((unit, user))
}
#[cfg(not(target_os = "linux"))]
fn service_unit() -> Option<(String, bool)> {
    None
}

#[cfg(unix)]
fn launch_supervisor(script: &Path) -> Result<()> {
    if let Some((_, user)) = service_unit() {
        let mut cmd = Command::new("systemd-run");
        if user {
            cmd.arg("--user");
        }
        let result = cmd
            .args(["--quiet", "--collect", "--no-block"])
            .arg("/bin/sh")
            .arg(script)
            .status()?;
        anyhow::ensure!(
            result.success(),
            "could not start independent update supervisor; current service continues"
        );
    } else {
        #[cfg(target_os = "macos")]
        {
            let label = format!("ai.asale.update.{}", std::process::id());
            anyhow::ensure!(
                Command::new("launchctl")
                    .args(["submit", "-l", &label, "--", "/bin/sh"])
                    .arg(script)
                    .status()?
                    .success(),
                "could not launch independent supervisor"
            );
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        let result = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "nohup /bin/sh {} >/dev/null 2>&1 </dev/null &",
                quote(script)
            ))
            .status()?;
        #[cfg(not(target_os = "macos"))]
        anyhow::ensure!(result.success(), "could not launch update supervisor");
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_supervisor(
    stage: &Path,
    prefix: &Path,
    unpack: &Path,
    bind: std::net::SocketAddr,
    version: &str,
    _: Option<&Path>,
    daemon_pid: u32,
    data_dir: &Path,
) -> Result<PathBuf> {
    prepare_windows_supervisor(stage, prefix, unpack, bind, version, daemon_pid, data_dir)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn prepare_windows_supervisor(
    stage: &Path,
    prefix: &Path,
    unpack: &Path,
    bind: std::net::SocketAddr,
    version: &str,
    daemon_pid: u32,
    data_dir: &Path,
) -> Result<PathBuf> {
    let desktop = std::env::var_os("ASALE_UPDATE_DESKTOP").map(PathBuf::from);
    let desktop_config = desktop
        .as_ref()
        .map(|p| {
            format!(
                "$desktop={}; $desktopBackup={}",
                ps(p),
                ps(&stage.join("desktop.previous.exe"))
            )
        })
        .unwrap_or_else(|| "$desktop=$null".into());
    let launch = desktop.as_ref().map(|p| format!("Start-Process {}",ps(p))).unwrap_or_else(||format!("Start-Process (Join-Path $prefix 'asaled.exe') -ArgumentList @('--bind','{bind}') -WindowStyle Hidden"));
    let worker = std::env::current_exe()?;
    let refresh_worker =
        std::env::var_os("ASALE_UPDATE_PREFIX").is_some() && worker.parent() != Some(prefix);
    let worker_stage = if refresh_worker {
        format!(
            "Copy-Item (Join-Path $unpack 'asaled.exe') {} -Force",
            ps(&worker.with_extension("new.exe"))
        )
    } else {
        String::new()
    };
    let worker_finish = if refresh_worker {
        format!("try {{ Wait-Process -Id {} -Timeout 60 -ErrorAction SilentlyContinue }} catch {{}}; Move-Item {} {} -Force",std::process::id(),ps(&worker.with_extension("new.exe")),ps(&worker))
    } else {
        String::new()
    };
    let body = format!(
        r#"$ErrorActionPreference='Stop'
{desktop_config}
$env:ASALE_DATA_DIR={data}
$oldPid={pid}
$prefix={prefix}; $unpack={unpack}; $backup={backup}
New-Item -ItemType Directory -Force $backup | Out-Null
$lease=[IO.File]::Open((Join-Path $prefix '.asale-update.lock'),[IO.FileMode]::OpenOrCreate,[IO.FileAccess]::ReadWrite,[IO.FileShare]::None)
{worker_stage}
foreach($n in @('asale.exe','asaled.exe')) {{ Copy-Item (Join-Path $prefix $n) (Join-Path $backup $n) -Force }}
if($desktop) {{ Copy-Item $desktop $desktopBackup -Force }}
$expected=if($desktop) {{$desktop}} else {{Join-Path $prefix 'asaled.exe'}}
$running=Get-Process -Id $oldPid -ErrorAction Stop
$expected=$expected.Replace('\\?\','')
$daemonPath=(Join-Path $prefix 'asaled.exe').Replace('\\?\','')
if($running.Path -ne $expected -and $running.Path -ne $daemonPath) {{ throw 'daemon process identity does not match the installation' }}
$service=Get-CimInstance Win32_Service -Filter "ProcessId=$oldPid" -ErrorAction Stop
if($desktop) {{ Get-Process -ErrorAction SilentlyContinue | Where-Object {{$_.Path -eq $desktop}} | Stop-Process -Force }}
if($service) {{ Stop-Service -Name $service.Name -ErrorAction Stop }} else {{ Stop-Process -Id $oldPid -ErrorAction SilentlyContinue }}
try {{ Wait-Process -Id $oldPid -Timeout 60 -ErrorAction Stop }} catch {{ if(Get-Process -Id $oldPid -ErrorAction SilentlyContinue) {{ exit 1 }} }}
try {{
 foreach($n in @('asale.exe','asaled.exe')) {{ Move-Item (Join-Path $unpack $n) (Join-Path $prefix $n) -Force }}
 if($desktop) {{ Move-Item (Join-Path $unpack 'desktop.exe') $desktop -Force }}
 if($service) {{ Start-Service -Name $service.Name -ErrorAction Stop }} else {{ {launch} }}
 if($desktop -and $service) {{ (New-Object -ComObject Shell.Application).ShellExecute($desktop,'','','open',1) }}
 for($i=0;$i -lt 60;$i++) {{
  Start-Sleep -Seconds 1
  try {{ $health=Invoke-RestMethod 'http://127.0.0.1:{port}/healthz'; if($health.version -eq '{version}') {{ {worker_finish}; exit 0 }} }} catch {{}}
 }}
 throw 'new daemon failed its health check'
}} catch {{
 if($service) {{ Stop-Service -Name $service.Name -ErrorAction SilentlyContinue }}
 Get-Process asaled -ErrorAction SilentlyContinue | Where-Object {{$_.Path -eq (Join-Path $prefix 'asaled.exe')}} | Stop-Process -Force
 if($desktop) {{ Get-Process -ErrorAction SilentlyContinue | Where-Object {{$_.Path -eq $desktop}} | Stop-Process -Force; Copy-Item $desktopBackup $desktop -Force }}
 foreach($n in @('asale.exe','asaled.exe')) {{ Copy-Item (Join-Path $backup $n) (Join-Path $prefix $n) -Force }}
 if($service) {{ Start-Service -Name $service.Name -ErrorAction Stop }} else {{ {launch} }}
 exit 1
}}
"#,
        pid = daemon_pid,
        prefix = ps(prefix),
        unpack = ps(unpack),
        backup = ps(&stage.join("backup")),
        port = bind.port(),
        data = ps(data_dir)
    );
    let script = stage.join("supervisor.ps1");
    std::fs::write(&script, body)?;
    Ok(script)
}
#[cfg(windows)]
fn launch_supervisor(script: &Path) -> Result<()> {
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(script)
        .creation_flags(0x00000008 | 0x00000200)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_windows_supervisor_can_be_parsed_independently() {
        let directory = std::env::temp_dir().join("asale-windows-supervisor-validation");
        std::fs::create_dir_all(&directory).unwrap();
        let script = super::prepare_windows_supervisor(
            &directory,
            &directory.join("cli"),
            &directory.join("new"),
            "127.0.0.1:9700".parse().unwrap(),
            "1.2.0",
            u32::MAX,
            &directory.join("data"),
        )
        .unwrap();
        let body = std::fs::read_to_string(script).unwrap();
        assert!(body.find("process identity").unwrap() < body.find("Stop-Process -Id").unwrap());
        assert!(body.contains("health.version"));
        assert!(body.contains("Copy-Item (Join-Path $backup $n)"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn supervisor_restores_old_binaries_when_the_new_daemon_fails_health_check() {
        use std::os::unix::fs::PermissionsExt;
        // Avoid a developer launching tests from inside a production service.
        assert!(
            super::service_unit().is_none(),
            "run this test outside a systemd service"
        );
        let directory =
            std::env::temp_dir().join(format!("asale-supervisor-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        let prefix = directory.join("bin");
        let stage = directory.join("stage");
        let unpack = stage.join("new");
        let tools = directory.join("tools");
        for p in [&prefix, &unpack, &tools, &directory.join("data")] {
            std::fs::create_dir_all(p).unwrap();
        }
        for name in ["asale", "asaled"] {
            for (p, version) in [(&prefix, "old"), (&unpack, "new")] {
                let file = p.join(name);
                std::fs::write(&file, format!("#!/bin/sh\n# {version}\nexit 0\n")).unwrap();
                std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        for (name, body) in [
            ("curl", "#!/bin/sh\necho '{\"version\":\"wrong\"}'\n"),
            ("sleep", "#!/bin/sh\nexit 0\n"),
        ] {
            let file = tools.join(name);
            std::fs::write(&file, body).unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let script = super::prepare_supervisor(
            &stage,
            &prefix,
            &unpack,
            "127.0.0.1:9700".parse().unwrap(),
            "1.2.0",
            None,
            99999999,
            &directory.join("data"),
        )
        .unwrap();
        let status = std::process::Command::new("/bin/sh")
            .arg(script)
            .env(
                "PATH",
                format!("{}:{}", tools.display(), std::env::var("PATH").unwrap()),
            )
            .status()
            .unwrap();
        assert!(status.success());
        for name in ["asale", "asaled"] {
            assert!(std::fs::read_to_string(prefix.join(name))
                .unwrap()
                .contains("# old"));
        }
        assert!(!directory.join(".asale-update-in-progress").exists());
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn version_order() {
        assert!(super::newer("1.2.0", "1.1.9"));
        assert!(!super::newer("1.1.6", "1.1.6"));
        assert!(!super::newer("1.1.6", "1.2.0"));
        assert!(!super::newer("bogus", "1.1.6"));
    }
}

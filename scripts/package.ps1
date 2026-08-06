<#
.SYNOPSIS
    客户端打包（Windows）—— 产出 .msi 与 .exe(NSIS) 安装包。

.DESCRIPTION
    与 package.sh 一一对应的 Windows 版本。Tauri 不能跨系统打包，Windows 安装包
    只能在 Windows 上出，所以三平台要么三台机器，要么 CI 三个 job
    （见 .github/workflows/release.yml）。

    编译期注入的值全部来自 .\.env.package（见 .env.package.example）：
      ASALE_QUOTA_PUBKEY  —— 缺了客户端拒绝上市卖出（唯一的经济保护）
      ASALE_SERVER_API 等 —— 装机后没有 shell 环境，地址必须编进二进制

    除安装包外还会出一份命令行归档 asale-cli-<版本>-windows-x86_64.zip，内含
    asale.exe（start/stop/status、开机自启）与 asaled.exe（服务本体，自带内嵌
    Web UI）。安装脚本 https://asale.ai/dl/install.ps1 取的就是这份。

    前置：Rust (rustup)、Node 20+、pnpm、Visual Studio Build Tools（MSVC + Windows SDK）、
          WebView2 Runtime（Win11 自带）。

    要出能给外部用户装的包，还得配 Authenticode 代码签名（见下面“代码签名”一节），
    否则 SmartScreen 会拦“未知发布者”。凭据齐了才签，缺了只是警告 —— 本地试打不受影响。
    额外前置：cargo install artifact-signing-cli、.NET 8、Azure CLI、Windows SDK 的 signtool。

.EXAMPLE
    pwsh scripts/package.ps1
    pwsh scripts/package.ps1 -Bundles msi
    pwsh scripts/package.ps1 -NoSign -Debug
    pwsh scripts/package.ps1 -CliOnly      # 只出命令行归档
#>
# 不要加 [CmdletBinding()]：那会给脚本自动带上 -Debug 等通用参数，和下面自己声明的
# -Debug 撞名，PowerShell 直接拒绝执行（MetadataError: A parameter with the name
# 'Debug' was defined multiple times）。这里保留 -Debug 是为了和 package.sh 的 --debug 对齐。
param(
    [string]$Target  = "",              # 例：x86_64-pc-windows-msvc / aarch64-pc-windows-msvc
    [string]$Bundles = "msi,nsis",
    [switch]$NoSign,                    # 不签更新包（本地试打）
    [switch]$Debug,                     # debug 构建，快很多
    [switch]$CliOnly,                   # 只出命令行归档，不打安装包
    [switch]$NoCli                      # 只打安装包，跳过命令行归档
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# asale-client 根目录
$ClientDir  = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $ClientDir

$EnvFile    = Join-Path $ClientDir ".env.package"
$UpdaterKey = Join-Path $ClientDir "asale-updater.key"

function Step($msg) { Write-Host ""; Write-Host "==> $msg" -ForegroundColor Cyan }
function Die($msg)  { Write-Host "!! $msg" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------- 打包参数
if (-not (Test-Path $EnvFile)) {
    Die "缺少 $EnvFile：copy .env.package.example .env.package 后填值"
}
# KEY=VALUE 逐行读进本进程环境；# 开头是注释，值里允许有 =。
Get-Content $EnvFile | ForEach-Object {
    $line = $_.Trim()
    if ($line -and -not $line.StartsWith("#") -and $line.Contains("=")) {
        $k, $v = $line.Split("=", 2)
        [Environment]::SetEnvironmentVariable($k.Trim(), $v.Trim(), "Process")
    }
}

if (-not $env:ASALE_QUOTA_PUBKEY) {
    Die @"
ASALE_QUOTA_PUBKEY 为空。
   没有它，打出来的客户端会拒绝上市卖出 —— 无法验证网关授权，就等于把自己的
   订阅额度交给任何自称是网关的对端。
   取值：网关启动日志里的 'quota pubkey' 那一行。
"@
}

foreach ($name in @("ASALE_SERVER_API", "ASALE_GATEWAY_API", "ASALE_GATEWAY_WS")) {
    $val = [Environment]::GetEnvironmentVariable($name, "Process")
    if (-not $val) { Die "$name 为空（见 $EnvFile）" }
    # 客户端运行时同样会校验并拒绝明文远端；提前挡一次，免得装到用户机器上才发现。
    if ($val -notmatch '^(https|wss)://') {
        if ($val -match '127\.0\.0\.1|localhost|\[::1\]') {
            Write-Host "   注意：$name=$val 指向本机，只适合自测包" -ForegroundColor Yellow
        } else {
            Die "$name=$val 是明文且非本机地址，客户端会拒绝连接。生产必须 https:// / wss://"
        }
    }
}

# ---------------------------------------------------------------- 更新签名
# tauri.conf.json 里 createUpdaterArtifacts=true：每个产物都要出 .sig，
# 没有私钥就直接构建失败，所以要么给钥匙，要么显式 -NoSign。
# -CliOnly 不经过 tauri bundler，也就没有 .sig 这回事。
if ($CliOnly) { $NoSign = [switch]$true }
if (-not $NoSign) {
    if (-not (Test-Path $UpdaterKey)) { Die "找不到 $UpdaterKey（updater 签名私钥）。本地试打可加 -NoSign" }
    $env:TAURI_SIGNING_PRIVATE_KEY = Get-Content $UpdaterKey -Raw
    if (-not $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD) { $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = "" }
} else {
    Write-Host "   -NoSign：产物不带 .sig，自动更新用不了（仅供本地验证）" -ForegroundColor Yellow
}

# ---------------------------------------------------------------- 依赖
Step "环境检查"
foreach ($cmd in @("cargo", "pnpm", "node")) {
    if (-not (Get-Command $cmd -ErrorAction SilentlyContinue)) {
        Die "找不到 $cmd。需要 Rust(rustup.rs) + Node 20+ + pnpm(npm i -g pnpm)"
    }
}
if ($Target) { rustup target add $Target | Out-Null }

# ---------------------------------------------------------------- 代码签名
# 没有 Authenticode 签名，SmartScreen 会对安装包弹“未知发布者 / Windows 已保护你的
# 电脑”。这跟 updater 的 .sig 是两码事：那个是 minisign，只管自动更新校验。
#
# 走 Azure Artifact Signing（旧名 Trusted Signing）：私钥在微软的 HSM 里，构建机
# 只拿一个 App Registration 的 client secret，没有 .pfx 可泄漏，也不用往证书存储里
# 导东西。签名由 bundle.windows.signCommand 调 artifact-signing-cli 完成，tauri 会
# 对 Asale.exe、.msi、-setup.exe 各调一次。
#
# 为什么不直接写进 tauri.conf.json：signCommand 一旦落在配置里，任何没有 Azure 凭据
# 的机器一构建就在签名这步失败，本地试打全废。所以按环境变量存在与否，用
# `tauri build --config` 动态合并进去。值可以放 .env.package，也可以由 CI 注入。
$SignVars = @(
    "AZURE_ARTIFACT_SIGNING_ENDPOINT"             # 例 https://wus2.codesigning.azure.net，按账号所在区域
    "AZURE_ARTIFACT_SIGNING_ACCOUNT"
    "AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE"
    "AZURE_CLIENT_ID"
    "AZURE_CLIENT_SECRET"
    "AZURE_TENANT_ID"
)
$SignSet     = @($SignVars | Where-Object { [Environment]::GetEnvironmentVariable($_, "Process") })
$SignMissing = @($SignVars | Where-Object { -not [Environment]::GetEnvironmentVariable($_, "Process") })
$SignArgs    = @()

if ($NoSign) {
    # -NoSign 一律理解成“什么都不签”：updater 的 .sig 和 Authenticode 一起关掉。
    # tauri 自己的 --no-sign 到底覆盖哪几种签名，官方措辞含糊（帮助文本写的是
    # "skip code signing"），不去依赖它的内部行为，这里显式不注入 signCommand。
    if ($SignSet.Count -gt 0) {
        Write-Host "   -NoSign：跳过 Authenticode 签名（凭据已配但不用）" -ForegroundColor Yellow
    }
} elseif ($SignSet.Count -eq 0) {
    Write-Host "   注意：没有配 Authenticode 代码签名，安装包会触发 SmartScreen 警告" -ForegroundColor Yellow
} elseif ($SignMissing.Count -gt 0) {
    # 半套凭据比没有更危险：包会照样出，只是没签名，而发布流程会以为签了。
    Die @"
Azure 代码签名凭据不全，缺：$($SignMissing -join ', ')
   要么六个都给，要么六个都不给（不给 = 不签名，安装包会触发 SmartScreen 警告）。
"@
} else {
    if (-not (Get-Command artifact-signing-cli -ErrorAction SilentlyContinue)) {
        Die "找不到 artifact-signing-cli，装它：cargo install artifact-signing-cli"
    }

    # artifact-signing-cli 把这两个路径写死在默认值里（signtool 甚至写死了 SDK 版本
    # 10.0.26100.0），装的 SDK 不是那个版本就直接失败。它认 SIGNTOOL_PATH /
    # AZURE_CLI_PATH 两个环境变量，所以这里替它找。
    if (-not $env:SIGNTOOL_PATH) {
        $kits = "${env:ProgramFiles(x86)}\Windows Kits\10\bin"
        $signtool = $null
        if (Test-Path $kits) {
            $signtool = Get-ChildItem $kits -Directory |
                Sort-Object Name -Descending |
                ForEach-Object { Join-Path $_.FullName "x64\signtool.exe" } |
                Where-Object { Test-Path $_ } |
                Select-Object -First 1
        }
        if (-not $signtool) {
            Die "找不到 signtool.exe（$kits\<版本>\x64）。装 Windows SDK，或显式给 SIGNTOOL_PATH"
        }
        $env:SIGNTOOL_PATH = $signtool
    }
    if (-not $env:AZURE_CLI_PATH) {
        $az = Get-Command az -ErrorAction SilentlyContinue
        if (-not $az) { Die "找不到 Azure CLI（az）。artifact-signing-cli 需要它取 token" }
        $env:AZURE_CLI_PATH = $az.Source
    }

    # 有需要时可以加 -i：让 CLI 跳过它签不了的文件而不是整个构建失败（tauri 会把
    # bundle.resources 里的 png 之类也送来签）。默认不加 —— 静默不签名正是这里要
    # 防的事，宁可构建失败。
    $signCommand = @{
        cmd  = "artifact-signing-cli"
        args = @(
            "-e", $env:AZURE_ARTIFACT_SIGNING_ENDPOINT
            "-a", $env:AZURE_ARTIFACT_SIGNING_ACCOUNT
            "-c", $env:AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE
            "-d", "Asale"                       # UAC 提示里显示的名字，不给就是一串随机字符
            "%1"                                # tauri 替换成待签文件的路径
        )
    }
    # 写成临时文件再传路径，不直接把 JSON 当参数：Windows PowerShell 5.1 往原生程序
    # 传含双引号的参数会丢引号，tauri 那边就是个 JSON 解析错误。--config 两种都收。
    # 必须无 BOM 地写：Set-Content -Encoding utf8 在 5.1 下带 BOM，serde_json 读不了。
    $SignConfig = Join-Path ([System.IO.Path]::GetTempPath()) "asale-sign-config.json"
    $SignJson = @{ bundle = @{ windows = @{ signCommand = $signCommand } } } | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText($SignConfig, $SignJson, [Text.UTF8Encoding]::new($false))
    $SignArgs = @("--config", $SignConfig)

    Write-Host ("   Authenticode：Azure Artifact Signing（{0} / {1}）" -f `
        $env:AZURE_ARTIFACT_SIGNING_ACCOUNT, $env:AZURE_ARTIFACT_SIGNING_CERTIFICATE_PROFILE) -ForegroundColor Green
}

# ---------------------------------------------------------------- 构建
Step "安装前端依赖"
pnpm install --frozen-lockfile
if ($LASTEXITCODE -ne 0) { Die "pnpm install 失败" }

# `tauri build` 出的就是 release，v2 的 CLI 没有 --release 这个参数
# （给了直接报 "unexpected argument"）。只有反过来的 --debug。
Write-Host "    ASALE_SERVER_API=$env:ASALE_SERVER_API"
Write-Host "    ASALE_GATEWAY_API=$env:ASALE_GATEWAY_API"
Write-Host "    ASALE_GATEWAY_WS=$env:ASALE_GATEWAY_WS"
Write-Host ("    ASALE_QUOTA_PUBKEY={0}…" -f $env:ASALE_QUOTA_PUBKEY.Substring(0, [Math]::Min(16, $env:ASALE_QUOTA_PUBKEY.Length)))

if (-not $CliOnly) {
    $tauriArgs = @("tauri", "build")
    # 光是不设私钥还不够：tauri.conf.json 里有 pubkey，bundler 会认定"配了公钥却
    # 没私钥"直接报错。要跳过就得明说。
    if ($NoSign) { $tauriArgs += "--no-sign" }
    if ($Debug) { $tauriArgs += "--debug" }
    if ($Target)  { $tauriArgs += @("--target", $Target) }
    if ($Bundles) { $tauriArgs += @("--bundles", $Bundles) }
    if ($SignArgs.Count) { $tauriArgs += $SignArgs }

    Step "构建安装包：pnpm $($tauriArgs -join ' ')"
    pnpm @tauriArgs
    if ($LASTEXITCODE -ne 0) { Die "构建失败" }
} else {
    # asaled 用 rust-embed 把 ../dist 编进二进制（无桌面模式的 Web UI 就是它），
    # 不先构建前端，出来的 asaled 只会回一句“没有内嵌 UI”。
    Step "构建前端（asaled 内嵌的 Web UI）"
    pnpm build
    if ($LASTEXITCODE -ne 0) { Die "前端构建失败" }
}

# 四个 crate 是一个 workspace，target 在 asale-client 根下，不在 src-tauri 里。
$out = Join-Path $ClientDir "target"
if ($Target) { $out = Join-Path $out $Target }
$profileDir = $(if ($Debug) { "debug" } else { "release" })
$out = Join-Path (Join-Path $out $profileDir) "bundle"

# ------------------------------------------------------- 命令行 / 服务产物
# asale.exe（命令行）+ asaled.exe（服务本体，自带内嵌 Web UI）。装了它，终端里
# 就有 asale start/stop/status，以及 `asale autostart enable` 写登录启动项。
# 归档名要和 asale-web 的 src/lib/downloads.ts 里的正则对齐。
if (-not $NoCli) {
    $version = (Select-String -Path (Join-Path $ClientDir "Cargo.toml") -Pattern '^version = "([^"]+)"' |
        Select-Object -First 1).Matches.Groups[1].Value
    if (-not $version) { Die "读不到版本号（Cargo.toml 的 [workspace.package] version）" }

    Step "构建命令行工具（asale + asaled）"
    # bin 名是 asale-cli 不是 asale：workspace 里 src-tauri 的桌面壳二进制已经叫
    # asale，两个 bin 写同一个 target 路径会互相覆盖（见 cli/Cargo.toml 的说明）。
    # 归档里再改回 asale.exe —— 那才是用户敲的命令。
    $cargoArgs = @("build", "-p", "asale-cli", "-p", "asale-daemon", "--bin", "asale-cli", "--bin", "asaled")
    if (-not $Debug) { $cargoArgs += "--release" }
    if ($Target) { $cargoArgs += @("--target", $Target) }
    cargo @cargoArgs
    if ($LASTEXITCODE -ne 0) { Die "命令行工具构建失败" }

    $binDir = Join-Path $ClientDir "target"
    if ($Target) { $binDir = Join-Path $binDir $Target }
    $binDir = Join-Path $binDir $profileDir

    $stage = Join-Path $ClientDir "target\asale-cli-stage"
    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    New-Item -ItemType Directory -Force -Path $stage | Out-Null
    Copy-Item (Join-Path $binDir "asale-cli.exe") (Join-Path $stage "asale.exe")
    Copy-Item (Join-Path $binDir "asaled.exe") (Join-Path $stage "asaled.exe")

    # 手动下载归档的人也该知道这两个文件是干什么的。
    @"
Asale client — command line install (v$version)

  asaled.exe   the service. Holds all of asale's logic and serves the web UI.
  asale.exe    the command line: start/stop/restart/status, boot registration.

Install by hand: copy both into a folder on your PATH, then

  asale start
  asale url                 the URL to open, access token included
  asale autostart enable    start the service when you sign in

Or let the installer do it:  irm https://asale.ai/dl/install.ps1 | iex

The token in that URL is the whole authorization: anyone holding it can read
your credentials and spend your balance. Do not expose the port to the internet.

Docs: https://asale.ai/   ·   asale help web
"@ | Set-Content -Encoding UTF8 (Join-Path $stage "INSTALL.txt")

    $cliDir = Join-Path $out "cli"
    New-Item -ItemType Directory -Force -Path $cliDir | Out-Null
    $archive = Join-Path $cliDir "asale-cli-$version-windows-x86_64.zip"
    if (Test-Path $archive) { Remove-Item -Force $archive }
    # 归档里就是三个文件，没有目录前缀 —— 安装脚本按固定名字取。
    Compress-Archive -Path (Join-Path $stage "*") -DestinationPath $archive
    Remove-Item -Recurse -Force $stage
    Write-Host "    $archive"
}

# ---------------------------------------------------------------- 产物
Step "产物"
if (Test-Path $out) {
    Get-ChildItem -Path $out -Recurse -Include *.msi, *.exe, *.zip, *.sig |
        ForEach-Object { "{0,10:N0} KB  {1}" -f ($_.Length / 1KB), $_.FullName }
    Write-Host ""
    Write-Host "产物目录：$out"
} else {
    Write-Host "    没找到 $out —— 构建可能只出了可执行文件"
}

<#
.SYNOPSIS
    客户端打包（Windows）—— 产出 .msi 与 .exe(NSIS) 安装包。

.DESCRIPTION
    与 package.sh 一一对应的 Windows 版本。Tauri 不能跨系统打包，Windows 安装包
    只能在 Windows 上出，所以三平台要么三台机器，要么 CI 三个 job
    （见 .github/workflows/release.yml）。

    编译期注入的值全部来自 .\.env（见 .env.example）：
      ASALE_QUOTA_PUBKEY  —— 缺了客户端拒绝上市卖出（唯一的经济保护）
      ASALE_SERVER_API 等 —— 装机后没有 shell 环境，地址必须编进二进制

    前置：Rust (rustup)、Node 20+、pnpm、Visual Studio Build Tools（MSVC + Windows SDK）、
          WebView2 Runtime（Win11 自带）。

.EXAMPLE
    pwsh scripts/package.ps1
    pwsh scripts/package.ps1 -Bundles msi
    pwsh scripts/package.ps1 -NoSign -Debug
#>
# 不要加 [CmdletBinding()]：那会给脚本自动带上 -Debug 等通用参数，和下面自己声明的
# -Debug 撞名，PowerShell 直接拒绝执行（MetadataError: A parameter with the name
# 'Debug' was defined multiple times）。这里保留 -Debug 是为了和 package.sh 的 --debug 对齐。
param(
    [string]$Target  = "",              # 例：x86_64-pc-windows-msvc / aarch64-pc-windows-msvc
    [string]$Bundles = "msi,nsis",
    [switch]$NoSign,                    # 不签更新包（本地试打）
    [switch]$Debug                      # debug 构建，快很多
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

# asale-client 根目录
$ClientDir  = Resolve-Path (Join-Path $PSScriptRoot "..")
Set-Location $ClientDir

$EnvFile    = Join-Path $ClientDir ".env"
$UpdaterKey = Join-Path $ClientDir "asale-updater.key"

function Step($msg) { Write-Host ""; Write-Host "==> $msg" -ForegroundColor Cyan }
function Die($msg)  { Write-Host "!! $msg" -ForegroundColor Red; exit 1 }

# ---------------------------------------------------------------- 打包参数
if (-not (Test-Path $EnvFile)) {
    Die "缺少 $EnvFile：copy .env.example .env 后填值"
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
# 没有代码签名证书时，SmartScreen 会对安装包弹“未知发布者”警告。
# 要消掉得配 Authenticode 证书（tauri.conf.json 的 bundle.windows.certificateThumbprint）。
if (-not $env:TAURI_WINDOWS_CERT_THUMBPRINT) {
    Write-Host "   注意：没有配 Authenticode 代码签名证书，安装包会触发 SmartScreen 警告" -ForegroundColor Yellow
}
if ($Target) { rustup target add $Target | Out-Null }

# ---------------------------------------------------------------- 构建
Step "安装前端依赖"
pnpm install --frozen-lockfile
if ($LASTEXITCODE -ne 0) { Die "pnpm install 失败" }

# `tauri build` 出的就是 release，v2 的 CLI 没有 --release 这个参数
# （给了直接报 "unexpected argument"）。只有反过来的 --debug。
$tauriArgs = @("tauri", "build")
# 光是不设私钥还不够：tauri.conf.json 里有 pubkey，bundler 会认定"配了公钥却
# 没私钥"直接报错。要跳过就得明说。
if ($NoSign) { $tauriArgs += "--no-sign" }
if ($Debug) { $tauriArgs += "--debug" }
if ($Target)  { $tauriArgs += @("--target", $Target) }
if ($Bundles) { $tauriArgs += @("--bundles", $Bundles) }

Step "构建：pnpm $($tauriArgs -join ' ')"
Write-Host "    ASALE_SERVER_API=$env:ASALE_SERVER_API"
Write-Host "    ASALE_GATEWAY_API=$env:ASALE_GATEWAY_API"
Write-Host "    ASALE_GATEWAY_WS=$env:ASALE_GATEWAY_WS"
Write-Host ("    ASALE_QUOTA_PUBKEY={0}…" -f $env:ASALE_QUOTA_PUBKEY.Substring(0, [Math]::Min(16, $env:ASALE_QUOTA_PUBKEY.Length)))
pnpm @tauriArgs
if ($LASTEXITCODE -ne 0) { Die "构建失败" }

# ---------------------------------------------------------------- 产物
Step "产物"
# 三个 crate 是一个 workspace，target 在 asale-client 根下，不在 src-tauri 里。
$out = Join-Path $ClientDir "target"
if ($Target) { $out = Join-Path $out $Target }
$out = Join-Path $out ($(if ($Debug) { "debug" } else { "release" }))
$out = Join-Path $out "bundle"
if (Test-Path $out) {
    Get-ChildItem -Path $out -Recurse -Include *.msi, *.exe, *.zip, *.sig |
        ForEach-Object { "{0,10:N0} KB  {1}" -f ($_.Length / 1KB), $_.FullName }
    Write-Host ""
    Write-Host "产物目录：$out"
} else {
    Write-Host "    没找到 $out —— 构建可能只出了可执行文件"
}

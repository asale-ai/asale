#!/usr/bin/env bash
# 客户端打包（macOS / Linux）
#
#   ./scripts/package.sh                     # 当前系统的默认产物
#   ./scripts/package.sh --universal         # macOS 通用二进制 (arm64 + x86_64)
#   ./scripts/package.sh --target x86_64-apple-darwin
#   ./scripts/package.sh --bundles deb,appimage
#   ./scripts/package.sh --no-sign           # 不签更新包（本地试打）
#   ./scripts/package.sh --debug             # debug 构建，快很多
#
# Windows 用同目录的 package.ps1。
#
# Tauri 不能跨系统打包：.dmg 只能在 macOS 出，.msi 只能在 Windows 出，
# .deb/.AppImage 只能在 Linux 出。三平台要么三台机器，要么 CI 三个 job
# （见 .github/workflows/release.yml）。
#
# 编译期注入的值全部来自 ./.env（见 .env.example）：
#   ASALE_QUOTA_PUBKEY   —— 缺了客户端拒绝上市卖出（唯一的经济保护）
#   ASALE_SERVER_API 等  —— 装机后没有 shell 环境，地址必须编进去
set -euo pipefail

cd "$(dirname "$0")/.."   # asale-client 根目录
ENV_FILE=".env"
UPDATER_KEY="asale-updater.key"

TARGET=""
BUNDLES=""
SIGN=1
# `tauri build` 出的就是 release，v2 的 CLI 没有 --release 这个参数（给了直接报
# "unexpected argument"）。只有反过来的 --debug。
PROFILE=""

for ((i = 1; i <= $#; i++)); do
  arg="${!i}"
  case "$arg" in
    --universal) TARGET="universal-apple-darwin" ;;
    --target)    i=$((i+1)); TARGET="${!i}" ;;
    --bundles)   i=$((i+1)); BUNDLES="${!i}" ;;
    --no-sign)   SIGN=0 ;;
    --debug)     PROFILE="--debug" ;;
    -h|--help)   sed -n '2,19p' "$0"; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

step() { echo; echo "==> $*"; }
die()  { echo "!! $*" >&2; exit 1; }

# ---------------------------------------------------------------- 打包参数
[[ -f "$ENV_FILE" ]] || die "缺少 ${ENV_FILE}：cp .env.example $ENV_FILE 后填值"
set -a; . "./$ENV_FILE"; set +a

[[ -n "${ASALE_QUOTA_PUBKEY:-}" ]] || die "ASALE_QUOTA_PUBKEY 为空（见 ${ENV_FILE}）。
   没有它，打出来的客户端会拒绝上市卖出 —— 无法验证网关授权，就等于把自己的
   订阅额度交给任何自称是网关的对端。
   取值：网关启动日志里的 'quota pubkey' 那一行。"

for v in ASALE_SERVER_API ASALE_GATEWAY_API ASALE_GATEWAY_WS; do
  val="${!v:-}"
  [[ -n "$val" ]] || die "$v 为空（见 ${ENV_FILE}）"
  # 客户端运行时也会做这个校验并直接拒绝启动；在这里先挡一次，
  # 免得打完包装到用户机器上才发现连不上。
  case "$val" in
    https://*|wss://*) ;;
    *127.0.0.1*|*localhost*|*'[::1]'*) echo "   注意：$v=$val 指向本机，只适合自测包" ;;
    *) die "$v=$val 是明文且非本机地址，客户端会拒绝连接。生产必须 https:// / wss://" ;;
  esac
done

# ---------------------------------------------------------------- 更新签名
# createUpdaterArtifacts=true（tauri.conf.json）意味着每个产物都要出一个 .sig，
# 没有私钥就直接构建失败，所以这里要么给钥匙，要么显式 --no-sign。
if [[ $SIGN == 1 ]]; then
  [[ -f "$UPDATER_KEY" ]] || die "找不到 ${UPDATER_KEY}（updater 签名私钥）。本地试打可加 --no-sign"
  export TAURI_SIGNING_PRIVATE_KEY="$(cat "$UPDATER_KEY")"
  export TAURI_SIGNING_PRIVATE_KEY_PASSWORD="${TAURI_SIGNING_PRIVATE_KEY_PASSWORD:-}"
else
  echo "   --no-sign：产物不带 .sig，自动更新用不了（仅供本地验证）"
fi

# ---------------------------------------------------------------- 依赖
os="$(uname -s)"
step "环境检查（${os}）"
command -v cargo >/dev/null || die "需要 Rust 工具链：https://rustup.rs"
command -v pnpm  >/dev/null || die "需要 pnpm：npm i -g pnpm"

if [[ "$os" == "Darwin" ]]; then
  [[ -n "$TARGET" ]] || TARGET="universal-apple-darwin"
  # cargo 的 registry 可以走 ~/.cargo/config.toml 里的镜像，rustup 不看那个文件：
  # 国内网络下装 target 会从 static.rust-lang.org 爬到天亮，给它一个镜像。
  # CI 在境外，workflow 里会把这个变量盖回官方源。
  export RUSTUP_DIST_SERVER="${RUSTUP_DIST_SERVER:-https://rsproxy.cn}"
  if [[ "$TARGET" == "universal-apple-darwin" ]]; then
    rustup target add aarch64-apple-darwin x86_64-apple-darwin >/dev/null
  else
    rustup target add "$TARGET" >/dev/null || true
  fi
  # 未签名/未公证的 .app 在别人机器上会被 Gatekeeper 拦（"已损坏，无法打开"）。
  # 要发给外部用户，得配 APPLE_CERTIFICATE / APPLE_ID / APPLE_TEAM_ID 再走公证。
  [[ -n "${APPLE_CERTIFICATE:-}${APPLE_SIGNING_IDENTITY:-}" ]] \
    || echo "   注意：没有配 Apple 签名证书，产出的 .app/.dmg 未签名未公证，只能自用"
elif [[ "$os" == "Linux" ]]; then
  # Tauri v2 在 Linux 上依赖 webkit2gtk-4.1；缺了会在 cargo build 阶段报
  # pkg-config 找不到，报错信息离真正原因很远，所以提前查一次。
  if command -v pkg-config >/dev/null && ! pkg-config --exists webkit2gtk-4.1; then
    die "缺少 Linux 构建依赖，Debian/Ubuntu 上执行：
   sudo apt-get install -y libwebkit2gtk-4.1-dev build-essential curl wget file \\
        libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf"
  fi
  [[ -n "$BUNDLES" ]] || BUNDLES="deb,appimage"
else
  die "不支持的系统: ${os}（Windows 请用 scripts/package.ps1）"
fi

# ---------------------------------------------------------------- 构建
step "安装前端依赖"
pnpm install --frozen-lockfile

args=(tauri build)
# 光是不 export 私钥还不够：tauri.conf.json 里有 pubkey，bundler 会认定"配了公钥却
# 没私钥"直接报错。要跳过就得明说。
if [[ $SIGN == 0 ]]; then args+=(--no-sign); fi
if [[ -n "$PROFILE" ]]; then args+=("$PROFILE"); fi
if [[ -n "$TARGET"  ]]; then args+=(--target "$TARGET"); fi
if [[ -n "$BUNDLES" ]]; then args+=(--bundles "$BUNDLES"); fi

step "构建：pnpm ${args[*]}"
echo "    ASALE_SERVER_API=$ASALE_SERVER_API"
echo "    ASALE_GATEWAY_API=$ASALE_GATEWAY_API"
echo "    ASALE_GATEWAY_WS=$ASALE_GATEWAY_WS"
echo "    ASALE_QUOTA_PUBKEY=${ASALE_QUOTA_PUBKEY:0:16}…"
pnpm "${args[@]}"

# ---------------------------------------------------------------- 产物
step "产物"
# 三个 crate 是一个 workspace（./Cargo.toml），target 在这里，不在 src-tauri/ 下面。
out="target"
if [[ -n "$TARGET" ]]; then out="$out/$TARGET"; fi
if [[ "$PROFILE" == "--debug" ]]; then out="$out/debug/bundle"; else out="$out/release/bundle"; fi
if [[ -d "$out" ]]; then
  find "$out" -maxdepth 2 -type f \
    \( -name '*.dmg' -o -name '*.app.tar.gz' -o -name '*.deb' -o -name '*.AppImage' -o -name '*.rpm' -o -name '*.sig' \) \
    -exec ls -lh {} \; | awk '{printf "    %-8s %s\n", $5, $NF}'
  echo
  echo "产物目录：$out"
else
  echo "    没找到 $out —— 构建可能只出了可执行文件"
fi

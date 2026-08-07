#!/usr/bin/env bash
# 客户端打包（macOS / Linux）
#
#   ./scripts/package.sh                     # 当前系统的默认产物
#   ./scripts/package.sh --universal         # macOS 通用二进制 (arm64 + x86_64)
#   ./scripts/package.sh --target x86_64-apple-darwin
#   ./scripts/package.sh --bundles deb,appimage
#   ./scripts/package.sh --no-sign           # 跳过代码签名（本地试打）
#   ./scripts/package.sh --debug             # debug 构建，快很多
#   ./scripts/package.sh --cli-only          # 只出命令行/无桌面产物，不打桌面安装包
#   ./scripts/package.sh --no-cli            # 只打桌面安装包，跳过命令行产物
#
# 除桌面安装包外，每次打包还会出一份命令行归档
# （asale-cli-<版本>-<平台>.tar.gz，内含 asale + asaled）。这份归档是无桌面
# Linux 的全部安装内容：asaled 自带内嵌 Web UI，asale 负责起停与开机自启，
# 浏览器访问端口就是完整的客户端。装桌面版的机器也会装它，好让终端里有
# `asale start/stop/status`。
#
# Windows 用同目录的 package.ps1。
#
# Tauri 不能跨系统打包：.dmg 只能在 macOS 出，.msi 只能在 Windows 出，
# .deb/.AppImage 只能在 Linux 出。三平台要么三台机器，要么 CI 三个 job
# （见 .github/workflows/release.yml）。
#
# 编译期注入的值全部来自 ./.env.package（见 .env.package.example）：
#   ASALE_QUOTA_PUBKEY   —— 缺了客户端拒绝上市卖出（唯一的经济保护）
#   ASALE_SERVER_API 等  —— 装机后没有 shell 环境，地址必须编进去
set -euo pipefail

cd "$(dirname "$0")/.."   # asale-client 根目录
ENV_FILE=".env.package"

TARGET=""
BUNDLES=""
SIGN=1
BUILD_APP=1
BUILD_CLI=1
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
    --cli-only)  BUILD_APP=0 ;;
    --no-cli)    BUILD_CLI=0 ;;
    -h|--help)   sed -n '2,26p' "$0"; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

step() { echo; echo "==> $*"; }
die()  { echo "!! $*" >&2; exit 1; }

# ---------------------------------------------------------------- 打包参数
[[ -f "$ENV_FILE" ]] || die "缺少 ${ENV_FILE}：cp .env.package.example $ENV_FILE 后填值"
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

# 更新走安装脚本（asale.ai/dl/install.sh），不是 Tauri 的增量 updater，所以这里
# 没有 minisign 私钥这一环了 —— 曾经的 createUpdaterArtifacts 和 .sig 一并去掉。
# 剩下的 --no-sign 只是透传给 tauri build，意思是"这一趟别做代码签名"。
if [[ $BUILD_APP == 0 ]]; then
  # --cli-only 不经过 tauri bundler，签名这件事无从谈起。
  SIGN=0
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
  #
  # 只在打桌面包时查：命令行产物（asale/asaled）不链接 webkit，这正是无桌面
  # Linux 能只装它们的原因 —— 一台连 X 都没有的服务器也能自己出这份归档。
  if [[ $BUILD_APP == 1 ]] && command -v pkg-config >/dev/null && ! pkg-config --exists webkit2gtk-4.1; then
    die "缺少 Linux 构建依赖，Debian/Ubuntu 上执行：
   sudo apt-get install -y libwebkit2gtk-4.1-dev build-essential curl wget file \\
        libxdo-dev libssl-dev libayatana-appindicator3-dev librsvg2-dev patchelf
   （只想出命令行/无桌面产物的话，加 --cli-only，这些依赖都不需要）"
  fi
  [[ -n "$BUNDLES" ]] || BUNDLES="deb,appimage"
else
  die "不支持的系统: ${os}（Windows 请用 scripts/package.ps1）"
fi

# ---------------------------------------------------------------- 构建
step "安装前端依赖"
pnpm install --frozen-lockfile

echo "    ASALE_SERVER_API=$ASALE_SERVER_API"
echo "    ASALE_GATEWAY_API=$ASALE_GATEWAY_API"
echo "    ASALE_GATEWAY_WS=$ASALE_GATEWAY_WS"
echo "    ASALE_QUOTA_PUBKEY=${ASALE_QUOTA_PUBKEY:0:16}…"

if [[ $BUILD_APP == 1 ]]; then
  args=(tauri build)
  if [[ $SIGN == 0 ]]; then args+=(--no-sign); fi
  if [[ -n "$PROFILE" ]]; then args+=("$PROFILE"); fi
  if [[ -n "$TARGET"  ]]; then args+=(--target "$TARGET"); fi
  if [[ -n "$BUNDLES" ]]; then args+=(--bundles "$BUNDLES"); fi

  step "构建桌面安装包：pnpm ${args[*]}"
  pnpm "${args[@]}"
else
  # asaled 用 rust-embed 把 ../dist 编进二进制（无桌面模式的 Web UI 就是它），
  # 所以即使不打桌面包也得先构建前端，否则出来的 asaled 只会回一句“没有内嵌 UI”。
  step "构建前端（asaled 内嵌的 Web UI）"
  pnpm build
fi

# 四个 crate 是一个 workspace（./Cargo.toml），target 在这里，不在 src-tauri/ 下面。
out="target"
if [[ -n "$TARGET" ]]; then out="$out/$TARGET"; fi
if [[ "$PROFILE" == "--debug" ]]; then out="$out/debug/bundle"; else out="$out/release/bundle"; fi

# ---------------------------------------------------------------- 公证 dmg
# tauri 只公证 .app：构建日志里 "Notarizing …/Asale.app" 之后就直接 "Signing …dmg"，
# 没有第二次公证。而 .app 内部的票据管不到外层容器 —— 用户下载的是 dmg，双击挂载时
# Gatekeeper 评估的是 dmg 本身，未公证就仍旧弹 "Apple 无法检查 App 是否包含恶意软件"。
# 装订到 dmg 上还顺带让首次打开能离线通过（否则要现场联网查 Apple 的公证库）。
#
# 这里不重新签名，dmg 已经被 tauri 签过；只是补提交公证 + stapler。
if [[ "$os" == "Darwin" && -d "$out" ]]; then
  # 用普通变量记有没有凭据，不靠 ${#arr[@]}：macOS 自带的还是 bash 3.2，
  # set -u 下对空数组取值会直接 unbound variable 退出。
  have_notary=0
  notary_args=()
  if [[ -n "${APPLE_API_KEY_PATH:-}" && -n "${APPLE_API_KEY:-}" && -n "${APPLE_API_ISSUER:-}" ]]; then
    notary_args=(--key "$APPLE_API_KEY_PATH" --key-id "$APPLE_API_KEY" --issuer "$APPLE_API_ISSUER")
    have_notary=1
  elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
    notary_args=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
    have_notary=1
  fi

  # 用 while read 而不是 for $(find)：产物名里有空格（"Asale 0.1.0.dmg" 这类）会被拆开。
  while IFS= read -r dmg; do
    if [[ $have_notary -eq 0 ]]; then
      echo "   注意：没有公证凭据，$(basename "$dmg") 未公证 —— 用户下载后双击仍会被 Gatekeeper 拦"
      continue
    fi
    step "公证 $(basename "$dmg")"
    # --wait：不等的话后面 stapler 必然失败（票据还没生成）。公证一般 1–5 分钟。
    xcrun notarytool submit "$dmg" "${notary_args[@]}" --wait
    xcrun stapler staple "$dmg"
    # 装订完立刻自检：notarytool 返回 Accepted 但 staple 没落盘的情况（磁盘只读、
    # 路径被替换）不会让上面两条命令失败，只有这里能抓到。
    spctl -a -vvv -t open --context context:primary-signature "$dmg"
  done < <(find "$out" -maxdepth 2 -type f -name '*.dmg')
fi

# ------------------------------------------------------- 命令行 / 无桌面产物
# 一份归档，两个二进制：
#
#   asaled  守护进程。客户端的全部逻辑都在它里面，Web UI 也编进去了
#           （rust-embed，见 daemon/src/rpc.rs），所以一台没有桌面的机器
#           `asaled --bind 0.0.0.0:9700` 之后，浏览器打开端口就是完整的客户端。
#   asale   命令行：start/stop/restart/status、开机自启注册、打印带 token 的网址。
#
# 这两个都不链接 webkit/GTK —— 无桌面 Linux 装的就是这一份，安装脚本
# （asale-web 的 /dl/install.sh）在检测不到桌面时只取它。桌面版机器也一起装，
# 好让终端里有 `asale` 命令。
#
# 归档名里带版本号，与 tauri 的产物一致；asale-web 的 src/lib/downloads.ts 按
# 后缀正则挑资产，改名要同时改那边。
if [[ $BUILD_CLI == 1 ]]; then
  VERSION="$(sed -n 's/^version = "\([^"]*\)".*/\1/p' Cargo.toml | head -1)"
  [[ -n "$VERSION" ]] || die "读不到版本号（Cargo.toml 的 [workspace.package] version）"

  # 注意 bin 名是 asale-cli 不是 asale：workspace 里 src-tauri 的桌面壳二进制已经
  # 叫 asale，两个 bin 写同一个 target 路径会互相覆盖（见 cli/Cargo.toml 的说明）。
  # 归档里再改回 asale —— 那才是用户敲的命令。
  cargo_args=(build -p asale-cli -p asale-daemon --bin asale-cli --bin asaled)
  profile_dir="release"
  if [[ "$PROFILE" == "--debug" ]]; then profile_dir="debug"; else cargo_args+=(--release); fi

  stage="target/asale-cli-stage"
  rm -rf "$stage"; mkdir -p "$stage" "$out/cli"

  if [[ "$os" == "Darwin" && "$TARGET" == "universal-apple-darwin" ]]; then
    # cargo 不认 universal-apple-darwin（那是 tauri 自己合出来的），两个 target
    # 各编一遍再 lipo。
    step "构建命令行工具（asale + asaled，arm64 + x86_64）"
    for t in aarch64-apple-darwin x86_64-apple-darwin; do
      cargo "${cargo_args[@]}" --target "$t"
    done
    # 左边是归档里的名字，右边是 cargo 产出的名字。
    for pair in "asale:asale-cli" "asaled:asaled"; do
      out_name="${pair%%:*}"; built="${pair##*:}"
      lipo -create -output "$stage/$out_name" \
        "target/aarch64-apple-darwin/$profile_dir/$built" \
        "target/x86_64-apple-darwin/$profile_dir/$built"
    done
    plat="macos-universal"
  else
    step "构建命令行工具（asale + asaled）"
    if [[ -n "$TARGET" ]]; then
      cargo "${cargo_args[@]}" --target "$TARGET"
      bin_dir="target/$TARGET/$profile_dir"
    else
      cargo "${cargo_args[@]}"
      bin_dir="target/$profile_dir"
    fi
    cp "$bin_dir/asale-cli" "$stage/asale"
    cp "$bin_dir/asaled" "$stage/asaled"
    case "$os" in
      Darwin) plat="macos-$(uname -m)" ;;
      *)      plat="linux-$(uname -m)" ;;
    esac
  fi

  # strip 要在签名之前：先签再 strip 会把签名一起去掉，签名失效的二进制在
  # Apple Silicon 上直接被内核拒绝执行（"killed 9"）。
  strip "$stage/asale" "$stage/asaled" 2>/dev/null || strip -x "$stage/asale" "$stage/asaled" 2>/dev/null || true

  if [[ "$os" == "Darwin" ]]; then
    # lipo 出来的胖二进制没有签名，而 Apple Silicon 上无签名的 Mach-O 根本起不来
    # （不是 Gatekeeper 弹窗，是内核直接 SIGKILL）。有 Developer ID 就用它，
    # 没有就 ad-hoc（`-s -`）—— 后者足够让二进制在本机跑起来，只是别人从浏览器
    # 下载会被隔离属性拦住，所以安装脚本装完会清一次 com.apple.quarantine。
    ident="${APPLE_SIGNING_IDENTITY:--}"
    for bin in asale asaled; do
      codesign --force --timestamp --options runtime -s "$ident" "$stage/$bin" 2>/dev/null \
        || codesign --force -s - "$stage/$bin"
    done
    if [[ "$ident" == "-" ]]; then
      echo "   注意：命令行二进制只做了 ad-hoc 签名（没有 APPLE_SIGNING_IDENTITY）"
    fi
  fi

  # 手动下载归档的人也该知道这两个文件是干什么的。
  cat > "$stage/INSTALL.txt" <<EOF
Asale client — command line / headless install (v${VERSION})

  asaled   the service. Holds all of asale's logic and serves the web UI itself.
  asale    the command line: start/stop/restart/status, boot registration.

Install by hand:

  sudo install -m 755 asale asaled /usr/local/bin/
  asale start --web            # listen on every interface (port 9700)
  asale url                    # the URL to open, access token included
  asale autostart enable       # come back after a reboot

Or let the installer do it: curl -fsSL https://asale.ai/dl/install.sh | sh

The token in that URL is the whole authorization: anyone holding it can read
your credentials and spend your balance. Keep the port off the public internet,
or put a TLS reverse proxy in front of it.

Docs: https://asale.ai/   ·   asale help web
EOF

  archive="asale-cli-${VERSION}-${plat}.tar.gz"
  # -C stage：归档里就是三个文件，没有目录前缀 —— 安装脚本按固定名字取。
  tar -czf "$out/cli/$archive" -C "$stage" asale asaled INSTALL.txt
  rm -rf "$stage"
  echo "    $out/cli/$archive"
fi

# ---------------------------------------------------------------- 产物
step "产物"
if [[ -d "$out" ]]; then
  find "$out" -maxdepth 2 -type f \
    \( -name '*.dmg' -o -name '*.deb' -o -name '*.AppImage' -o -name '*.rpm' \
       -o -name 'asale-cli-*.tar.gz' \) \
    -exec ls -lh {} \; | awk '{printf "    %-8s %s\n", $5, $NF}'
  echo
  echo "产物目录：$out"
else
  echo "    没找到 $out —— 构建可能只出了可执行文件"
fi

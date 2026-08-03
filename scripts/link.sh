#!/usr/bin/env bash
# 把这份源码接到系统的 `asale` 命令上（macOS / Linux）
#
#   ./scripts/link.sh                 # debug 构建，链接到 /usr/local/bin
#   ./scripts/link.sh --release       # release 构建（启动快、体积小，编译慢）
#   ./scripts/link.sh --prefix ~/.local/bin
#   ./scripts/link.sh --no-build      # 只重做链接，不编译
#   ./scripts/link.sh --web           # 顺带重新构建前端（asaled 内嵌的 Web UI）
#   ./scripts/link.sh --status        # 现在的 asale / asaled 指向哪里
#   ./scripts/link.sh --unlink        # 撤销：删软链，把备份的正式版放回去
#
# 装的是**软链**，不是拷贝：链完之后改代码只要再 `cargo build`，终端里的
# `asale` 立刻就是新的，不用重新跑这个脚本。
#
# 两个二进制都要链：
#   asale   命令行本体（cargo 里的 bin 名是 asale-cli，见 cli/Cargo.toml）
#   asaled  服务本体，`asale start` 拉起的就是它
# 只链 asale 其实也能跑 —— `paths::find_asaled()` 会先看自己旁边有没有
# asaled，而软链在被执行时已经解析成 target/<profile>/asale-cli，"旁边"就是
# 同一次构建的 asaled。但那样 `asaled` 这个命令本身仍是旧的正式版，
# 两个入口给出不同版本，排查问题时非常费解，所以一起链。
#
# 编译期注入的值来自 ./.env（见 .env.example），与 scripts/package.sh 同源：
# ASALE_QUOTA_PUBKEY 缺了客户端拒绝上市卖出，会永远卡在「正在上线」。
# 这里不校验地址必须是 https/wss —— 本地开发本来就指向 127.0.0.1。
set -euo pipefail

cd "$(dirname "$0")/.."   # asale-client 根目录
ROOT="$PWD"
ENV_FILE=".env"
# 备份放数据目录而不是 target/：`cargo clean` 不该把正式版的二进制一起删掉。
BACKUP_DIR="${ASALE_DATA_DIR:-$HOME/.asale}/link-backup"

PREFIX="/usr/local/bin"
PROFILE="debug"
BUILD=1
BUILD_WEB=0
ACTION="link"

for ((i = 1; i <= $#; i++)); do
  arg="${!i}"
  case "$arg" in
    --release)  PROFILE="release" ;;
    --debug)    PROFILE="debug" ;;
    --prefix)   i=$((i+1)); PREFIX="${!i}" ;;
    --no-build) BUILD=0 ;;
    --web)      BUILD_WEB=1 ;;
    --status)   ACTION="status" ;;
    --unlink)   ACTION="unlink" ;;
    -h|--help)  sed -n '2,25p' "$0"; exit 0 ;;
    *) echo "未知参数: $arg" >&2; exit 2 ;;
  esac
done

# ~ 在 --prefix 的值里不会被 shell 展开（那是 `--prefix ~/.local/bin` 写法之外
# 的情况，比如 --prefix=…、或从别的脚本传进来的字符串），手动补一次。
PREFIX="${PREFIX/#\~/$HOME}"

step() { echo; echo "==> $*"; }
die()  { echo "!! $*" >&2; exit 1; }

# 目标目录一般是 root 的，但 ~/.local/bin 不是；能写就别要 sudo。
SUDO=""
need_sudo() {
  local dir="$1"
  [[ -d "$dir" && -w "$dir" ]] && return 0
  [[ ! -d "$dir" ]] && [[ -w "$(dirname "$dir")" ]] && return 0
  command -v sudo >/dev/null || die "$dir 不可写，且这台机器上没有 sudo"
  SUDO="sudo"
  echo "   $dir 需要管理员权限，下面会用 sudo"
}

# 这条软链是不是本仓库装的？unlink 只删自己装的东西。
points_into_repo() {
  local link="$1"
  [[ -L "$link" ]] || return 1
  local target
  target="$(readlink "$link")"
  case "$target" in "$ROOT"/*) return 0 ;; *) return 1 ;; esac
}

describe() {
  local name="$1"
  local p="$PREFIX/$name"
  if [[ -L "$p" ]]; then
    local t; t="$(readlink "$p")"
    if points_into_repo "$p"; then
      echo "   $p -> $t   （本仓库）"
    else
      echo "   $p -> $t"
    fi
  elif [[ -e "$p" ]]; then
    echo "   $p   （普通文件，正式安装）"
  else
    echo "   $p   （不存在）"
  fi
  # PATH 里排在前面的可能是别的一份，这才是敲 `asale` 真正执行的东西。
  local first; first="$(command -v "$name" 2>/dev/null || true)"
  if [[ -n "$first" && "$first" != "$p" ]]; then
    echo "   注意：PATH 里先命中的是 $first"
  fi
}

# ---------------------------------------------------------------- status
if [[ "$ACTION" == "status" ]]; then
  step "当前指向"
  describe asale
  describe asaled
  [[ -e "$BACKUP_DIR/asale" || -e "$BACKUP_DIR/asaled" ]] && {
    echo
    echo "   备份（--unlink 时会放回去）：$BACKUP_DIR"
    ls -l "$BACKUP_DIR" | sed 's/^/     /'
  }
  exit 0
fi

# ---------------------------------------------------------------- unlink
if [[ "$ACTION" == "unlink" ]]; then
  need_sudo "$PREFIX"
  step "撤销链接"
  for name in asale asaled; do
    p="$PREFIX/$name"
    if points_into_repo "$p"; then
      $SUDO rm -f "$p"
      echo "   删除 $p"
    elif [[ -e "$p" ]]; then
      echo "   跳过 $p —— 不是本仓库装的软链，原样留着"
      continue
    fi
    if [[ -e "$BACKUP_DIR/$name" ]]; then
      $SUDO install -m 755 "$BACKUP_DIR/$name" "$p"
      rm -f "$BACKUP_DIR/$name"
      echo "   恢复 ${p}（来自备份）"
    fi
  done
  rmdir "$BACKUP_DIR" 2>/dev/null || true
  echo
  echo '完成。`hash -r` 一下（或开个新终端）让 shell 忘掉旧路径。'
  exit 0
fi

# ---------------------------------------------------------------- 构建
if [[ $BUILD == 1 ]]; then
  [[ -f "$ENV_FILE" ]] || die "缺少 ${ENV_FILE}：cp .env.example $ENV_FILE 后填值"
  set -a; . "./$ENV_FILE"; set +a
  [[ -n "${ASALE_QUOTA_PUBKEY:-}" ]] || die "ASALE_QUOTA_PUBKEY 为空（见 ${ENV_FILE}）。
   没有它，链上去的客户端无法验证网关授权，卖出会永远卡在「正在上线」。
   取值：网关启动日志里的 'quota pubkey' 那一行。"

  command -v cargo >/dev/null || die "需要 Rust 工具链：https://rustup.rs"

  # asaled 用 rust-embed 把 ../dist 编进二进制（浏览器打开端口看到的就是它）。
  # 目录空着也能编过，只是运行时回一句「没有内嵌 UI」，所以这里替它挡一次。
  if [[ $BUILD_WEB == 1 || ! -f "dist/index.html" ]]; then
    command -v pnpm >/dev/null || die "需要 pnpm：npm i -g pnpm"
    step "构建前端（asaled 内嵌的 Web UI）"
    pnpm build
  else
    echo "   前端沿用现成的 dist/（要重新构建加 --web）"
  fi

  step "构建 asale + asaled（${PROFILE}）"
  echo "    ASALE_SERVER_API=${ASALE_SERVER_API:-<默认>}"
  echo "    ASALE_GATEWAY_WS=${ASALE_GATEWAY_WS:-<默认>}"
  echo "    ASALE_QUOTA_PUBKEY=${ASALE_QUOTA_PUBKEY:0:16}…"
  args=(build -p asale-cli -p asale-daemon --bin asale-cli --bin asaled)
  [[ "$PROFILE" == "release" ]] && args+=(--release)
  cargo "${args[@]}"
fi

BIN_DIR="$ROOT/target/$PROFILE"
# cargo 出的名字是 asale-cli，敲的命令是 asale —— 同一个 workspace 里桌面壳的
# bin 已经占了 asale 这个名字（见 cli/Cargo.toml 顶部的说明）。
[[ -x "$BIN_DIR/asale-cli" ]] || die "找不到 $BIN_DIR/asale-cli（先跑一次不带 --no-build 的链接）"
[[ -x "$BIN_DIR/asaled"   ]] || die "找不到 $BIN_DIR/asaled（先跑一次不带 --no-build 的链接）"

# ---------------------------------------------------------------- 链接
need_sudo "$PREFIX"
[[ -d "$PREFIX" ]] || $SUDO install -d -m 755 "$PREFIX"

step "链接到 $PREFIX"
mkdir -p "$BACKUP_DIR"
for pair in "asale:asale-cli" "asaled:asaled"; do
  name="${pair%%:*}"; built="${pair##*:}"
  dest="$PREFIX/$name"
  # 备份的是"真身"：装过的正式版是普通文件，软链没什么好备份的。
  # 已经有备份就不动它 —— 第二次链接时 dest 已是自己的软链，覆盖会把
  # 正式版那份备份冲掉。
  if [[ -f "$dest" && ! -L "$dest" && ! -e "$BACKUP_DIR/$name" ]]; then
    cp -p "$dest" "$BACKUP_DIR/$name"
    echo "   备份原有的 $dest -> $BACKUP_DIR/$name"
  fi
  $SUDO ln -sfn "$BIN_DIR/$built" "$dest"
  echo "   $dest -> $BIN_DIR/$built"
done

# ---------------------------------------------------------------- 自检
step "自检"
hash -r 2>/dev/null || true
"$PREFIX/asale" --version || die "$PREFIX/asale 跑不起来"
first="$(command -v asale 2>/dev/null || true)"
if [[ -n "$first" && "$first" != "$PREFIX/asale" ]]; then
  echo "   注意：PATH 里先命中的是 ${first}，敲 asale 用的还是那一份"
elif [[ -z "$first" ]]; then
  echo "   注意：$PREFIX 不在 PATH 里，把它加进去才能直接敲 asale"
fi

cat <<EOF

好了。改完代码 \`cargo build$([[ "$PROFILE" == release ]] && echo " --release")\` 一下，asale 就是新的（软链不用重做）。

  asale status            # 服务在不在、版本是多少
  asale start / stop      # 起停 asaled
  ./scripts/link.sh --status
  ./scripts/link.sh --unlink   # 换回正式版

默认还是正式版那一套状态（~/.asale、127.0.0.1:9700），装了桌面版就会跟它抢
同一个端口和数据目录。要跟正式版井水不犯河水，用开发那一套环境变量：

  ASALE_DATA_DIR=~/.asale-dev ASALE_BIND=127.0.0.1:9701 ASALE_PROXY_PORT=9788 asale start
EOF

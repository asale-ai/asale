# 開発ドキュメント

Asale クライアントのアーキテクチャ、ローカル開発、パッケージングとリリース。
コードを変更する人・自分でビルドする人向けです。インストールして使いたいだけなら
[README](../README.ja.md) を参照してください。

[English](DEVELOPMENT.md) · [简体中文](DEVELOPMENT.zh-CN.md) ·
[繁體中文](DEVELOPMENT.zh-TW.md) · **日本語**

---

## アーキテクチャ

```
asale-client/
├─ protocol/    asale-protocol —— wsrelay のワイヤプロトコル。server と client が共有する唯一の定義
├─ core/        asale-client-core —— プロトコルクライアント、実行器、ローカルストア（単独でビルド・テスト可能）
│   ├─ ws.rs         署名付きハンドシェイク、supply.declare、ハートビート、ジョブ配信
│   ├─ executor.rs   ローカルのサブスク認証情報を注入、ストリーム返却、使用量の解析、予算の適用
│   ├─ discovery.rs  ToolAdapter：CLI ごとの検出と設定の読み書き
│   ├─ security.rs   デバイスの Ed25519 識別子
│   └─ store.rs      SQLite。keychain の参照のみを保存し、平文の認証情報は持たない
├─ daemon/      asaled —— すべての業務ロジック。ローカル HTTP/JSON-RPC :9700
│   ├─ oauth.rs / auth_store.rs   プラットフォーム別 OAuth ログインと ~/.asale/auths への隔離保存
│   ├─ proxy.rs                   ローカル購入プロキシ :9787（CLI が向ける接続先）
│   ├─ publisher.rs               販売側のセッション、上限、自動停止
│   └─ tool_config.rs             各 CLI 設定の書き換え／復元（元ファイルのバックアップ付き）
├─ src-tauri/   Tauri 2 のシェル：トレイ、ログイン時起動、自動更新、ディープリンク asale://、単一インスタンス
└─ src/         フロントエンド Vite + React 18 + i18next（zh / zh-TW / en / ja、ライト／ダークテーマ）
```

**ロジックはすべて daemon にあり、Tauri は単なるシェル** です。そのためフロントエンドは
ブラウザで `http://localhost:9173` を開くだけで全ページが動作し（daemon が起動していれば
可）、デバッグのたびにデスクトップウィンドウを開く必要はありません。

---

## ローカル開発

Rust（stable）、Node 20+、pnpm が必要です。

```bash
pnpm install
pnpm dev:app          # daemon + Tauri ウィンドウを起動（ASALE_QUOTA_PUBKEY を注入）
cargo test            # workspace 全体のテスト
cargo test -p asale-client-core
```

> `pnpm tauri dev` ではなく `pnpm dev:app` を使ってください。`ASALE_QUOTA_PUBKEY` が
> ないとクライアントはゲートウェイの認可を検証できず、販売が「オンライン化中」から
> 永久に進みません。

### 製品版と同時に動かす

`dev:app` はローカル状態一式を開発専用のコピーへ切り替えるので、インストール済みの
製品版を開いたままでも並行して起動できます。

| | 製品版 | `pnpm dev:app` |
|---|---|---|
| データディレクトリ | `~/.asale` | `~/.asale-dev`（`ASALE_DATA_DIR`） |
| daemon | `127.0.0.1:9700` | `127.0.0.1:9701`（`ASALE_BIND`） |
| ローカルプロキシ | `9787` | `9788`（`ASALE_PROXY_PORT`） |
| bundle identifier | `com.asale.desktop` | `com.asale.desktop.dev`（`src-tauri/tauri.dev.conf.json`） |

identifier は必ず分けてください。単一インスタンスのロックは `/tmp/<identifier>_si.sock` なので、
共有すると開発版は起動直後に終了し、製品版のウィンドウが前面に出るだけになります。
ウィンドウ状態とログイン時起動の登録も identifier 単位なので同時に分離されます。

各変数は `${VAR:-既定値}` の形なので、一時的な差し替えは上書きするだけです。

```bash
ASALE_DATA_DIR=~/.asale-staging ASALE_BIND=127.0.0.1:9702 pnpm dev:app
```

`pnpm dev` 単体（ブラウザでのデバッグ）はフロントエンドが `127.0.0.1:9700` を向いたままなので、
開発 daemon に繋ぐには `VITE_ASALE_DAEMON=http://127.0.0.1:9701 pnpm dev` とします。
`dev:app` が起動する vite はこの変数を引き継ぐため、追加の設定は不要です。

共有されたままなのは CLI ツール自身の設定（`~/.claude`、`~/.codex/config.toml`）だけです。
購読と購入はまさにこれらの実ファイルを書き換える機能なので、両者は互いを上書きします。
同時に操作しないでください。

OAuth のクライアント認証情報は [`.env.example`](../.env.example) を参照
（Gemini は自前で用意が必要、Claude/Codex は公開のデフォルト値あり）。

### システムの `asale` コマンドをこのソースに向ける

`pnpm dev:app` が面倒を見るのはデスクトップウィンドウです。**ターミナルの** `asale` も
このコードにしたいとき（CLI の開発、`asale start` の挙動の確認、ヘッドレスモードの
ローカル再現）は `scripts/link.sh` を使います:

```bash
./scripts/link.sh                 # debug ビルドを /usr/local/bin にシンボリックリンク（sudo が要る）
./scripts/link.sh --release       # 起動が速く小さい。コンパイルは遅い
./scripts/link.sh --prefix ~/.local/bin   # /usr/local/bin を触らない＝sudo も不要
./scripts/link.sh --status        # 今 asale / asaled がどこを指しているか
./scripts/link.sh --unlink        # 取り消し: リンクを削除し、バックアップした正式版を戻す
```

コピーではなく**シンボリックリンク**なので、一度リンクすれば後は `cargo build` するだけで
ターミナルの `asale` が即座に新しいバイナリになります。`asale` と `asaled` の両方を
リンクします。前者だけでも動きます（`paths::find_asaled()` は自分の隣を先に見ますし、
シンボリックリンクは実行時点で `target/<profile>/` に解決済みです）が、それだと
`asaled` コマンド自体は正式版のままで、2 つの入口が別のバージョンを名乗ることになり
調査が非常につらくなります。

`/usr/local/bin` にインストール済みの実体は `~/.asale/link-backup/` に退避され、
`--unlink` で元に戻ります。`--unlink` が消すのはこのリポジトリを指すリンクだけです。

コンパイル時に埋め込む値はパッケージングと同じ `./.env` から読みます。
`ASALE_QUOTA_PUBKEY` が無ければリンクしたビルドも販売できません。

リンクしたビルドの既定は正式版と同じ状態（`~/.asale`、`127.0.0.1:9700`）なので、
デスクトップ版が入っているとポートとデータディレクトリを取り合います。上の表の
変数で分けてください:

```bash
ASALE_DATA_DIR=~/.asale-dev ASALE_BIND=127.0.0.1:9701 ASALE_PROXY_PORT=9788 asale start
```

Windows 版のスクリプトはありません。`cargo run -p asale-cli -- status` を使ってください。

---

## パッケージング

パッケージングのパラメータはすべて `.env` から読みます（`cp .env.example .env` の後に
記入）。これらの値は**コンパイル時**にバイナリへ固定されます。デスクトップアプリは
ダブルクリック起動で shell 環境を持たないため、接続先とゲートウェイ公開鍵は
埋め込むしかありません。

```bash
cp .env.example .env      # ASALE_QUOTA_PUBKEY を記入。無いとビルドしたクライアントは販売できない

./scripts/package.sh                          # macOS → .dmg（既定は arm64 + x86_64 のユニバーサルバイナリ）
./scripts/package.sh --bundles deb,appimage   # Linux 上で
pwsh scripts/package.ps1                      # Windows 上で → .msi / .exe
./scripts/package.sh --no-sign --debug        # ローカル試験ビルド：更新パッケージに署名せず、ビルドが大幅に速い
```

スクリプトは `pnpm tauri build` の呼び出しを組み立てるだけでなく、ユーザーのマシンに
入れて初めて発覚するような問題を事前に弾きます：接続先は https/wss 必須（クライアントは
実行時にも平文のリモートアドレスを拒否）、公開鍵が空でないこと、Linux の
webkit2gtk-4.1 依存、macOS で Apple 証明書が無い場合の警告。

Tauri はクロスプラットフォームでパッケージングできません：`.dmg` は macOS、
`.msi`/`.exe` は Windows、`.deb`/`.AppImage` は Linux でしか生成できません。
3 プラットフォーム = 3 台のマシン、あるいは `v*` タグを push して
[`.github/workflows/release.yml`](../.github/workflows/release.yml) の 3 つの job に
一度にすべて作らせてください。

成果物は `target/<target>/release/bundle/` に出力され、各インストーラの隣に `.sig` が
1 つずつ置かれます。

### コマンドラインとヘッドレス用アーカイブ

実行のたびに `bundle/cli/asale-cli-<version>-<platform>.tar.gz`（Windows では `.zip`）も
生成されます。中身はバイナリ 2 つです:

- **`asaled`** —— サービス本体。asale のすべてのロジックを持ち、ウェブ UI も埋め込まれて
  います（`rust-embed`、`daemon/src/rpc.rs` を参照）。そのためデスクトップのないマシンでも
  `asale start --web` の後、任意のブラウザーから利用できます。
- **`asale`** —— コマンドライン: start/stop/restart/status、起動時登録、トークン付き URL の
  表示。[CLI.md](CLI.md) を参照。

どちらも webkit / GTK にリンクしません。これがまっさらなサーバーに導入できる理由であり、
ビルドマシンにデスクトップ関連の依存を一切必要としない理由でもあります:

> cargo 上のコマンドラインの bin 名は **`asale-cli`** で、アーカイブに入れる際に
> パッケージングスクリプトが `asale` へ改名します。デスクトップシェルのバイナリが既に
> `asale` であり、同一ワークスペースの 2 つの bin が同じ `target/<profile>/` パスに書くと
> 互いを上書きしてしまうためです。ローカルでは `cargo run -p asale-cli -- status`、
> インストール後は `asale status` になります。

```bash
./scripts/package.sh --cli-only     # アーカイブのみ。.dmg/.deb/.AppImage は作らない
./scripts/package.sh --no-cli       # インストーラのみ。従来どおり
```

`--cli-only` でもフロントエンドは先にビルドされます。ウェブ UI は `asaled` の *中に*
コンパイルされるため、`pnpm build` を飛ばすと「UI が埋め込まれていない」と答えるだけの
サービスができあがります。

このアーカイブが `https://asale.ai/dl/install.sh` のダウンロード対象で、サイト側リポジトリの
`src/lib/downloads.ts` にある正規表現で選ばれます —— 名前を変えるならその表も変更が必要です。

---

## リリースと自動更新

更新パッケージの署名秘密鍵は `asale-updater.key` です（gitignored。公開鍵は
`tauri.conf.json` に埋め込み済み）。この秘密鍵を失う、あるいはローテーションすると、
既にインストール済みのクライアントは二度と新バージョンを検証できません —— 本番用の
鍵として管理してください。

自動更新は `https://dl.asale.ai/updater/{{target}}/{{current_version}}` を参照し、
標準の Tauri updater JSON を返す必要があります（最新であれば 204）。

インストーラはサイトと一緒に公開します（サイトのリポジトリの `public/download/` 配下に
それぞれ 1 部ずつ）。macOS のパッケージは Developer ID 証明書で署名し Apple の公証を通して
いるため、Gatekeeper は警告なしで通します —— この処理は `tauri build` の内部で行われ、
`.github/workflows/release.yml` に挙げた `APPLE_*` の各 secret が必要です。設定しないまま
ビルドすると ad-hoc 署名のバンドルになり、自分のマシンでしか動きません。

---

## 関連ドキュメント

メインリポジトリには仕様と設計のドキュメント群があり、コードコメント中の `spec §x.y` は
対応する spec を指しています。

- `asale-client-spec.md` —— クライアント実装仕様（how）
- `asale-client-design.md` —— 設計上のトレードオフ（why）
- `token-trading.md` —— 買い手 → プラットフォーム → 売り手の実際の経路。コード上の位置を注記
- `deploy/README.md` —— デプロイ、証明書、環境変数、クライアントのパッケージング

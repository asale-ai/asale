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

OAuth のクライアント認証情報は [`.env.example`](../.env.example) を参照
（Gemini は自前で用意が必要、Claude/Codex は公開のデフォルト値あり）。

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

---

## リリースと自動更新

更新パッケージの署名秘密鍵は `asale-updater.key` です（gitignored。公開鍵は
`tauri.conf.json` に埋め込み済み）。この秘密鍵を失う、あるいはローテーションすると、
既にインストール済みのクライアントは二度と新バージョンを検証できません —— 本番用の
鍵として管理してください。

自動更新は `https://dl.asale.ai/updater/{{target}}/{{current_version}}` を参照し、
標準の Tauri updater JSON を返す必要があります（最新であれば 204）。

インストーラはサイトと一緒に公開します（サイトのリポジトリの `public/download/` 配下に
それぞれ 1 部ずつ）。macOS のパッケージは現状 Apple の署名・公証をしていないため、
ユーザーは初回起動時に手動で許可する必要があります。

---

## 関連ドキュメント

メインリポジトリには仕様と設計のドキュメント群があり、コードコメント中の `spec §x.y` は
対応する spec を指しています。

- `asale-client-spec.md` —— クライアント実装仕様（how）
- `asale-client-design.md` —— 設計上のトレードオフ（why）
- `token-trading.md` —— 買い手 → プラットフォーム → 売り手の実際の経路。コード上の位置を注記
- `deploy/README.md` —— デプロイ、証明書、環境変数、クライアントのパッケージング

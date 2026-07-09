# 実装台帳 (LEDGER)

作業開始時に必ず読み、完了時に更新する。

## マイルストーン状況

- **M0 基盤**: **実装完了(2026-07-09)**。`cargo build/clippy(--all-targets -D warnings)/fmt --check/test` すべて green(--offline)。ユニット22件 pass + ネットワーク依存の smoke テスト1件 `#[ignore]`。実 API 結合確認(`GET /2.0/user`)は環境に実 token が無いためスキップ(下記の未検証の仮定は据え置き)。
- M1 PRレビュー: 未着手
- M2 パイプライン監視: 未着手
- M3 リポジトリブラウズ: 未着手

## M0 実装メモ (2026-07-09)

- モジュール構成(実装): `main`(clap/tokio runtime/logout) / `config`(directories+toml) / `auth`(keyring) / `logging`(BBTUI_LOG時のみfile出力) / `api`{`error`(thiserror), `models`(serde), `client`(reqwest Basic + get_paged)} / `tui`{`mod`(端末RAIIガード Tui + panic hook), `app`(App/Screen/Msg/Command/update), `onboarding`(入力状態), `ui`(描画), `event`(ループ+dispatch)}。
- 画面遷移: Onboarding → Workspaces → Repositories → RepoSelected(プレースホルダ)。Elm風 `update()->Command`、非同期は `event::dispatch` が `tokio::spawn` して結果を `Msg` で返す。
- **イベントループの実装判断(design から逸脱)**: crossterm の非同期 `EventStream`(`.next()`)は `StreamExt`(futures-util)を **直接依存として宣言**しないと使えない。futures-util は vendor 済みだが transitive のみで、直接 `use` するには Cargo.toml への追加(=依存追加)が必要。依存凍結ルールを厳守するため、**入力は専用スレッドで blocking `event::read()` し `mpsc` へ橋渡し**する構成にした。メインループは「入力チャネル」「API結果チャネル」の2系統を `tokio::select!` で待つ(mpsc + select! の設計意図は維持)。crossterm の `event-stream` feature は結果的に未使用。
- **paginate の Send 問題**: `impl AsyncFnMut` クロージャで組んだ get_paged は高階ライフタイムにより `tokio::spawn` で "Send is not general enough" になる。→ ページ取得を `Pin<Box<dyn Future + Send + 'a>>`(型エイリアス `PageFuture`)を返す `FnMut` フェッチャに変更して解消。`get_paged` はこの `paginate` を使い、テストはモックフェッチャで next 追跡/クエリ初回限定/上限打切り/エラー伝播を検証。
- **crossterm 型は `ratatui::crossterm` 再エクスポート経由**で統一(直接依存 crossterm 0.28.1 と一致)。端末ガードは RAII(`Drop`)+ panic hook の二重で復元。
- **秘密情報**: token は keyring のみ。`BitbucketClient` の `Debug` は手動実装で token を `<redacted>`。config.toml には email/display_name/default_workspace のみ(token 非保存)。
- **keyring バックエンド → 解決済み(2026-07-09)**: `keyring = { features = ["apple-native"] }` を有効化し、`security-framework`/`core-foundation`(+ `-sys`)を vendor 追加。実行時は **macOS Keychain 実バックエンド**を使用し、token はプロセス再起動を跨いで永続化される(受け入れ条件「再起動で Onboarding スキップ」を実挙動で満たす)。全ゲート green を再確認済み。token は Keychain のみ・平文保存なしは不変。
  - 注: 現状 macOS 専用(`apple-native`)。Linux ビルド時は `keyring` に `linux-native` 等の feature を足して再 vendor が必要。
- ログ出力先はポータブルに `ProjectDirs::cache_dir()` を採用(Linux=`~/.cache/bitbucket-tui/`、macOS=`~/Library/Caches/dev.bitbucket-tui/`)。spec の `~/.cache/...` は Linux 表記。

## 検証済みの事実 (2026-07-09)

- 認証は HTTP Basic、**username = Atlassianアカウントのメール / password = API token(スコープ付き)**。Bitbucketユーザー名・トークン名では通らない(出典: Atlassian support "Using API tokens" / 401 KB)。
- App Password は 2026-06-09〜07-27 ブラウンアウト、**2026-07-28 完全廃止**。新規は API token 一択。
- ページングは `{ values, next, page, size, pagelen }` 形式。`next` URL を追跡。
- ツールチェーン: `cargo 1.96.1` / edition 2024。cargo は PATH 外。**`rustup run stable cargo <sub>` は `cargo vendor`/`build` で `rustc` を見失い失敗する**。必ず toolchain bin を PATH 前置して直接呼ぶ: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"` → `cargo <sub> --offline`。
- **vendored offline**: 全依存を `cargo vendor vendor` 済み、`.cargo/config.toml`(vendored-sources)配置。`cargo build/clippy/test/run --offline` が green。`vendor/`・`.cargo/config.toml` は `.gitignore`(再生成は `cargo vendor`)。**依存は凍結: `cargo add`/`update`/`vendor` はネットワーク不可**(feature 追加が要るときはメインが再 vendor する)。crates.io へはこのセッション/Codex とも到達不可。
- codex CLI: `/opt/homebrew/bin/codex` に存在。
- **Codex は git ワークスペース単位で書込み権限を持つ**。非git ディレクトリは隣接 git リポジトリ(muster等)の書込みルートにフォールバックし失敗する。→ プロジェクトを `git init` し、タスクは `codex-companion.mjs task --cwd <proj> --write` で起動する。`bitbucket-tui` は git 初期化済み。
- **ネットワーク制約 → vendored で解決済み**: Codex サンドボックスも通常 Bash も crates.io 到達不可。`cargo vendor vendor` で全依存を vendor 化し、`.cargo/config.toml`(vendored-sources) を配置。以降 `cargo build/clippy/test --offline` が green。`vendor/` と `.cargo/config.toml` は `.gitignore` 済み(再生成は `cargo vendor`)。
- **cargo 呼び出し(重要)**: `rustup run stable cargo <sub>` は `cargo vendor` 等で `rustc` を見つけられず失敗する。**ツールチェーン bin を PATH 前置**して直接呼ぶこと: `export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"` → `cargo <sub> --offline`。
- **crossterm はバージョン競合注意**: ratatui 0.29 は crossterm 0.28 を要求。直接依存も **0.28** に統一済み(0.29 だとバックエンド型不一致)。コード内では `ratatui::crossterm` 再エクスポートを使うこと。
- 依存確定版(Cargo.lock): ratatui 0.29 / crossterm 0.28.1 / tokio 1.52 / reqwest 0.12.28(rustls-tls,json) / serde 1.0.228 / serde_json 1.0.150 / keyring 3.6.3 / directories 6.0 / toml 1.1 / anyhow 1.0 / thiserror 2.0 / tracing 0.1 / tracing-subscriber 0.3 / clap 4.6 / base64 0.22。**依存は凍結。`cargo add`/`update`/`vendor` はネットワーク不可のため実行禁止**。

## 未検証の仮定

- 必要スコープ名(`account`/`repository`/`pullrequest`/`pullrequest:write`/`pipeline`)の正確さ — 実際の 200/403 応答で確定させる。
- `GET /2.0/workspaces` と `GET /2.0/repositories/{workspace}?role=member` のパラメータ挙動 — 実データで確認。

## 未解決の問い

- OAuth 2.0 対応の要否(現状 API token のみで足りる想定)。
- M1 の PR 横断取得に最適なエンドポイント(repo単位 `.../pullrequests` の集約 vs 他)。M1着手時に確定。

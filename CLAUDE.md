# bitbucket-tui

Bitbucket Cloud をキーボード駆動で操作する Rust 製 TUI クライアント。PRレビューが主目的。

## ドキュメント

- `docs/design.md` — 設計書(全体像・アーキ・マイルストーン)。**冒頭に変更履歴。読むこと**
- `docs/specs/M*.md` — 各マイルストーンの実装仕様書(実装エージェントへの委譲物)
- `docs/LEDGER.md` — 実装台帳(検証済みの事実 / 未検証の仮定 / 未解決の問い / マイルストーン状況)。**作業開始時に必ず読み、完了時に更新する**

## 体制

- 統括・設計判断: メインセッション(Opus/Fable)
- 実装: Codex(codex plugin 経由)。不可時は Sonnet サブエージェント
- 調査: Sonnet サブエージェント

## 対象・認証(重要)

- **Bitbucket Cloud のみ**。REST API 2.0 (`https://api.bitbucket.org/2.0`)
- 認証は **API token(スコープ付き)** の HTTP Basic。**username = Atlassianアカウントのメール**、password = API token
- App Password は 2026-07-28 廃止のため非対応

## ツールチェーン

- `cargo` は PATH 上に無い。**`rustup run stable cargo …`** で呼ぶ(cargo 1.96.1 / edition 2024)
- 例: `rustup run stable cargo clippy --all-targets -- -D warnings`

## 品質規約

- `cargo fmt --check` / `cargo clippy --all-targets -- -D warnings` / `cargo test` が常に通る状態を保つ
- `unwrap()` / `expect()` はテスト以外で禁止(初期化時のみ理由付き expect 可)
- エラー: lib的モジュールは `thiserror`、bin(main)は `anyhow`
- **TUI 中に stdout/stderr へ書かない**。ログは `BBTUI_LOG` 有効時のみ `~/.cache/bitbucket-tui/bitbucket-tui.log`(tracing)
- raw mode / alternate screen は **RAII + panic hook** で必ず復元する
- **秘密情報(API token)は平文ファイルに置かない**。OS Keychain(keyring crate)のみ

## ライセンス

MIT OR Apache-2.0(デュアル)

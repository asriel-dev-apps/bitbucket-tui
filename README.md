# bitbucket-tui

Bitbucket Cloud をキーボードで操作する Rust 製 TUI クライアント。PR レビューを主目的に、パイプライン監視とリポジトリ閲覧もできる。

- 対象: **Bitbucket Cloud のみ**（REST API 2.0）
- 認証: **スコープ付き API token** による HTTP Basic
- ライセンス: MIT OR Apache-2.0

## 機能

- **PR レビュー** — 一覧 / 詳細 / Diff（色付き）、approve・request-changes・コメント投稿・merge（確認モーダル）
- **パイプライン監視** — 一覧 / ステップ / ログ、stop・re-run、進行中の自動更新
- **リポジトリ閲覧** — ブランチ / コミット履歴 / コミット差分 / ソースツリー・ファイル（閲覧専用）

## 認証

username / password ではなく、**Atlassian アカウントのメールアドレス**と**スコープ付き API token** の HTTP Basic を使う（App Password は 2026-07-28 廃止のため非対応）。

1. [Atlassian の API tokens](https://id.atlassian.com/manage-profile/security/api-tokens) で **Create API token with scopes** を選び、対象に Bitbucket を指定する。
2. スコープを付与する（粒度スコープ。**write は read を含意しない**ため両方選ぶ）:
   - `read:user:bitbucket` / `read:workspace:bitbucket` / `read:repository:bitbucket`
   - `read:pullrequest:bitbucket` / `write:pullrequest:bitbucket`（approve・コメント・merge）
   - `read:pipeline:bitbucket` / `write:pipeline:bitbucket`（stop・re-run）
3. 生成された token を控える（再表示不可）。

起動後のログイン画面で **Email**（Atlassian アカウントのメール）と **Token** を入力する。token は **OS Keychain** にのみ保存し、平文ファイルには書かない（email と表示名は `config.toml` に保存）。

## インストール

Rust ツールチェーンがあれば、clone せず1コマンドで導入できる（`~/.cargo/bin/bitbucket-tui` に入る）。

```sh
cargo install --git https://github.com/asriel-dev-apps/bitbucket-tui.git
```

更新は同じコマンドに `--force` を付けて再実行する。

> 現状は macOS 向け（token 保存に macOS Keychain / `keyring` の `apple-native` を使用）。

### ソースからビルド

```sh
cargo run                    # ビルドして起動
# または
cargo build --release
./target/release/bitbucket-tui
```

### CLI

```sh
bitbucket-tui              # TUI 起動
bitbucket-tui logout       # 保存済みの token と設定を消去
bitbucket-tui --reset-auth # 認証情報を消去してから起動
```

## キー操作

| キー | 動作 |
|------|------|
| `↑` / `k`・`↓` / `j` | 移動 |
| `Enter` | 決定 / 開く |
| `Esc` / `Backspace` | 戻る |
| `PgUp` / `PgDn`・`g` / `G` | スクロール / 先頭・末尾 |
| `r` | 再読込 |
| `?` | ヘルプ（**その画面のキーを表示**） |
| `q` / `Ctrl+C` | 終了 |

approve `a` / merge `M`、パイプライン stop `S`・re-run `R`、ブランチ `b`・ソース `s` といった画面ごとのキーは、その画面で `?` を押せば一覧できる。破壊的な操作（merge / stop / re-run）は必ず確認モーダルを経由する。

## マウス操作

- ホイールはカーソル下のペインを 1 ノッチ 3 行（一覧では 3 件）移動する。
- 左クリックで PR 詳細のペイン移動、一覧行の選択、概要内リンク・画像、フッターの操作ヒントを操作できる。選択済みの一覧行をもう一度クリックすると `Enter` と同じ動作になる。
- モーダル外のクリックは `Esc` と同じく閉じる。merge / stop / re-run の確認は誤操作防止のためクリックでは実行されず、`Enter` だけで決定する。
- Diff 画面のファイル一覧サイドバー境界をドラッグすると幅を調整できる（最小幅未満まで縮めると非表示になる。`t` で再表示）。

マウスキャプチャ中、端末本来のテキスト選択は **Shift を押しながらドラッグ**する。tmux などのターミナルマルチプレクサ内では、マウスイベントや Shift+ドラッグのパススルー対応状況に依存する。

## ログ

`BBTUI_LOG` を設定したときのみログを出力する（TUI 実行中は stdout/stderr に書かない）。

```sh
BBTUI_LOG=debug cargo run
```

出力先は `bitbucket-tui.log`（macOS: `~/Library/Caches/dev.bitbucket-tui/`、Linux: `~/.cache/bitbucket-tui/`）。

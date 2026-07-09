# bitbucket-tui

Bitbucket Cloud をキーボード駆動で操作する Rust 製 TUI クライアント。PR レビューを主目的とし、
現在は **M0（基盤）** まで実装済み。

- 対象: **Bitbucket Cloud のみ**（REST API 2.0 / `https://api.bitbucket.org/2.0`）
- 認証: **API token（スコープ付き）による HTTP Basic**
- ライセンス: MIT OR Apache-2.0

M0 で動作する範囲: 初回認証（Onboarding）→ ワークスペース選択 → リポジトリ選択 →
選択済み画面（M1 の PR 一覧が入る継ぎ目のプレースホルダ）。

---

## 認証: API token の発行と必要スコープ

username / password ではなく、**Atlassian アカウントのメールアドレス** と
**スコープ付き API token** の HTTP Basic 認証を使う。
（Bitbucket ユーザー名やトークン名では通らない。App Password は 2026-07-28 に廃止のため非対応。）

### API token の発行手順

1. <https://id.atlassian.com/manage-profile/security/api-tokens>（Atlassian アカウント設定 > Security > API tokens）を開く。
2. **Create API token with scopes** を選び、対象に Bitbucket を指定。
3. 以下のスコープを付与する（M0〜M2 想定）:
   - `account`
   - `repository`
   - `pullrequest`
   - `pullrequest:write`
   - `pipeline`
4. 生成された token 文字列を控える（再表示不可）。

### ログイン時に入力する値

| 項目 | 値 |
|------|-----|
| Email | Atlassian アカウントのメールアドレス |
| Token | 上で発行したスコープ付き API token |

token は **OS Keychain（keyring）にのみ**保存し、平文ファイルには書かない。
email と表示名は設定ファイル（`config.toml`）に平文で保存する。

---

## ビルド（オフライン前提）

依存は `vendor/` に vendoring 済みで、**ネットワーク無しでビルドできる**（`.cargo/config.toml` が
vendored-sources を指す）。`vendor/` が無い場合は `cargo vendor vendor` で再生成が必要（要ネットワーク）。

`cargo` は PATH 上に無いため、ツールチェーンの bin を PATH 前置してから呼ぶ:

```sh
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

cargo build --offline
cargo clippy --all-targets --offline -- -D warnings
cargo fmt --check
cargo test --offline
```

> すべての cargo コマンドに `--offline` を付ける。`rustup run stable cargo …` は環境により
> `rustc` を見つけられないため使わない。

---

## 起動

```sh
cargo run --offline
# もしくはビルド済みバイナリ
./target/debug/bitbucket-tui
```

### CLI

```sh
bitbucket-tui              # TUI 起動
bitbucket-tui logout       # 保存済みの token（Keychain）と設定を消去
bitbucket-tui --reset-auth # 認証情報を消去してから TUI 起動
```

### キー操作

| キー | 動作 |
|------|------|
| `↑` / `k`, `↓` / `j` | リスト移動 |
| `Enter` | 決定 / 開く |
| `Esc` | 戻る（認証画面ではエラー消去） |
| `Tab` | （認証画面）email / token フィールド切替 |
| `?` | ヘルプ表示（任意のキーで閉じる） |
| `q` | 終了 |
| `Ctrl+C` | 強制終了 |

---

## 手動確認手順

1. `cargo run --offline` で起動。初回は Onboarding が表示される。
2. Email に Atlassian アカウントのメール、Token に発行した API token（マスク表示）を入力し `Enter`。
   - **成功**: `GET /2.0/user` で検証後、ワークスペース一覧へ遷移する。
   - **401（不正なメール/トークン）**: 画面にエラーが表示され、そのまま再入力できる。
   - **403（スコープ不足）**: スコープ不足の可能性がエラーとして表示される。
3. ワークスペースを選んで `Enter` → リポジトリ一覧（更新日時降順、ページング反映）。
4. リポジトリを選んで `Enter` → 選択済み画面に `full_name` と「M1: PR一覧をここに実装」が出る。
5. `q` で終了。

---

## ログ

- **`BBTUI_LOG` が設定されているときのみ**ログを出力する（TUI 実行中は stdout/stderr へ出さない）。
- 出力先はキャッシュディレクトリ配下の `bitbucket-tui.log`
  （Linux: `~/.cache/bitbucket-tui/`、macOS: `~/Library/Caches/dev.bitbucket-tui/`）。
- `BBTUI_LOG` の値は `tracing` の EnvFilter ディレクティブ（例: `BBTUI_LOG=debug`、
  `BBTUI_LOG=bitbucket_tui=trace`）。空文字なら `info`。

```sh
BBTUI_LOG=debug cargo run --offline
```

---

## 既知の制約（このビルド環境）

- **Keychain 永続化**: 現在 vendoring 済みの `keyring` は OS バックエンド機能
  （macOS の `apple-native` 等）が無効のため、実行時は **mock バックエンド**にフォールバックする。
  token の保存自体は成功し Onboarding は完了できるが、**プロセス再起動をまたいだ永続化はされない**
  （毎回 Onboarding が必要）。実 Keychain 連携には `security-framework` / `core-foundation` の
  vendoring と `keyring` の `apple-native` feature 有効化が必要（いずれもネットワークが要るため凍結中）。
  コードは keyring API のみを使い平文保存は一切行わないため、依存を差し替えれば本番挙動になる。

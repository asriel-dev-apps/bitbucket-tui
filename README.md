# bitbucket-tui

Bitbucket Cloud をキーボード駆動で操作する Rust 製 TUI クライアント。PR レビューを主目的とし、
現在は **M1（PR レビュー）** まで実装済み。

- 対象: **Bitbucket Cloud のみ**（REST API 2.0 / `https://api.bitbucket.org/2.0`）
- 認証: **API token（スコープ付き）による HTTP Basic**
- ライセンス: MIT OR Apache-2.0

動作する範囲: 初回認証（Onboarding）→ ワークスペース選択 → リポジトリ選択 →
**PR 一覧 → PR 詳細 → Diff（色付きスクロール）**、および approve / request-changes /
一般コメント投稿 / merge（確認モーダル）。

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

### キー操作（共通）

| キー | 動作 |
|------|------|
| `↑` / `k`, `↓` / `j` | リスト移動 |
| `Enter` | 決定 / 開く |
| `Esc` | 戻る（認証画面ではエラー消去） |
| `Tab` | （認証画面）email / token フィールド切替 |
| `?` | ヘルプ表示（画面ごとのキーも表示・任意のキーで閉じる） |
| `q` | 終了 |
| `Ctrl+C` | 強制終了 |

### キー操作（PR 一覧 / 詳細 / Diff）

| 画面 | キー | 動作 |
|------|------|------|
| PR 一覧 | `Enter` | PR 詳細を開く |
| PR 一覧 | `o` / `m` / `d` / `a` | state フィルタ切替（OPEN / MERGED / DECLINED / ALL） |
| PR 一覧 | `r` | 現在のフィルタで再読込 |
| PR 詳細 | `d` | Diff を開く |
| PR 詳細 | `c` | 一般コメントを書く（`Ctrl+S` 送信 / `Esc` 取消） |
| PR 詳細 | `a` | approve / unapprove をトグル |
| PR 詳細 | `x` | request-changes / 取消 をトグル |
| PR 詳細 | `M` | **merge（確認モーダルを開く）** |
| PR 詳細 | `↑↓` / `PgUp` `PgDn` | 変更ファイル選択 / 本文スクロール |
| merge モーダル | `←` `→` / `Tab` | マージ戦略切替（merge_commit / squash / fast_forward） |
| merge モーダル | `Space` | ソースブランチ削除トグル |
| merge モーダル | `Enter` / `Esc` | 実行 / 取消 |
| Diff | `↑↓` / `j` `k` | 1 行スクロール |
| Diff | `PgUp` / `PgDn` | 1 画面スクロール |
| Diff | `g` / `G` | 先頭 / 末尾へ |
| Diff | `n` / `N` | 次 / 前のファイル境界へジャンプ |

diff の色分け: `+` 追加＝緑 / `-` 削除＝赤 / `@@` ハンク＝シアン / `diff --git` ファイル見出し＝黄 /
それ以外＝既定色。

---

## 手動確認手順

1. `cargo run --offline` で起動。初回は Onboarding が表示される。
2. Email に Atlassian アカウントのメール、Token に発行した API token（マスク表示）を入力し `Enter`。
   - **成功**: `GET /2.0/user` で検証後、ワークスペース一覧へ遷移する。
   - **401（不正なメール/トークン）**: 画面にエラーが表示され、そのまま再入力できる。
   - **403（スコープ不足）**: スコープ不足の可能性がエラーとして表示される。
3. ワークスペースを選んで `Enter` → リポジトリ一覧（更新日時降順、ページング反映）。
4. リポジトリを選んで `Enter` → **PR 一覧（OPEN 既定）**。`o/m/d/a` で state を切り替え、`r` で再読込。
5. PR を選んで `Enter` → **PR 詳細**（メタ / 本文 / 変更ファイル一覧 / コメント）。
6. `q` で終了。

### PR レビュー一巡（実 token が必要・任意）

テスト用のリポジトリ・PR で以下を通しで確認する:

1. リポジトリを開いて PR 一覧を表示（`o/m/d/a` で state 切替、`r` で再読込）。
2. PR を `Enter` で開き、詳細のメタ・本文・変更ファイル一覧を確認。
3. `d` で Diff を開き、`↑↓`/`PgUp`/`PgDn`/`g`/`G` でスクロール、`n`/`N` でファイル境界移動。色分けを確認。
4. `Esc` で詳細へ戻り、`a` で approve → ステータス行に結果、詳細を再取得して承認数が反映されるのを確認。
   もう一度 `a` で unapprove。`x` で request-changes / 取消も同様。
5. `c` でコメントを書き、本文を入力して `Ctrl+S` で送信 → コメント一覧に反映されるのを確認（`Esc` で取消）。
6. `M` で merge 確認モーダルを開き、`←`/`→`/`Tab` で戦略、`Space` で close source branch を選んで `Enter`
   → マージ後に PR 状態が更新される（`Esc` でモーダル取消）。**確認モーダルを経由しないと merge は実行されない**。
7. 401 / 403 / 409 などのエラーはステータス行に表示される。

> 実 API で判明したフィールド/挙動（`participant.state` の値、`status` 値、merge の 202 応答、
> inline 位置の意味など）は `docs/LEDGER.md` の「未検証の仮定」に反映すること。

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

- **Keychain 永続化**: `keyring` は `apple-native` feature を有効化し `security-framework` /
  `core-foundation` を vendoring 済み。**macOS の実 Keychain** に token を保存し、プロセス再起動を
  またいで永続化される（2 回目以降は Onboarding をスキップ）。token は Keychain のみに保存し、
  平文ファイルには一切書かない。
  - macOS 専用（`apple-native`）。Linux でビルドする場合は `keyring` に `linux-native` 等の
    feature を追加して再 vendoring が必要。

# 認証情報の保存と受け渡し

bitbucket-tui が API token とメールアドレスをどこに・どの形で保存し、起動時にどう TUI へ渡すかをまとめる。

## 保存場所（2 箇所に分かれる）

| 何を | どこに | 形式 |
|---|---|---|
| **API token** | OS Keychain（macOS） | サービス名 `bitbucket-tui` / アカウント `<メールアドレス>` / secret に token 文字列 |
| **メールアドレス**（Basic 認証の username） | `config.toml`（平文） | `email = "you@example.com"` |

- token は **Keychain のみ**に保存し、平文ファイルには一切書かない（`src/auth.rs`）。
- Keychain エントリは `keyring` crate が `Entry::new("bitbucket-tui", <email>)` で作る generic password。
- `config.toml` には email のほか、任意で `display_name`・`default_workspace` を持つ（`src/config.rs`）。
- `config.toml` の場所:
  - macOS: `~/Library/Application Support/dev.bitbucket-tui/config.toml`
  - Linux: `~/.config/bitbucket-tui/config.toml`

## なぜ両方必要か（起動時の復元フロー）

`src/main.rs` の `restore_client` は次の順で認証を復元する。

```
config.toml の email を読む
        │
        ├─ 無い ─────────────► Onboarding へ
        │
        ▼
その email をキーに Keychain から token を引く
        │
        ├─ 無い ─────────────► Onboarding へ
        │
        ▼
BitbucketClient を復元（Onboarding をスキップ）
```

email は「**どの Keychain エントリを引くか**」の検索キーになる。したがって token だけ Keychain にあっても、`config.toml` に email が無ければ引けず Onboarding に落ちる（逆も同様）。**両方揃って初めて**認証がスキップされる。

## TUI への渡し方

環境変数でも CLI 引数でもなく、**起動時にディスクから読むだけ**。

1. `Config::load()` で `config.toml` から email を取得
2. `auth::load_token(email)` で Keychain から token を取得
3. `BitbucketClient::new(email, token)` を生成し `App::new(config, client)` に渡す
4. 各 API 呼び出しで HTTP Basic 認証として送る（`username = email`, `password = token`）

token は再起動をまたいで macOS Keychain に永続化されるため、2 回目以降の起動は Onboarding を経ずにそのまま使える。

## 通常の使い方（Onboarding）

手で仕込む必要はない。初回起動時に Onboarding 画面で **Email** と **Token** を入力すると:

1. `GET /2.0/user` で検証
2. 成功したら `save_token(email, token)` で Keychain へ、email/表示名を `config.toml` へ保存
3. 次回以降は自動復元

別マシンで使う場合も、初回に一度この Onboarding を通すだけでよい（認証情報はマシンごとに保存されるため移行は不要）。

## 手動で事前登録する場合（任意）

Onboarding を通さずに仕込むこともできる。**token と email の両方**を用意する。

```sh
# Keychain に token を登録
security add-generic-password -s bitbucket-tui -a "you@example.com" -w "<API_TOKEN>"

# config.toml に email を書く
mkdir -p ~/Library/Application\ Support/dev.bitbucket-tui
printf 'email = "you@example.com"\n' > ~/Library/Application\ Support/dev.bitbucket-tui/config.toml
```

ただし検証（`GET /2.0/user`）が走らないぶん、確実なのは Onboarding 経由。

## 認証情報の消去

```sh
bitbucket-tui logout       # Keychain の token と config.toml を削除
bitbucket-tui --reset-auth # 消去してから TUI を起動
```

## 関連コード

- `src/auth.rs` — Keychain への保存 / 読込 / 削除（`save_token` / `load_token` / `delete_token`）
- `src/config.rs` — `config.toml` の読み書き（email / display_name / default_workspace）
- `src/main.rs` — 起動時の復元（`restore_client`）と `logout`
- `src/api/client.rs` — HTTP Basic 認証での API 呼び出し

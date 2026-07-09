# bitbucket-tui 設計書 v0.1

## 変更履歴
- **v0.1 (2026-07-09)**: 初版。対象=Bitbucket Cloud、実装=Rust/ratatui、認証=API token(Basic)。M0〜M3ロードマップ確定。実装は Codex へ委譲。
- **v0.1.1 (2026-07-09)**: M1 実装に合わせ画面遷移を更新(`RepoSelected` を廃止し `PullRequests`→`PullRequestDetail`→`Diff` を追加)。詳細は `specs/M1.md` / `LEDGER.md`。

---

## 1. 概要

Bitbucket Cloud をキーボード駆動で操作する TUI クライアント。**PRレビューを主目的**とし、パイプライン監視・リポジトリブラウズを段階的に追加する。GitHub CLI の対話UI版に近い立ち位置を Bitbucket 向けに実現する。

## 2. 対象と前提

- **Bitbucket Cloud (bitbucket.org) のみ**。Data Center / Server は対象外(API仕様・認証が別物)。
- REST API 2.0: `https://api.bitbucket.org/2.0`
- **認証: API token(スコープ付き)による HTTP Basic**
  - username = **Atlassianアカウントのメールアドレス**(Bitbucketユーザー名やトークン名では通らない)
  - password = **API token**
  - App Password は **2026-07-28 に完全廃止**のため非対応。最初から API token 前提。
  - スコープ無しトークンは全エンドポイントで弾かれる。必要スコープ(M0〜M2想定):
    `account` / `repository` / `pullrequest` / `pullrequest:write` / `pipeline` / `pipeline:write`
- ページング: レスポンスは `{ values: [...], next: "<url>", page, size, pagelen }`。`next` を追跡。
- レート制限: 429 は `Retry-After` を尊重。

## 3. アーキテクチャ

- **単一バイナリクレート(M0)**。肥大化したら muster 同様 workspace 分割へ移行。
- **非同期: tokio**。描画は同期ループ、APIは tokio task で実行し **mpsc で結果をUIへ配送**(bubbletea の `Msg` パターンを Rust で再現)。
- モジュール構成:
  - `main` — CLI(clap)、tokio runtime、端末RAIIガード + panic hook、起動制御
  - `config` — 設定ファイル(directories + toml): `email`, `default_workspace`
  - `auth` — 認証情報(keyring): API token を OS Keychain に保存/読込/削除
  - `api` — Bitbucket クライアント(reqwest / Basic認証 / ページング / エラー型) + モデル(serde)
  - `tui` — App状態・画面遷移・イベントループ・描画・オンボーディング

```
[crossterm events] ─┐
                    ├─ tokio::select! ─> update(App) ─> render(ratatui)
[mpsc: ApiResult] ──┘
        ▲
        └─ spawn: reqwest → Bitbucket REST API 2.0
```

## 4. 画面遷移(全体像)

```
Onboarding(初回のみ) → Workspaces → Repositories → PullRequests(M1) → PullRequestDetail(M1) → Diff(M1)
                                          │                                    │
                                          │                                    ├─ approve/unapprove(a)
                                          │                                    ├─ request-changes/取消(x)
                                          │                                    ├─ 一般コメント投稿(c, Ctrl+S)
                                          │                                    └─ merge(M → 確認モーダル)
                                          ├─ Pipelines(M2)
                                          └─ Branches/Source(M3)
```

M1 実装済み(2026-07-09)。`Screen::RepoSelected` は廃止し、リポジトリ選択で `PullRequests`
(OPEN 既定、`o/m/d/a` で state 切替)をロードする。詳細は `docs/specs/M1.md` を参照。

## 5. マイルストーン

| 順 | 名称 | 内容 | 仕様書 |
|---|---|---|---|
| **M0** | 基盤 | 認証・APIクライアント・ページング・TUIスケルトン・workspace/repo選択 | `docs/specs/M0.md` |
| **M1** | PRレビュー(最優先) | PR一覧→詳細→diff→approve/request-changes/comment/merge | (M0完了後に起票) |
| **M2** | パイプライン監視 | pipeline一覧・状態・ステップログstream・再実行/停止 | 〃 |
| **M3** | リポジトリブラウズ | repo横断・ブランチ・コミット・ソース閲覧 | 〃 |

## 6. 体制

- 統括・設計判断: メインセッション(Opus/Fable)
- 実装: **Codex(codex plugin 経由)**。不可時は Sonnet サブエージェント
- 調査: Sonnet サブエージェント

## 7. 品質規約

`CLAUDE.md` を参照(fmt/clippy/test 常時グリーン、unwrap/expect禁止、TUI中stdout禁止、端末復元RAII+panic hook)。

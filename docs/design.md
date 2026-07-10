# bitbucket-tui 設計書 v0.1

## 変更履歴
- **v0.1 (2026-07-09)**: 初版。対象=Bitbucket Cloud、実装=Rust/ratatui、認証=API token(Basic)。M0〜M3ロードマップ確定。
- **v0.1.1 (2026-07-09)**: M1 実装に合わせ画面遷移を更新(`RepoSelected` を廃止し `PullRequests`→`PullRequestDetail`→`Diff` を追加)。詳細は `specs/M1.md` / `LEDGER.md`。
- **v0.1.2 (2026-07-10)**: M2 実装に合わせ画面遷移を更新(`Pipelines`→`PipelineDetail`→`StepLog` を追加、`Repositories` から `p`、`PullRequests` から `P` で入る。stop/re-run は確認モーダル、進行中は自動ポーリング更新)。詳細は `specs/M2.md` / `LEDGER.md`。
- **v0.1.3 (2026-07-10)**: M3 実装に合わせ画面遷移を更新(`Branches`→`Commits`→`CommitDetail`→`Diff`(流用) / `Source`→`FileView` を追加、`Repositories`/`PullRequests` から `b`=Branches・`s`=Source。commit diff は M1 の Diff、FileView は M2 の logview を流用。閲覧専用)。**ロードマップ M0〜M3 すべて実装完了**。詳細は `specs/M3.md` / `LEDGER.md`。
- **v0.1.4 (2026-07-10)**: M4(差分レビュー強化)実装。Diff 画面に現在行カーソル(`↑↓/jk`/`Shift+J/K`/`PgUp/PgDn`/`g/G`/`n/N` が「現在行」を動かし自動スクロール、ハイライト+位置表示)を追加し、PR 差分のみ `c` でインラインコメント投稿(`Ctrl+S` 送信/`Esc` 取消。コミット差分では拒否)を実装。詳細は `LEDGER.md`(M4 実装メモ)。

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
  - スコープ無しトークンは全エンドポイントで弾かれる。必要スコープ（"with scopes" の**粒度スコープ**。
    `write` は `read` を含意しないため両方必要）:
    `read:user:bitbucket` / `read:workspace:bitbucket` / `read:repository:bitbucket` /
    `read:pullrequest:bitbucket` / `write:pullrequest:bitbucket` /
    `read:pipeline:bitbucket` / `write:pipeline:bitbucket`
    （旧 `account`/`repository`/`pullrequest:write` 等は App password/OAuth 用の別体系）
- ページング: レスポンスは `{ values: [...], next: "<url>", page, size, pagelen }`。`next` を追跡。
- レート制限: 429 は `Retry-After` を尊重。

## 3. アーキテクチャ

- **単一バイナリクレート(M0)**。肥大化したら Cargo workspace 分割へ移行。
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
Onboarding(初回のみ) → Workspaces → Repositories ─Enter→ PullRequests(M1) → PullRequestDetail(M1) → Diff(M1)
                                       │ │ │ │              │              │
                                       │ │ │ │              │(P/b/s)       ├─ approve/unapprove(a)
                                       │ │ │ │              │              ├─ request-changes/取消(x)
                                       │ │ │ │              ▼              ├─ 一般コメント投稿(c, Ctrl+S)
                                       │ │ └──(p)──→ Pipelines(M2) ─Enter→ PipelineDetail(M2) → StepLog(M2)
                                       │ │                 │                  │
                                       │ │                 ├─ stop(S → 確認モーダル)
                                       │ │                 ├─ re-run(R → 確認モーダル)
                                       │ │                 └─ 自動ポーリング更新(進行中・a で ON/OFF)
                                       │ ├──(b)──→ Branches(M3) ─Enter→ Commits(M3) ─Enter→ CommitDetail(M3) ─d→ Diff(M1流用)
                                       │ │              └──(s)──┐                                    (Esc→CommitDetail)
                                       │ └──(s)──→ Source(M3) ◄─┘ ─Enter→(dir)潜る / (file)→ FileView(M3, logview流用)
                                       │                └─ Backspace/Esc: 親へ(ルートで Repositories)
```

M1 実装済み(2026-07-09)。`Screen::RepoSelected` は廃止し、リポジトリ選択で `PullRequests`
(OPEN 既定、`o/m/d/a` で state 切替)をロードする。詳細は `docs/specs/M1.md` を参照。

M2 実装済み(2026-07-10)。`Repositories` で `p`=Pipelines(既存 `Enter`=PullRequests は不変)、
`PullRequests` で `P`=同 repo の Pipelines。`Pipelines`→`PipelineDetail`→`StepLog` を追加し、
進行中パイプラインは自動ポーリングで更新、stop/re-run は確認モーダル経由。詳細は
`docs/specs/M2.md` を参照。

M3 実装済み(2026-07-10)。`Repositories`/`PullRequests` で `b`=Branches・`s`=Source(既定ブランチ
のルート)。`Branches`→`Commits`→`CommitDetail`→`Diff`(M1 の Diff ビューアを流用、`Esc` で戻り先を
出し分け) / `Source`(ディレクトリ潜行・親戻り)→`FileView`(M2 の logview ページャを流用、
バイナリ/巨大は代替表示)を追加。閲覧専用。詳細は `docs/specs/M3.md` を参照。**M0〜M3 完了**。

## 5. マイルストーン

| 順 | 名称 | 内容 | 仕様書 |
|---|---|---|---|
| **M0** | 基盤 | 認証・APIクライアント・ページング・TUIスケルトン・workspace/repo選択 | `docs/specs/M0.md` |
| **M1** | PRレビュー(最優先) | PR一覧→詳細→diff→approve/request-changes/comment/merge | `docs/specs/M1.md` |
| **M2** | パイプライン監視 | pipeline一覧・状態・ステップログ・再実行/停止・自動ポーリング | `docs/specs/M2.md` |
| **M3** | リポジトリブラウズ | ブランチ・コミット履歴・コミット差分・ソースツリー/ファイル閲覧(閲覧専用) | `docs/specs/M3.md` |

## 6. 品質規約

`CLAUDE.md` を参照(fmt/clippy/test 常時グリーン、unwrap/expect禁止、TUI中stdout禁止、端末復元RAII+panic hook)。

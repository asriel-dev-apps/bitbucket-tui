# bitbucket-tui 設計書 v0.1

## 変更履歴

- **v0.1.12 (2026-07-16)**: M6 のレビュー体験を実運用フィードバックで拡張。(1) Diff 本文を「表示行モデル」(`DiffState.display_rows`)へ刷新し、`↑↓` がコメントにも乗るようにした(コメント無し時は diff 行と 1:1 のフォールバック)。(2) コメントをスレッド単位の枠(ボックス)で囲み、split でも新側カラムに表示。(3) カーソルで選んだコメントに `r`返信 / `e`編集 / `d`削除(確認モーダル) / `R`解決トグル(API: PUT/DELETE/resolve)。(4) ファイル一覧サイドバーに変更種別マーカー(M/A/D/R)。Like はスコープ外。詳細は `specs/M6.md` / `LEDGER.md`。
- **v0.1.11 (2026-07-16)**: M6(Diff レビュー体験の強化)実装。Diff 本文(unified)の該当行直下にインラインコメントのスレッド(著者/日付/本文、返信は 1 段インデント)を差し込み表示し、`r` でスレッドへ返信できるようにした(`create_reply` は `parent` を送る。オフライン検証のみ)。スクロールは `scroll`/`cursor` を `parsed.lines` 添字に保ったまま、コメント行を「視覚行」として勘定する `max_scroll`/`ensure_cursor_visible` に拡張(コメント無し・split では従来式に一致)。ファイル一覧サイドバーをフォルダ階層ツリー(単一子フォルダは圧縮)+ ファイル毎の `+追加 -削除` とコメント数バッジへ刷新し、表示行と `parsed.files` 添字の写像でナビ・クリック・ハイライトを一貫させた。詳細は `specs/M6.md` / `LEDGER.md`。
- **v0.1.10 (2026-07-11)**: M5 の 5.4 を実装し M5 完了。端末のマウスキャプチャを RAII/panic hook の復元対象に加え、毎フレームの App-owned layout table によるペイン・一覧・モーダル・ヒントのヒットテスト、3 行ホイール、クリック選択/再クリック決定、概要リンク/画像クリックを追加。破壊的確認はクリック決定不可とした。
- **v0.1.9 (2026-07-11)**: M5 の 5.2 を実装。概要を自前 grapheme wrap のリッチドキュメントへ移行し、取得可能な本文画像を URL ごとの `StatefulProtocol` でインライン表示。仮想文書高による scroll clamp と、将来のクリック判定用リンク位置表を追加した。5.4（マウス）は未着手。
- **v0.1.8 (2026-07-11)**: M5 の 5.5 を実装。PR 添付画像が Bitbucket signin へ転送された場合は API token で取得不能である旨と `o` によるブラウザ表示を案内し、外部ホストの画像取得では Authorization を送らないようにした。当時 5.2/5.4 は未実装（5.2 は v0.1.9 で完了）。
- **v0.1.7 (2026-07-11)**: M5 のうち 5.1/5.3 を実装。PR 詳細に概要/変更ファイル/コメントのフォーカス巡回とフォーカス先スクロールを追加し、概要・コメントの上限を `Paragraph::line_count` による折り返し後行数で正確化。`L` で本文・コメントのリンクパレットを開けるようにした。5.2/5.4/5.5 は未実装。
- **v0.1 (2026-07-09)**: 初版。対象=Bitbucket Cloud、実装=Rust/ratatui、認証=API token(Basic)。M0〜M3ロードマップ確定。
- **v0.1.1 (2026-07-09)**: M1 実装に合わせ画面遷移を更新(`RepoSelected` を廃止し `PullRequests`→`PullRequestDetail`→`Diff` を追加)。詳細は `specs/M1.md` / `LEDGER.md`。
- **v0.1.2 (2026-07-10)**: M2 実装に合わせ画面遷移を更新(`Pipelines`→`PipelineDetail`→`StepLog` を追加、`Repositories` から `p`、`PullRequests` から `P` で入る。stop/re-run は確認モーダル、進行中は自動ポーリング更新)。詳細は `specs/M2.md` / `LEDGER.md`。
- **v0.1.3 (2026-07-10)**: M3 実装に合わせ画面遷移を更新(`Branches`→`Commits`→`CommitDetail`→`Diff`(流用) / `Source`→`FileView` を追加、`Repositories`/`PullRequests` から `b`=Branches・`s`=Source。commit diff は M1 の Diff、FileView は M2 の logview を流用。閲覧専用)。**ロードマップ M0〜M3 すべて実装完了**。詳細は `specs/M3.md` / `LEDGER.md`。
- **v0.1.4 (2026-07-10)**: M4(差分レビュー強化)実装。Diff 画面に現在行カーソル(`↑↓/jk`/`Shift+J/K`/`PgUp/PgDn`/`g/G`/`n/N` が「現在行」を動かし自動スクロール、ハイライト+位置表示)を追加し、PR 差分のみ `c` でインラインコメント投稿(`Ctrl+S` 送信/`Esc` 取消。コミット差分では拒否)を実装。詳細は `LEDGER.md`(M4 実装メモ)。
- **v0.1.5 (2026-07-11)**: PR 本文の画像をターミナル内に表示する `Screen::ImageView` を追加(PR 詳細で `i`、`n`/`p`/`←→` で巡回、`Esc` で戻る)。初版時点では `ratatui-image` のネイティブ画像プロトコル(Sixel/Kitty/iTerm2)は本クレートの `ratatui`(0.29)と非互換なバージョン(`^0.30.1`)へ依存するため採用できず、`image` クレート(新規追加)によるデコード+自前のハーフブロック(`▀`/`▄`)描画で代替した。
- **v0.1.6 (2026-07-11)**: 同日中に `ratatui-image` を `11.0.6`→`8.1.1`(`ratatui = "^0.29"` 依存で本クレートと同一インスタンスに解決される版)へ差し替え、`StatefulImage`/`StatefulProtocol` によるネイティブ画像プロトコル描画へ移行した(自前のハーフブロック描画は削除。端末が非対応でも `ratatui-image` 自身が内蔵ハーフブロックへ自動フォールバックする)。詳細は `LEDGER.md`(画像表示 実装メモ / 画像表示 実装メモ: ratatui-image 8.1.1 移行)。

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

画像表示 実装済み(2026-07-11)。`PullRequestDetail` で `i`=ImageView（本文の `![alt](url)` 画像を
`n`/`p`/`←→` で巡回表示、`Esc` で `PullRequestDetail` へ戻る）を追加。`ratatui-image` 8.1.1 の
`StatefulImage`/`StatefulProtocol` によるネイティブ画像プロトコル（Sixel/Kitty/iTerm2、非対応
端末では `ratatui-image` 自身が内蔵ハーフブロックへ自動フォールバック）で描画する（初版は
バージョン非互換のため自前のハーフブロック描画で代替していたが、同日中に 8.1.1 へ移行し解消）。
詳細は `LEDGER.md`(画像表示 実装メモ / 画像表示 実装メモ: ratatui-image 8.1.1 移行)を参照。

M5 5.2 実装済み(2026-07-11)。`PullRequestDetail` の概要だけを `tui::richdoc` に移し、ヘッダを
先頭 Text block、本文をコードフェンス外の `![alt](url)` で Text/Image block に分割する。
Text は grapheme/表示セル幅で事前 wrap、画像は取得成功時のみ `StatefulImage` + `Resize::Crop`
で最大 20 行表示し、合計仮想高さでスクロールをクランプする。コメントペインは従来の
`Paragraph::line_count` のまま。リンク位置表は `App` のレイアウト表と組み合わせ、スクロール補正した概要リンクのクリック判定に使う。

M5 5.4 実装済み(2026-07-11)。crossterm の mouse capture を初期化し、描画時に確定した Rect と一覧の先頭表示行を `App::layout` へ毎フレーム書き戻す。入力側は `Down(Left)` / `ScrollUp` / `ScrollDown` だけを受け、既存のキー/Enter ハンドラへ委譲する。

### キーマップ / マウスマップ

| 入力 | 対象 | 動作 |
|---|---|---|
| ホイール | PR 詳細の概要/コメント、Diff 本文、StepLog、FileView | カーソル下を 1 ノッチ 3 行移動 |
| ホイール | 全一覧、PR 変更ファイル、Diff ファイル一覧、リンク/ジャンプパレット | 選択を 3 件移動 |
| 左クリック | PR 詳細の 3 ペイン | クリックしたペインへフォーカス移動 |
| 左クリック | 一覧行 | 行を選択。選択済み行は `Enter` 相当（既存ハンドラを使用） |
| 左クリック | PR 概要のリンク/画像 | リンクをブラウザで開く / 対応画像を ImageView で開く |
| 左クリック | フッターの操作ヒント | 表示された対応キーと同じ操作 |
| 左クリック | モーダル外 | `Esc` 相当で閉じる |
| 左クリック | merge / stop / re-run 確認内 | 決定しない。実行は `Enter` のみ |

マウスキャプチャ中の端末ネイティブなテキスト選択は `Shift+ドラッグ`。tmux 等のターミナルマルチプレクサではマウスイベント/修飾キーのパススルー実装に依存する。

## 5. マイルストーン

| 順 | 名称 | 内容 | 仕様書 |
|---|---|---|---|
| **M0** | 基盤 | 認証・APIクライアント・ページング・TUIスケルトン・workspace/repo選択 | `docs/specs/M0.md` |
| **M1** | PRレビュー(最優先) | PR一覧→詳細→diff→approve/request-changes/comment/merge | `docs/specs/M1.md` |
| **M2** | パイプライン監視 | pipeline一覧・状態・ステップログ・再実行/停止・自動ポーリング | `docs/specs/M2.md` |
| **M3** | リポジトリブラウズ | ブランチ・コミット履歴・コミット差分・ソースツリー/ファイル閲覧(閲覧専用) | `docs/specs/M3.md` |

## 6. 品質規約

`CLAUDE.md` を参照(fmt/clippy/test 常時グリーン、unwrap/expect禁止、TUI中stdout禁止、端末復元RAII+panic hook)。

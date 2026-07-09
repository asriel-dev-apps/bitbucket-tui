# 実装台帳 (LEDGER)

作業開始時に必ず読み、完了時に更新する。

## マイルストーン状況

- **M0 基盤**: **実装完了(2026-07-09)**。`cargo build/clippy(--all-targets -D warnings)/fmt --check/test` すべて green(--offline)。ユニット22件 pass + ネットワーク依存の smoke テスト1件 `#[ignore]`。実 API 結合確認(`GET /2.0/user`)は環境に実 token が無いためスキップ(下記の未検証の仮定は据え置き)。
- **M1 PRレビュー**: **実装完了(2026-07-09)**。`cargo build/clippy(--all-targets -D warnings)/fmt --check/test` すべて green(--offline)。ユニット54件 pass(+22→54) + `#[ignore]` 1件。`RepoSelected` を廃し repo 選択→PR一覧→詳細→Diff(色付きスクロール)→approve/unapprove・request-changes/取消・一般コメント投稿・merge(確認モーダル+strategy 選択+close source branch)を実装。**実 API 結合確認は環境に実 token が無いためスキップ**(PR/Comment/DiffStat の serde フィールド・state 値・merge 202・inline 位置は下記「未検証の仮定」のまま。build+clippy+モックテストのみで検証)。
- **M2 パイプライン監視**: **実装完了(2026-07-10)**。`cargo build/clippy(--all-targets -D warnings)/fmt --check/test` すべて green(--offline)。ユニット93件 pass(+54→93) + `#[ignore]` 1件。repo から Pipelines 一覧(状態色)→PipelineDetail(ステップ一覧)→StepLog(スクロール/擬似 tail)、stop/re-run(確認モーダル)、進行中パイプラインの自動ポーリング更新(tokio timer tick、`a` で ON/OFF、全完了で停止)を実装。**実 API 結合確認は環境に実 token が無いためスキップ**(Pipeline/Step の serde フィールド・state/result 値・log 404 挙動・trigger body の正確形・pipelines エンドポイントの trailing slash は下記「未検証の仮定」のまま。build+clippy+モックテストのみで検証)。
- **M3 リポジトリブラウズ**: **実装完了(2026-07-10)**。`cargo build/clippy(--all-targets -D warnings)/fmt --check/test` すべて green(--offline)。ユニット136件 pass(+93→136) + `#[ignore]` 1件。repo/PR から Branches 一覧→Commits 履歴→CommitDetail→Diff(M1 流用) / Source(ツリー閲覧: ディレクトリ潜り/親戻り)→FileView(内容ページャ: logview 流用)を実装。閲覧専用。**実 API 結合確認は環境に実 token が無いためスキップ**(Branch/Commit/SrcEntry の serde フィールド・src の dir/file 応答形・diff spec・branch 名/パスの URL エンコード方針は下記「未検証の仮定」のまま。build+clippy+モックテストのみで検証)。
- **ロードマップ M0〜M3 すべて実装完了(2026-07-10)**。

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

## M1 実装メモ (2026-07-09)

- **モジュール拡張**: `api/models.rs` に `PullRequest`/`Participant`/`BranchRef`/`Branch`/`Commit`/`RenderedText`/`Comment`/`CommentContent`/`Inline`/`CommentParent`/`DiffStatEntry`/`PathEntry`/`MergeStrategy`/`MergeParams` を追加(id 以外は `Option`/`#[serde(default)]` で耐性)。`api/client.rs` に `list_pull_requests`(states 複数指定=`state` 繰り返しクエリ)/`get_pull_request`/`get_pr_diff`(生テキスト)/`get_pr_diffstat`/`list_comments`/`approve`/`unapprove`/`request_changes`/`unrequest_changes`/`create_comment`/`merge_pull_request` と、共通ヘルパ `send_get_text`/`send_empty`(POST/DELETE ボディ無)/`send_json`/`send_json_discard`/`send_json_text` + `comment_body` を追加。`tui/diff.rs` を新規追加(ユニファイド diff の行分類+着色、ファイル境界追跡)。`tui/app.rs` に `Screen`{PullRequests,PullRequestDetail,Diff}・`Msg`/`Command` 拡張・`PrStateFilter`/`Me`/`MergeModal`/`CommentEditor`/`DiffState` を追加。`tui/ui.rs`/`tui/event.rs` を対応拡張。
- **Elm パターン維持**: `update()->Command`、非同期は `event::dispatch` が `tokio::spawn`→`Msg`。詳細を開くと `Command::Batch([LoadPrDetail, LoadDiffStat, LoadComments])` を発行(**新規に `Command::Batch(Vec<Command>)` を導入**し dispatch が再帰展開)。approve/request-changes/merge/comment 成功後は該当 `LoadPrDetail`/`LoadComments` を再発行して状態を反映。
- **diff 着色は手動**(syntect 不使用): 行頭で `+`=緑/`-`=赤/`@@`=シアン/`diff --git`=黄(bold)/`index`等メタ=淡色/context=既定。`+++`/`---` は追加/削除より先にメタ判定。`str::lines()` で末尾空行を出さない。ファイル境界は `diff --git`(無ければ `--- ` にフォールバック)を `n`/`N` ジャンプに使用。Diff のスクロールは `Paragraph::scroll`、上限は描画時に確定した viewport で算出(`DiffState.viewport` を毎フレーム更新)。
- **破壊的操作の確認**: merge は必ず確認モーダル(`MergeModal`)経由。`M`=モーダルを開くだけ(merge しない)、モーダル内 `Enter` で初めて `Command::Merge`。strategy は `←/→/Tab` 巡回、close source branch は `Space` トグル。approve/request-changes は即時トグルで結果を `Status::Success` に表示(**`Status` に `Success` を追加**)。
- **自分の承認状態判定はベストエフォート**: participant の `user` を `Me`{account_id,uuid,display_name} と照合(uuid/account_id/display_name のいずれか一致)。再起動時は `GET /2.0/user` を再取得しないため display_name のみで照合になり得る。誤判定しても merge 後の再取得で表示は補正される。→ **未検証**(実 participant の識別フィールドが不明)。
- **コメントエディタ**: 複数行の簡易バッファ(末尾追記/backspace/Enter=改行のみ、任意位置編集は非対応)。`Ctrl+S`=送信 / `Esc`=取消。ボディは `{"content":{"raw":".."}}`。inline 投稿は未実装(stretch。一覧では inline アンカー `path:line` を表示)。
- **large_enum_variant 回避**: `Msg::PrDetailLoaded` の `PullRequest` は `Box` 化(他の巨大 variant は無し)。
- **未実施**: 実 token が無いため approve→diff→comment→merge の実結合確認はしていない。build+clippy(-D warnings)+モックテストのみ green。

## M2 実装メモ (2026-07-10)

- **モジュール拡張**: `api/models.rs` に `Pipeline`/`PipelineStep`/`PipelineState`/`NamedRef`/`PipelineTarget`/`PipelineSelector`/`PipelineStatus`(enum) と、状態判定 `classify_pipeline_status`・所要時間整形 `format_duration_secs`・re-run ボディ生成 `PipelineTarget::trigger_body` を追加(`uuid` 以外は `Option`/`#[serde(default)]` で耐性)。`api/client.rs` に `list_pipelines`/`get_pipeline`/`list_pipeline_steps`/`get_step_log`(text)/`stop_pipeline`/`trigger_pipeline` と **percent-encode ヘルパ `percent_encode`**(+`hex_digit`)を追加。`api/error.rs` に `ApiError::is_not_found`(404 判定)。`tui/logview.rs` を新規追加(`LogView`: スクロール状態 + ANSI/制御文字除去 `strip_ansi`/`sanitize_log`)。`tui/app.rs` に `Screen`{Pipelines,PipelineDetail,StepLog}・`PipelineAction`/`ConfirmModal`・`Msg`/`Command` 拡張・ポーリング tick 処理を追加。`tui/ui.rs`/`tui/event.rs` を対応拡張。
- **uuid の波括弧エンコード(既知の罠)**: `pipeline_uuid`/`step_uuid` は `{...}` 込みの文字列。URL 化する箇所(`get_pipeline`/`list_pipeline_steps`/`get_step_log`/`stop_pipeline`)で `percent_encode` を通し、`{`→`%7B`/`}`→`%7D` にエンコードする(unreserved `A-Za-z0-9-._~` 以外を全て `%XX` 化)。素の `{...}` だと `The value provided is not a valid uuid` になる。ユニットテストで検証済み。
- **自動ポーリング(監視の肝)**: イベントループの `tokio::select!` に `tokio::time::interval(5s)` の tick 枝を追加し、`Msg::Tick` を流す。最初の即時 tick は起動直後の無駄打ちを避けて捨てる。`update()` の `on_tick` が「自動更新 ON かつ Pipelines/PipelineDetail 画面かつ進行中(PENDING/IN_PROGRESS/BUILDING)がある」ときだけ静かな再取得コマンドを発行し、**全完了で自然停止**(発火しなくなる)。`a` キーで自動更新 ON/OFF をトグル。手動 `r` は常時可。tick→リフレッシュ発行はモックテストで検証。
- **状態→色**: `PipelineStatus`(Successful/Failed/InProgress/Stopped/Pending/Unknown)へ `classify_pipeline_status` で丸め、`ui` 側で 緑/赤/黄/DarkGray/Gray/Reset にマップ。result 名優先(完了時)→state 名の順で判定。値は大文字化して寛容にマッチ。
- **破壊的操作の確認**: stop/re-run は M1 の merge モーダルと同じく `ConfirmModal` 経由。`S`/`R` はモーダルを開くだけ、モーダル内 `Enter` で初めて `Command::StopPipeline`/`TriggerPipeline`。stop は進行中パイプラインのみ(完了済みは拒否してステータスにエラー)。**確認なしには走らない**(モックテストで検証)。実行成功後は `auto_refresh=true` にして静かに再取得(Loading を出さず成功メッセージを残す。M1 の `MergeDone`→`refresh_detail` と同じ挙動)。re-run 成功後は新しい実行が先頭に出るため一覧へ遷移。
- **ステップログ**: `text/plain` を全取得(Range 末尾取得は未実装=任意)。404 は `is_not_found` で判定し `Msg::StepLogLoaded{text:None}`→`LogView::missing`→「(ログなし)」表示。ANSI エスケープ(CSI)と `\r` 等の制御文字は `strip_ansi` で除去(タブ・改行は温存)。スクロールは M1 の diff/pager と同じ操作(`↑↓/jk`・`PgUp/PgDn`・`g/G`)。`r` で再取得(擬似 tail、同一ステップならスクロール位置を維持)。
- **ナビゲーション**: `Repositories` で `p`=Pipelines(既存 `Enter`=PullRequests はそのまま)。`PullRequests` で `P`=同 repo の Pipelines。`Pipelines`→`Esc`=Repositories、`PipelineDetail`→`Esc`=Pipelines、`StepLog`→`Esc`=PipelineDetail。`review_context()`(client+workspace+repo slug)をパイプライン系でも再利用。
- **一覧の選択維持**: 自動ポーリングで一覧/ステップが毎回先頭に戻らないよう、`SelectList::set_items_keep_selection`(選択インデックスを新件数にクランプ)を追加してリフレッシュ時に使用。
- **large_enum_variant 回避**: `Msg::PipelineLoaded` の `Pipeline` は `Box` 化(M1 の `PrDetailLoaded` と同様)。
- **未実施**: 実 token が無いため一覧自動更新→ログ閲覧→stop→re-run の実結合確認はしていない。build+clippy(-D warnings)+モックテストのみ green。

## M3 実装メモ (2026-07-10)

- **モジュール拡張**: `api/models.rs` に `SrcEntry`{entry_type("commit_file"/"commit_directory"),path,size?,mimetype?}・`CommitAuthor`{raw?,user?} を追加し、既存 `Branch`(→ name + `target:Option<Commit>`)/`Commit`(→ hash + message/date/author/parents)/`Repository`(→ `mainbranch:Option<Branch>`) を後方互換に拡張(追加フィールドは全て `Option`/`#[serde(default)]`)。表示ヘルパ(`Commit::short_hash/summary/author_name/parent_short_hashes`、`Branch::target_*`、`SrcEntry::is_dir/name/path_str`、`Repository::main_branch_name`)を追加。`api/client.rs` に `list_branches`/`list_commits`(revision 可省略)/`get_commit`/`get_commit_diff`(text)/`list_src`(dir 列挙)/`get_src_file`(text) と **パス用エンコード `encode_path`**(`percent_encode` と違い `/` を温存)を追加。`tui/logview.rs` に `LogView::from_file`・`looks_binary`・`MAX_FILE_LINES` を追加(FileView へ流用)。`tui/app.rs` に `Screen`{Branches,Commits,CommitDetail,Source,FileView}・`SourceState`・`Msg`/`Command` 拡張・遷移ロジックを追加。`tui/ui.rs`/`tui/event.rs` を対応拡張。
- **既存モデルの拡張方針**: 新規 `Branch`/`Commit` を別途作らず、PR/pipeline が使う既存 `Branch`/`Commit` を拡張して一本化(spec のモデル名に合わせる)。追加フィールドは optional なので PR/pipeline のデシリアライズは不変。`Commit` は `parents: Vec<Commit>`(Vec 経由の再帰で size 有限)。この拡張で `Commit` が肥大化し、`PipelineTarget`(commit を内包)を持つ `Command::TriggerPipeline` が **clippy `large_enum_variant`** を踏んだため、`target` を `Box<PipelineTarget>` に変更して解消(M1/M2 の `Box` 化と同方針)。
- **Diff の共用**: commit 差分は M1 の `DiffState`/diff パーサ/着色をそのまま流用。`Screen::Diff` は PR と commit で共有し、`App.diff_return`(戻り先 Screen)で `Esc` の遷移先を出し分ける(PR→PullRequestDetail / commit→CommitDetail)。commit diff は PR id で照合できないため専用の `Command::LoadCommitDiff`/`Msg::CommitDiffLoaded{spec,text}` を追加し、`current_commit.hash == spec` で照合。CommitDetail/PR detail 遷移時に `diff` を None リセットして取り違えを防ぐ。
- **FileView の共用**: M2 の `LogView` ページャを流用。`LogView::from_file(key,title,mimetype,content)` が **バイナリ判定 `looks_binary`**(内容の NUL バイト or 既知バイナリ mimetype)で「(バイナリ表示不可)」を出し、巨大は先頭 `MAX_FILE_LINES`(=5000)行 + 注記で打切り。**判定は sanitize 前の生テキストで行う**(sanitize が NUL を落とすため順序が重要)。照合キー(`step_uuid` フィールドを流用)にファイルパスを入れる。mimetype は開いた `SrcEntry.mimetype` を `App.open_file_mimetype` に持って FileLoaded 時に使用。
- **Source の潜り/戻り**: `SrcEntry.path` はルートからのフルパス前提。ディレクトリへ潜るときは `entry.path` をそのまま新 path に採用、親へは純粋関数 `parent_dir`(末尾 `/` 無視、ルート=空文字は `None`→repo へ戻る)で算出。列挙は受信時に `sort_src_entries`(ディレクトリ→ファイル、各グループ名前昇順)で整列。dir/file の出し分けは `SrcEntry::is_dir`(`type=="commit_directory"`)で判定し、dir は `list_src`(JSON)、file は `get_src_file`(text)を呼ぶ。
- **既定ブランチ**: repo 一覧応答の `mainbranch.name` を `Repository::main_branch_name` で取得し、`select_repo` 時に `App.repo_main_branch` へ保持。Source ルートはこれを使い、未取得時は `"main"` にフォールバック(**未検証の仮定**。master 等の repo では要 revision 指定)。
- **ナビゲーション**: `Repositories` で `b`=Branches / `s`=Source root(既定ブランチ)。`Enter`=PR・`p`=Pipelines は不変。`PullRequests` からも `b`/`s`。Branches: `Enter`=Commits・`s`=そのブランチの Source root・`r`=再読込。Commits: `Enter`=CommitDetail・`r`=再読込。CommitDetail: `d`=Diff・`↑↓/PgUp/PgDn`=メッセージスクロール。Source: `Enter`=潜る/開く・`Backspace`/`Esc`=親(ルートで repo)・`r`=再読込。FileView: `↑↓/jk PgUp/PgDn g/G`=スクロール・`Esc`=Source。各画面 `q`/`Ctrl+C`/`?` 踏襲。
- **未実施**: 実 token が無いため Branches→Commits→CommitDetail→Diff / Source 潜行→FileView の実結合確認はしていない。build+clippy(-D warnings)+モックテストのみ green。

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
- **(M1) PR/Comment/DiffStat の serde フィールド**: 仕様書の推定名で実装(`PullRequest.participants[].{approved,state,role}`、`DiffStatEntry.{status,lines_added,lines_removed,old.path,new.path}`、`Comment.{content.raw,inline.{path,from,to},deleted}` 等)。実 API 初回応答で有無/名称/値(特に `participant.state` の `changes_requested` 表記、`status` の `modified/added/removed/renamed`、PR `state` の `OPEN/MERGED/DECLINED/SUPERSEDED`)を確定し、モデルとフィルタ判定を補正する。
- **(M1) participant の自己識別フィールド**: uuid/account_id/display_name のどれが participant.user に含まれるか未確定。承認トグルの POST/DELETE 判定に使用。
- **(M1) merge の非同期応答(202)**: 大きな merge は 202 + タスクポーリングになり得る。現状は成功ステータス扱いで「マージしました」を表示し PR を再取得するのみ(ポーリング未実装)。実挙動を確認して要否を判断。
- **(M1) diff エンドポイントのリダイレクト/Content-Type**: `.../diff` は `text/plain` 前提で生テキスト取得。実際のリダイレクト有無・エンコーディングを確認。
- **(M2) Pipeline/Step の serde フィールド**: 仕様書の推定名で実装(`Pipeline.{uuid,build_number,state{name,result{name},stage{name}},creator,created_on,completed_on,target{type,ref_type,ref_name,commit{hash},selector{type,pattern}},trigger{name},duration_in_seconds}`、`PipelineStep.{uuid,name,state,started_on,completed_on,duration_in_seconds}`)。実 API 初回応答で有無/名称を確定する。
- **(M2) state/result 値**: pipeline `state.name`=`PENDING`/`IN_PROGRESS`/`BUILDING`/`COMPLETED`/`PAUSED`/`HALTED`/`STOPPED` 等、`result.name`=`SUCCESSFUL`/`FAILED`/`ERROR`/`STOPPED` 等を推定して `classify_pipeline_status` でマッチ(大文字化・寛容判定)。**進行中判定は PENDING/IN_PROGRESS/BUILDING/RUNNING**。実値(特に BUILDING/RUNNING の実在、result の正確な語彙)を確認して補正。
- **(M2) pipelines エンドポイントの trailing slash**: list/detail/steps/trigger すべて `/pipelines/`(末尾スラッシュ)で実装。仕様書は list を `/pipelines`(スラッシュ無)と表記。実 API でどちらが正か(リダイレクト/404 有無)を確認。
- **(M2) trigger(re-run) ボディの正確形**: `{"target":{"type":"pipeline_ref_target","ref_type":..,"ref_name":..,"selector":{"type":"default"|..}}}` を元 target から再構成(commit は送らずブランチ先端を再実行)。実 API で必須/任意フィールド・selector 種別(default/custom/pull_requests 等)を確認。custom selector の pattern は引き継ぐ実装。
- **(M2) ステップログの 404/Range/Content-Type**: ログ未生成は 404 と仮定し「(ログなし)」表示。巨大ログの Range 末尾取得は未実装(全取得)。実際の 404 条件・`Content-Type`・進行中の追記挙動を確認。
- **(M2) stop の応答**: `POST .../stopPipeline` は成功可否のみ扱い(ボディ未使用)、成功後は静かに再取得。実際のステータスコード(202 等)・非同期性を確認。
- **(M3) Branch/Commit/SrcEntry の serde フィールド**: 仕様書の推定名で実装(`Branch.{name,target{hash,date,message,author}}`、`Commit.{hash,message,date,author{raw,user},parents[]{hash}}`、`SrcEntry.{type,path,size,mimetype}`)。実 API 初回応答で有無/名称(特に `type` の `commit_directory`/`commit_file` 表記、`author.raw` の形式、`refs/branches` の `target` 形)を確定する。
- **(M3) src エンドポイントの dir/file 応答形**: `GET .../src/{commit}/{path}` は path がディレクトリなら JSON 列挙(ページング)、ファイルなら生バイト、という前提。判定は列挙側の `SrcEntry.type` で行い、ファイルは別メソッドで生テキスト取得。実際の Content-Type・リダイレクト・ルート(`.../src/{commit}/`)の trailing slash 挙動・ページング有無を確認。
- **(M3) revision / path の URL エンコード**: `commits/{revision}`・`src/{commit}/{path}`・`diff/{spec}`・`commit/{hash}` は `encode_path`(unreserved + `/` を温存し他を `%XX`)でエンコード。ブランチ名の `/`(例 `feature/x`)は素のパスセパレータとして送る前提。実 API で `/` を含むブランチ名・特殊文字パスが正しく解決されるか(要 `%2F` 化かどうか)を確認。
- **(M3) commit diff の spec**: `GET .../diff/{hash}` に単一ハッシュを渡して当該コミットの差分を取得(親との差分)前提。M1 の diff パーサ/着色をそのまま流用。実際のリダイレクト/Content-Type/マージコミットの差分表現を確認。
- **(M3) 既定ブランチのフォールバック**: repo 一覧の `mainbranch.name` を使い、未取得時は `"main"` にフォールバック。`master` 等の repo では Source root が 404 になり得る(その場合はステータス行にエラー表示・Branches から明示選択で回避)。repo 詳細取得によるフォールバックは未実装。
- **(M3) バイナリ/巨大ファイル判定**: `reqwest` の `text()`(lossy UTF-8)後の NUL バイト有無 + mimetype でバイナリ判定、`MAX_FILE_LINES`(5000)超で先頭打切り。実バイナリ応答が本当にテキストとして返る(=生バイトが text() を通る)のか、`?format=meta` を使うべきか等は実挙動で確認。size ヘッダによる事前打切りは未実装(全取得後に行数で打切り)。

## 未解決の問い

- OAuth 2.0 対応の要否(現状 API token のみで足りる想定)。
- M1 の PR 横断取得に最適なエンドポイント(repo単位 `.../pullrequests` の集約 vs 他)。→ **M1 では repo 単位 `.../pullrequests` を採用**(選択リポジトリ内の PR をレビューする体験を優先)。repo 横断は後続で検討。
- (M1) inline コメント投稿(stretch)の要否と、inline アンカーの `from`/`to`(旧/新ファイル行)の正確な意味。一覧表示のみ実装済み、投稿は未実装。
- (M1) コメント一覧・PR 一覧のページング UI(「さらに読み込む」)。現状は `get_paged` の安全上限(20 ページ)まで自動集約するのみで、明示的な追加読み込み UI は未実装。
- (M2) 自動ポーリング間隔は固定 5 秒。実運用で適正か(レート制限との兼ね合い)、可変にすべきかは実挙動で判断。tick は常時 5 秒ごとに `Msg::Tick` を流すが、対象画面かつ進行中がある時のみ API を叩く設計。
- (M2) `q` の終了スコープ: `StepLog`/`Diff` では `q`=終了、`Pipelines`/`PipelineDetail` でも `q`=終了(一覧系画面の共通踏襲)。監視中に誤終了しやすいなら要再検討。
- (M2) BBQL フィルタ(`q=` によるブランチ/状態絞り込み)は未実装(既定の新しい順一覧のみ)。ステップ単位再実行・キャッシュ/アーティファクト操作・真のログストリーミングも後回し(スコープ外)。
- (M3) ページングは `get_paged` の安全上限(20 ページ)まで自動集約するのみで、明示的な追加読み込み UI は未実装(Branches/Commits/Source 共通)。ブランチ/コミットが大量にある repo での「もっと読む」体験は後続で検討。
- (M3) シンタックスハイライトなし(ファイルはプレーン表示、diff のみ M1 の +/- 着色)。blame・検索・tags・branching model 詳細・ファイル/ブランチ編集は全てスコープ外(閲覧専用)。
- (M3) Source の `s`(Branches/Repositories からのルート)と Branches の `Enter`(Commits)の使い分けは、既定ブランチ以外を見るときに一度 Branches を経由する導線。repo 直下から任意 revision の Source/Commits に飛ぶショートカット(revision 入力)は未実装。
- (M3) 実 token での結合未確認のため、`docs/specs/M3.md` 検証方法の手動一巡(ブランチ→履歴→コミット差分、Source 潜行→ファイル表示)は未実施。実施時に判明したフィールド/挙動を上記「未検証の仮定」へ反映する。

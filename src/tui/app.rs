//! アプリ状態・画面遷移・`update()`。
//!
//! bubbletea の `Model`/`Msg`/`Cmd` に相当する構造。`update()` は状態を更新し、副作用を
//! [`Command`] として返す。実際の非同期実行（API 呼び出しの spawn）は `event` モジュールが行う。

use std::collections::{HashMap, VecDeque};

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use ratatui::widgets::ListState;

use crate::api::{
    ApiError, BitbucketClient, Branch, Comment, Commit, DiffStatEntry, ListSort, MergeParams,
    MergeStrategy, PageInfo, Pipeline, PipelineStep, PipelineTarget, PullRequest, Repository,
    SrcEntry, User, Workspace,
};
use crate::auth;
use crate::config::Config;
use crate::tui::diff::{ParsedDiff, parse as parse_diff};
use crate::tui::logview::LogView;
use crate::tui::onboarding::{Field, OnboardingState, TextInput};
use crate::tui::theme::{Theme, ThemeName};

/// 画面種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Workspaces,
    Repositories,
    PullRequests,
    PullRequestDetail,
    Diff,
    Pipelines,
    PipelineDetail,
    StepLog,
    Branches,
    Commits,
    CommitDetail,
    Source,
    FileView,
}

/// PR 一覧の state フィルタ。
///
/// `Hash` は PR 一覧キャッシュ（[`RevisitCache`]）のキーの一部として使うために導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrStateFilter {
    Open,
    Merged,
    Declined,
    All,
}

impl PrStateFilter {
    /// API へ渡す `state` 値の並び（`All` は全状態を明示）。
    pub fn states(self) -> &'static [&'static str] {
        match self {
            PrStateFilter::Open => &["OPEN"],
            PrStateFilter::Merged => &["MERGED"],
            PrStateFilter::Declined => &["DECLINED"],
            PrStateFilter::All => &["OPEN", "MERGED", "DECLINED", "SUPERSEDED"],
        }
    }

    /// UI 表示ラベル。
    pub fn label(self) -> &'static str {
        match self {
            PrStateFilter::Open => "OPEN",
            PrStateFilter::Merged => "MERGED",
            PrStateFilter::Declined => "DECLINED",
            PrStateFilter::All => "ALL",
        }
    }
}

/// 現在ログイン中のユーザー識別子（自分の承認状態を判定するために使う）。
///
/// 再起動時は `GET /2.0/user` を再取得しないため `display_name` のみになり得る。実 API の
/// participant フィールド（uuid/account_id）の一致で自分を特定するが、いずれも未検証のため
/// ベストエフォート判定とする。
#[derive(Debug, Clone, Default)]
pub struct Me {
    pub account_id: Option<String>,
    pub uuid: Option<String>,
    pub display_name: Option<String>,
}

/// merge 確認モーダルの状態。
#[derive(Debug, Clone)]
pub struct MergeModal {
    /// `MergeStrategy::ALL` へのインデックス。
    pub strategy: usize,
    pub close_source_branch: bool,
    pub submitting: bool,
}

impl MergeModal {
    fn new(close_source_branch: bool) -> Self {
        Self {
            strategy: 0,
            close_source_branch,
            submitting: false,
        }
    }

    /// 現在選択中のマージ戦略。
    pub fn strategy(&self) -> MergeStrategy {
        MergeStrategy::ALL[self.strategy % MergeStrategy::ALL.len()]
    }

    fn cycle_strategy(&mut self) {
        self.strategy = (self.strategy + 1) % MergeStrategy::ALL.len();
    }
}

/// パイプラインへの破壊的操作の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineAction {
    /// 未完了ステップを停止する。
    Stop,
    /// 元 target で再実行する。
    Rerun,
}

impl PipelineAction {
    /// 確認モーダルの見出し。
    pub fn title(self) -> &'static str {
        match self {
            PipelineAction::Stop => "パイプライン停止の確認",
            PipelineAction::Rerun => "パイプライン再実行の確認",
        }
    }

    /// 確認モーダルの本文（破壊的操作の説明）。
    pub fn description(self) -> &'static str {
        match self {
            PipelineAction::Stop => "破壊的操作: 実行中のステップを停止します。",
            PipelineAction::Rerun => "破壊的操作: 同じ target でパイプラインを再実行します。",
        }
    }
}

/// stop / re-run の確認モーダル状態（M1 の merge モーダルと同じ「確認しないと実行しない」仕組み）。
#[derive(Debug, Clone)]
pub struct ConfirmModal {
    pub action: PipelineAction,
    /// 対象パイプラインの uuid（波括弧込み）。
    pub pipeline_uuid: String,
    /// 表示用のビルドラベル（`#123`）。
    pub build_label: String,
    /// re-run 用に元 target を引き継ぐ（stop では使わない）。
    pub target: Option<PipelineTarget>,
    pub submitting: bool,
}

impl ConfirmModal {
    /// 対象パイプラインに対する確認モーダルを作る。
    fn new(action: PipelineAction, pipeline: &Pipeline) -> Self {
        Self {
            action,
            pipeline_uuid: pipeline.uuid.clone(),
            build_label: pipeline.build_label(),
            target: pipeline.target.clone(),
            submitting: false,
        }
    }
}

/// 一般コメント投稿の簡易エディタ状態。
#[derive(Debug, Clone, Default)]
pub struct CommentEditor {
    pub text: String,
    pub submitting: bool,
}

impl CommentEditor {
    fn is_submittable(&self) -> bool {
        !self.text.trim().is_empty()
    }
}

/// Diff 画面内のフォーカス（ファイル一覧サイドバー / 本文）。`Tab` で切り替える。
///
/// 既定は `Body`（サイドバー導入前の挙動＝矢印キーが直接本文をスクロールする、を維持する
/// ため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffFocus {
    /// ファイル一覧サイドバー。↑↓/jk でファイル選択（本文の `scroll` が追従する）。
    Files,
    /// 差分本文。既存の ↑↓/jk PgUp/PgDn g/G スクロール。
    #[default]
    Body,
}

/// Diff 画面の表示状態（スクロール・ファイル境界ジャンプ・サイドバー選択）。
#[derive(Debug, Clone, Default)]
pub struct DiffState {
    pub parsed: ParsedDiff,
    /// 先頭からのスクロール行数。
    pub scroll: usize,
    /// 直近描画時のビューポート高さ（スクロール上限計算に使う。`ui` が毎フレーム更新）。
    pub viewport: usize,
    /// 見出し（例: `#12`）。
    pub title: String,
    /// 着色済み行（`ratatui::text::Line`）の遅延キャッシュ。
    ///
    /// `parsed` から毎フレーム再構築すると全行ぶんのヒープ確保が走るため、初回描画時に
    /// `ui` 側が一度だけ構築して書き戻す。新しい diff をロードした際は `DiffState` 自体を
    /// 作り直す（`rendered_lines: None` で始まる）ため、別途の無効化ロジックは不要。
    pub rendered_lines: Option<Vec<Line<'static>>>,
    /// サイドバーで選択中のファイルインデックス（`parsed.files` へのインデックス）。
    ///
    /// `next_file`/`prev_file`（`n`/`N`）とサイドバー選択（`Tab` でフォーカス移動後の
    /// ↑↓/jk）の双方から更新され、常に `scroll` と同期する（本文側からジャンプしても
    /// サイドバーの選択が追従し、その逆も成り立つ）。
    pub file_index: usize,
    /// 画面内フォーカス（ファイル一覧 / 本文）。
    pub focus: DiffFocus,
}

impl DiffState {
    fn max_scroll(&self) -> usize {
        self.parsed.len().saturating_sub(self.viewport.max(1))
    }

    fn scroll_down(&mut self, amount: usize) {
        self.scroll = (self.scroll + amount).min(self.max_scroll());
    }

    fn scroll_up(&mut self, amount: usize) {
        self.scroll = self.scroll.saturating_sub(amount);
    }

    fn scroll_to_top(&mut self) {
        self.scroll = 0;
    }

    fn scroll_to_bottom(&mut self) {
        self.scroll = self.max_scroll();
    }

    /// 次のファイル境界へジャンプする（`n`）。サイドバー選択（`file_index`）も同期する。
    fn next_file(&mut self) {
        if let Some((index, &start)) = self
            .parsed
            .file_starts
            .iter()
            .enumerate()
            .find(|&(_, &start)| start > self.scroll)
        {
            self.scroll = start.min(self.max_scroll());
            self.file_index = index;
        }
    }

    /// 前のファイル境界へジャンプする（`N`）。サイドバー選択（`file_index`）も同期する。
    fn prev_file(&mut self) {
        if let Some((index, &start)) = self
            .parsed
            .file_starts
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start < self.scroll)
        {
            self.scroll = start;
            self.file_index = index;
        }
    }

    /// サイドバーで指定インデックスのファイルを選択し、本文の `scroll` を先頭行に合わせる。
    /// 範囲外は何もしない（パニックしない）。
    fn select_file(&mut self, index: usize) {
        if let Some(file) = self.parsed.files.get(index) {
            self.file_index = index;
            self.scroll = file.start.min(self.max_scroll());
        }
    }

    /// サイドバー選択を 1 つ下へ（末尾で停止）。
    fn select_file_next(&mut self) {
        let len = self.parsed.files.len();
        if len == 0 {
            return;
        }
        self.select_file((self.file_index + 1).min(len - 1));
    }

    /// サイドバー選択を 1 つ上へ（先頭で停止）。
    fn select_file_prev(&mut self) {
        if self.parsed.files.is_empty() {
            return;
        }
        self.select_file(self.file_index.saturating_sub(1));
    }

    /// Diff 画面内のフォーカスを切り替える（`Tab`）。
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            DiffFocus::Files => DiffFocus::Body,
            DiffFocus::Body => DiffFocus::Files,
        };
    }
}

/// Source（ソースツリー閲覧）画面の状態。
///
/// 現在の `reference`（ブランチ名/ハッシュ）と `path`（ルートからのディレクトリパス。
/// 空文字がルート）を保持し、ディレクトリ列挙を選択リストで表示する。
#[derive(Debug, Default)]
pub struct SourceState {
    pub reference: String,
    pub path: String,
    pub entries: SelectList<SrcEntry>,
}

impl SourceState {
    /// ヘッダ表示用の `ref:/path` 文字列。
    pub fn location(&self) -> String {
        format!("{}:/{}", self.reference, self.path)
    }
}

/// ステータス行の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading(String),
    Success(String),
    Error(String),
}

/// イベントループから `update()` へ渡されるメッセージ。
#[derive(Debug)]
pub enum Msg {
    /// キー入力。
    Key(KeyEvent),
    /// Onboarding の認証検証に成功（`GET /2.0/user`）。
    AuthValidated {
        email: String,
        token: String,
        user: User,
    },
    /// Onboarding の認証検証に失敗。
    AuthFailed(ApiError),
    /// ワークスペース一覧（1 ページ分）の取得完了。
    WorkspacesLoaded {
        workspaces: Vec<Workspace>,
        page_info: PageInfo,
    },
    /// リポジトリ一覧（1 ページ分）の取得完了。
    RepositoriesLoaded {
        workspace: String,
        /// リクエスト時に指定していたソート順（ステイル判定・キャッシュキーに使う）。
        sort: ListSort,
        repos: Vec<Repository>,
        page_info: PageInfo,
    },
    /// ワークスペース/リポジトリ取得の失敗。
    LoadFailed(ApiError),
    /// PR 一覧（1 ページ分）の取得完了。
    PullRequestsLoaded {
        repo: String,
        filter: PrStateFilter,
        /// リクエスト時に指定していたソート順（ステイル判定・キャッシュキーに使う）。
        sort: ListSort,
        prs: Vec<PullRequest>,
        page_info: PageInfo,
    },
    /// PR 詳細の取得完了（承認状態の再反映にも使う）。
    PrDetailLoaded { id: u64, pr: Box<PullRequest> },
    /// diffstat の取得完了。
    DiffStatLoaded {
        id: u64,
        entries: Vec<DiffStatEntry>,
    },
    /// コメント一覧の取得完了。
    CommentsLoaded { id: u64, comments: Vec<Comment> },
    /// diff テキストの取得完了。
    DiffLoaded { id: u64, text: String },
    /// approve/request-changes 系アクションの成功。
    ReviewActionDone { id: u64, message: String },
    /// コメント投稿の成功。
    CommentPosted { id: u64 },
    /// merge の成功（202 の「処理中」を含む）。
    MergeDone { id: u64 },
    /// レビュー系アクション（approve/comment/merge 等）の失敗。
    ActionFailed(ApiError),
    /// 自動ポーリングのタイマ tick（進行中パイプラインの定期リフレッシュ）。
    Tick,
    /// パイプライン一覧の取得完了。
    PipelinesLoaded {
        repo: String,
        pipelines: Vec<Pipeline>,
    },
    /// パイプライン詳細の取得完了。
    PipelineLoaded {
        uuid: String,
        pipeline: Box<Pipeline>,
    },
    /// パイプラインステップ一覧の取得完了。
    PipelineStepsLoaded {
        uuid: String,
        steps: Vec<PipelineStep>,
    },
    /// ステップログの取得完了（`text` が `None` なら 404＝ログなし）。
    StepLogLoaded {
        step_uuid: String,
        text: Option<String>,
    },
    /// stop / re-run の成功。
    PipelineActionDone { action: PipelineAction },
    /// ブランチ一覧（1 ページ分）の取得完了。
    BranchesLoaded {
        repo: String,
        branches: Vec<Branch>,
        page_info: PageInfo,
    },
    /// コミット履歴の取得完了。
    CommitsLoaded {
        revision: Option<String>,
        commits: Vec<Commit>,
    },
    /// コミット詳細の取得完了。
    CommitDetailLoaded { hash: String, commit: Box<Commit> },
    /// コミット差分テキストの取得完了。
    CommitDiffLoaded { spec: String, text: String },
    /// ソースのディレクトリ列挙の取得完了。
    SourceLoaded {
        reference: String,
        path: String,
        entries: Vec<SrcEntry>,
    },
    /// ソースファイル内容の取得完了。
    FileLoaded { path: String, text: String },
}

/// `update()` が返す副作用の指示。実行は `event` モジュールが担う。
#[derive(Debug)]
pub enum Command {
    /// 何もしない。
    None,
    /// アプリ終了。
    Quit,
    /// 複数コマンドをまとめて実行する。
    Batch(Vec<Command>),
    /// email+token を検証する（`GET /2.0/user`）。
    ValidateAuth { email: String, token: String },
    /// ワークスペース一覧の指定ページを取得する（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    LoadWorkspaces { client: BitbucketClient, page: u32 },
    /// 指定ワークスペースのリポジトリ一覧の指定ページを、指定ソート順で取得する。
    LoadRepositories {
        client: BitbucketClient,
        workspace: String,
        sort: ListSort,
        page: u32,
    },
    /// PR 一覧の指定ページを、指定ソート順で取得する。
    LoadPullRequests {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        filter: PrStateFilter,
        sort: ListSort,
        page: u32,
    },
    /// PR 詳細を取得する。
    LoadPrDetail {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
    },
    /// diffstat を取得する。
    LoadDiffStat {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
    },
    /// diff テキストを取得する。
    LoadDiff {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
    },
    /// コメント一覧を取得する。
    LoadComments {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
    },
    /// approve（`approve=true`）/ unapprove（`false`）。
    Approve {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        approve: bool,
    },
    /// request-changes（`request=true`）/ 取消（`false`）。
    RequestChanges {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        request: bool,
    },
    /// 一般コメントを投稿する。
    CreateComment {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        raw: String,
    },
    /// PR をマージする。
    Merge {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        params: MergeParams,
    },
    /// パイプライン一覧を取得する。
    LoadPipelines {
        client: BitbucketClient,
        workspace: String,
        repo: String,
    },
    /// パイプライン詳細を取得する。
    LoadPipeline {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        uuid: String,
    },
    /// パイプラインステップ一覧を取得する。
    LoadPipelineSteps {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        uuid: String,
    },
    /// ステップログを取得する。
    LoadStepLog {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        pipeline_uuid: String,
        step_uuid: String,
    },
    /// パイプラインを停止する。
    StopPipeline {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        uuid: String,
    },
    /// パイプラインを再実行する。
    ///
    /// `target` は [`Commit`] を内包し大きめのため、enum サイズ抑制のため `Box` 化する。
    TriggerPipeline {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        target: Box<PipelineTarget>,
    },
    /// ブランチ一覧の指定ページを取得する（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    LoadBranches {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        page: u32,
    },
    /// コミット履歴を取得する（`revision` 省略時は既定ブランチ）。
    LoadCommits {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        revision: Option<String>,
    },
    /// コミット詳細を取得する。
    LoadCommitDetail {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        hash: String,
    },
    /// コミット差分テキストを取得する。
    LoadCommitDiff {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        spec: String,
    },
    /// ソースのディレクトリ列挙を取得する。
    LoadSource {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        reference: String,
        path: String,
    },
    /// ソースファイル内容を取得する。
    LoadFile {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        reference: String,
        path: String,
    },
}

/// 選択状態を持つリスト。ratatui の `ListState` を内包し、スクロールは List ウィジェットに委ねる。
///
/// `matches` は `items` のうちフィルタ（`filter`）を通過した要素のインデックスを表示順で
/// 保持する。`filter` が空文字なら `matches` は恒等写像（`0..items.len()`）になるため、
/// 検索を使わない画面（pipelines/branches/commits/source 等）は `filter`/`matches` に
/// 一切触れないままで従来と同じ挙動（全件・`items` のインデックスそのままの選択）になる。
/// 選択（`state`）は常に「`matches` 上の位置」を指す。
///
/// `T: Default` を要求しないよう `Default` は手動実装する。
#[derive(Debug)]
pub struct SelectList<T> {
    pub items: Vec<T>,
    pub state: ListState,
    /// 検索フィルタ文字列（空ならフィルタなし）。検索を使わない画面では常に空のまま。
    pub filter: String,
    /// フィルタ通過した `items` のインデックス（表示順）。`ui` の一覧描画はこれを辿る。
    pub matches: Vec<usize>,
}

impl<T> Default for SelectList<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            matches: Vec::new(),
        }
    }
}

impl<T> SelectList<T> {
    /// 要素を差し替え、選択位置を先頭（空なら未選択）にリセットする。フィルタも解除する
    /// （新しいデータセットに古いフィルタを引き継がないため）。
    pub fn set_items(&mut self, items: Vec<T>) {
        self.filter.clear();
        self.matches = (0..items.len()).collect();
        self.state
            .select(if items.is_empty() { None } else { Some(0) });
        self.items = items;
    }

    /// 要素を差し替えつつ、選択インデックスを可能な限り維持する（新しい件数にクランプ）。
    ///
    /// 自動ポーリングでの一覧リフレッシュ時に、選択位置が毎回先頭へ戻らないようにするために使う。
    /// 検索を使わない画面専用（`filter` は常に空のまま・`matches` は恒等写像を保つ）。
    pub fn set_items_keep_selection(&mut self, items: Vec<T>) {
        self.matches = (0..items.len()).collect();
        let selection = if items.is_empty() {
            None
        } else {
            Some(self.state.selected().unwrap_or(0).min(items.len() - 1))
        };
        self.state.select(selection);
        self.items = items;
    }

    /// 要素を差し替えつつ、`key_fn` で選択中要素と一致するものを新しい一覧から探して選択する
    /// （見つからなければ現在のインデックスを新件数にクランプする＝`set_items_keep_selection`
    /// と同じフォールバック）。
    ///
    /// 同一文脈（同じ画面/フィルタ/ページ/ソート）の再検証結果が届いたときに使う。並び順が
    /// 変わり得る場合（サーバソート下で他者の更新により順序が変動する等）でも、識別子
    /// （slug/full_name/PR id 等）で同一アイテムを追従できるため、単純なインデックス維持より
    /// 頑健。検索を使わない画面専用（`set_items_keep_selection` と同じ制約）。
    pub fn set_items_keep_selection_by<F, K>(&mut self, items: Vec<T>, key_fn: F)
    where
        F: Fn(&T) -> K,
        K: PartialEq,
    {
        let current_key = self.selected().map(&key_fn);
        self.matches = (0..items.len()).collect();
        let selection = current_key
            .and_then(|key| items.iter().position(|item| key_fn(item) == key))
            .or_else(|| {
                if items.is_empty() {
                    None
                } else {
                    Some(self.state.selected().unwrap_or(0).min(items.len() - 1))
                }
            });
        self.state.select(selection);
        self.items = items;
    }

    /// 検索フィルタ文字列を更新し、`key_fn` が返す文字列（大文字小文字は無視）に対する
    /// 部分一致で `matches` を再計算する。選択位置は新しい `matches` の範囲にクランプする。
    pub fn set_filter<F>(&mut self, filter: String, key_fn: F)
    where
        F: Fn(&T) -> String,
    {
        self.filter = filter;
        self.recompute_matches(key_fn);
    }

    fn recompute_matches<F>(&mut self, key_fn: F)
    where
        F: Fn(&T) -> String,
    {
        if self.filter.is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let needle = self.filter.to_lowercase();
            self.matches = self
                .items
                .iter()
                .enumerate()
                .filter(|(_, item)| key_fn(item).to_lowercase().contains(&needle))
                .map(|(index, _)| index)
                .collect();
        }
        let selection = if self.matches.is_empty() {
            None
        } else {
            Some(
                self.state
                    .selected()
                    .unwrap_or(0)
                    .min(self.matches.len() - 1),
            )
        };
        self.state.select(selection);
    }

    /// 選択を 1 つ下へ（末尾で停止）。フィルタ未使用なら全件、使用中なら `matches` の範囲。
    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let next = match self.state.selected() {
            Some(index) if index + 1 < self.matches.len() => index + 1,
            Some(index) => index,
            None => 0,
        };
        self.state.select(Some(next));
    }

    /// 選択を 1 つ上へ（先頭で停止）。
    pub fn select_prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let prev = match self.state.selected() {
            Some(0) | None => 0,
            Some(index) => index - 1,
        };
        self.state.select(Some(prev));
    }

    /// 選択を `amount` 件下へ（末尾でクランプ）。`Shift+J` の 10 件移動に使う。
    pub fn select_next_by(&mut self, amount: usize) {
        if self.matches.is_empty() {
            return;
        }
        let next = match self.state.selected() {
            Some(index) => (index + amount).min(self.matches.len() - 1),
            None => 0,
        };
        self.state.select(Some(next));
    }

    /// 選択を `amount` 件上へ（先頭でクランプ）。`Shift+K` の 10 件移動に使う。
    pub fn select_prev_by(&mut self, amount: usize) {
        if self.matches.is_empty() {
            return;
        }
        let prev = match self.state.selected() {
            Some(index) => index.saturating_sub(amount),
            None => 0,
        };
        self.state.select(Some(prev));
    }

    /// 現在選択中の要素（`matches` 上の位置を `items` のインデックスへ変換して引く）。
    pub fn selected(&self) -> Option<&T> {
        let position = self.state.selected()?;
        let index = *self.matches.get(position)?;
        self.items.get(index)
    }

    /// 表示順（フィルタ適用後）で要素を辿るイテレータ。`ui` の一覧描画に使う。
    pub fn visible(&self) -> impl Iterator<Item = &T> + '_ {
        self.matches.iter().map(move |&index| &self.items[index])
    }
}

/// 直近開いた PR（ジャンプパレットの候補に使う）。
#[derive(Debug, Clone)]
pub struct RecentPr {
    pub workspace: String,
    /// `workspace/repo` 形式（`App::selected_repo` と同じ形式）。
    pub repo_full_name: String,
    pub pr: PullRequest,
}

/// ジャンプパレットの候補が実行する遷移。
#[derive(Debug, Clone)]
pub enum JumpAction {
    /// 画面だけを切り替える（保持済みデータをそのまま使う）。
    Screen(Screen),
    /// 指定ワークスペースへ入る（未取得ならリポジトリ取得を発行）。
    Workspace(String),
    /// 指定リポジトリへ入る（PR 一覧取得を発行）。
    Repository(Box<Repository>),
    /// 直近開いた PR の詳細へ直接ジャンプする。
    RecentPr(Box<RecentPr>),
}

/// ジャンプパレットの 1 候補。
#[derive(Debug, Clone)]
pub struct JumpEntry {
    /// 一覧に表示するラベル。
    pub label: String,
    /// 検索対象文字列（`label` より緩く、複数の呼び名を含められる）。
    pub search_key: String,
    pub action: JumpAction,
}

/// ジャンプパレット（`Ctrl+K`）の状態。保持済みデータへの一気ジャンプ・画面ジャンプに使う。
#[derive(Debug)]
pub struct JumpPaletteState {
    pub entries: SelectList<JumpEntry>,
}

/// ページ番号ジャンプ（`g`）の入力状態。Workspaces/Repositories/PullRequests 共通。
///
/// 数字のみを受け付ける簡易バッファ（`on_key_page_jump` 参照）。`Enter` で確定し
/// [`App::goto_page`] を呼ぶ、`Esc` で取消。
#[derive(Debug, Clone, Default)]
pub struct PageJumpModal {
    pub input: String,
}

/// `JumpEntry` の検索キー抽出（`SelectList::set_filter`/`reorder` に渡す）。
fn jump_entry_key(entry: &JumpEntry) -> String {
    entry.search_key.clone()
}

/// 1 キャッシュ（[`RevisitCache`]）あたりの最大保持件数。
///
/// 超過時は最も古いキー（挿入順）を 1 件追い出す（FIFO）。ワークスペース/リポジトリ/PR は
/// セッション中に無制限に増え得るため、素の `HashMap` のままだと長時間のレビューセッションで
/// メモリが際限なく肥大する。真の LRU（アクセス順追跡）は実装コストの割に本用途では過剰と
/// 判断し、挿入順ベースの単純な FIFO 追い出しのみを採用する。
const REVISIT_CACHE_MAX_ENTRIES: usize = 30;

/// 画面再訪時の stale-while-revalidate キャッシュ用コンテナ。
///
/// 「再訪時にキャッシュがあれば即表示し、裏で再取得して届いたら差し替える」ために使う
/// 単純なキー・バリューストア。挿入順を [`VecDeque`] で追跡し、
/// [`REVISIT_CACHE_MAX_ENTRIES`] を超えたら最も古いキーを追い出す。
#[derive(Debug)]
pub struct RevisitCache<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for RevisitCache<K, V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K, V> RevisitCache<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    /// キーに対応する値への参照を返す（キャッシュ命中判定に使う）。
    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// キーに対応する値への可変参照を返す（部分更新の upsert に使う）。
    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    /// 挿入/上書きする。新規キーの追加で上限を超える場合は最も古いキーを 1 件追い出す
    /// （既存キーの上書きは追い出しを起こさない＝挿入順は保持する）。
    fn insert(&mut self, key: K, value: V) {
        if !self.map.contains_key(&key) {
            if self.order.len() >= REVISIT_CACHE_MAX_ENTRIES
                && let Some(oldest) = self.order.pop_front()
            {
                self.map.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }
        self.map.insert(key, value);
    }

    /// 述語 `keep` が `false` を返すキーをすべて取り除く。
    ///
    /// キーの一部（例: repo slug）だけが一致すればページ番号を問わず一括無効化したい場合に使う
    /// （[`App::invalidate_pull_requests_cache_for_current_repo`] 参照。ページ番号までは
    /// 追跡していないため、`remove` を全ページ分呼ぶ代わりにこちらを使う）。
    fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&K) -> bool,
    {
        let removed: Vec<K> = self
            .order
            .iter()
            .filter(|key| !keep(key))
            .cloned()
            .collect();
        for key in removed {
            self.map.remove(&key);
        }
        self.order.retain(|key| keep(key));
    }
}

/// PR 詳細キャッシュの 1 エントリ。詳細・diffstat・コメントをまとめて保持し、
/// [`App::open_pr_detail_with`] での即時表示と 3 種の `Msg::*Loaded` からの部分更新に使う。
#[derive(Debug, Clone)]
pub struct PrDetailCache {
    pub pr: PullRequest,
    pub diffstat: Vec<DiffStatEntry>,
    pub comments: Vec<Comment>,
}

/// PR 一覧の再訪キャッシュの型（[`App::pull_requests_cache`]）。
///
/// キー（repo slug, state フィルタ, ソート, ページ番号）のタプルが 4 要素になり
/// `clippy::type_complexity` に抵触するため、型エイリアスへ切り出している。
type PullRequestsCache =
    RevisitCache<(String, PrStateFilter, ListSort, u32), (Vec<PullRequest>, PageInfo)>;

/// アプリ全体の状態。
pub struct App {
    pub screen: Screen,
    pub config: Config,
    pub client: Option<BitbucketClient>,
    /// 現在の配色テーマ（`ThemeName` から導出。`Ctrl+T` で巡回）。
    pub theme: Theme,
    /// 現在のテーマ名（`config.theme` の永続化・巡回の起点に使う）。
    pub theme_name: ThemeName,
    pub me: Me,
    pub onboarding: OnboardingState,
    pub workspaces: SelectList<Workspace>,
    /// Workspaces 一覧のページ状態（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    /// `[`/`]` でページ間移動、`g` でページ番号ジャンプ。
    pub workspaces_page_info: PageInfo,
    /// ワークスペース一覧の再訪キャッシュ（キー = ページ番号）。`[`/`]`/`g` でのページ移動と
    /// 画面再訪の双方で即時表示に使い、`Msg::WorkspacesLoaded` 受信のたびに最新化する
    /// （stale-while-revalidate）。
    pub workspaces_cache: RevisitCache<u32, (Vec<Workspace>, PageInfo)>,
    pub repositories: SelectList<Repository>,
    /// Repositories 一覧の現在のサーバソート順（`S` キーで巡回。Bitbucket ブラウザ版と同じ
    /// 4 種類）。切り替え時は 1 ページ目から再取得する（[`App::cycle_repositories_sort`]）。
    pub repositories_sort: ListSort,
    /// Repositories 一覧のページ状態。
    pub repositories_page_info: PageInfo,
    /// リポジトリ一覧の再訪キャッシュ（キー = (workspace slug, ソート, ページ番号)）。
    /// [`App::load_repositories_page`] が再訪時に即表示するために使い、`Msg::RepositoriesLoaded`
    /// 受信のたびに最新化する（stale-while-revalidate）。
    pub repositories_cache: RevisitCache<(String, ListSort, u32), (Vec<Repository>, PageInfo)>,
    pub selected_workspace: Option<String>,
    pub selected_repo: Option<String>,
    /// 選択リポジトリの既定ブランチ名（`mainbranch.name`）。Source ルートに使う。
    pub repo_main_branch: Option<String>,
    pub pull_requests: SelectList<PullRequest>,
    /// PullRequests 一覧の現在のサーバソート順（`S` キーで巡回）。切り替え時は 1 ページ目から
    /// 再取得する（[`App::cycle_pull_requests_sort`]）。
    pub pull_requests_sort: ListSort,
    /// PullRequests 一覧のページ状態。
    pub pull_requests_page_info: PageInfo,
    /// PR 一覧の再訪キャッシュ（キー = (repo slug, state フィルタ, ソート, ページ番号)）。
    /// `repo` は `Msg::PullRequestsLoaded` が運ぶ値（`review_context()` 由来の repo slug）に
    /// 合わせており、既存のステイル判定ガード（`repo_slug()` 一致チェック）と精度を揃えている
    /// （workspace をまたいだ同名 repo slug の衝突は既存ガードと同じ既知の制約）。
    pub pull_requests_cache: PullRequestsCache,
    pub pr_state_filter: PrStateFilter,
    pub current_pr: Option<PullRequest>,
    /// PR 詳細の再訪キャッシュ（キー = (repo full_name, PR id)）。
    /// [`App::open_pr_detail_with`] が即時表示に使い、`Msg::PrDetailLoaded`/`DiffStatLoaded`/
    /// `CommentsLoaded` の受信ごとに該当フィールドだけ部分更新する。
    pub pr_detail_cache: RevisitCache<(String, u64), PrDetailCache>,
    pub diffstat: SelectList<DiffStatEntry>,
    pub comments: Vec<Comment>,
    pub detail_scroll: u16,
    /// 直近描画時の PR 詳細本文のビューポート高さ（`detail_scroll` の上限計算に使う。
    /// `ui` が毎フレーム更新する。`DiffState::viewport` / `LogView::viewport` と同じ役割）。
    pub detail_viewport: usize,
    pub diff: Option<DiffState>,
    pub comment_editor: Option<CommentEditor>,
    pub merge_modal: Option<MergeModal>,
    pub pipelines: SelectList<Pipeline>,
    pub current_pipeline: Option<Pipeline>,
    pub pipeline_steps: SelectList<PipelineStep>,
    pub step_log: Option<LogView>,
    /// StepLog 画面で開いているステップの uuid（受信ログ・再取得の照合に使う）。
    pub open_step_uuid: Option<String>,
    pub confirm_modal: Option<ConfirmModal>,
    /// 進行中パイプラインの自動ポーリング更新が有効か。
    pub auto_refresh: bool,
    /// Diff 画面から `Esc` で戻る先（PR 詳細 or コミット詳細）。
    pub diff_return: Screen,
    /// Branches/Source を `Repositories`/`PullRequests` から開いたときの「戻り先」画面。
    /// 各画面での `b`/`s` キー押下時に記録し、Branches 画面の `Esc`・Source 画面のルートでの
    /// `Esc`/`Backspace`（親が無い＝これ以上遡れない）がこの戻り先を使う。Branches 経由で
    /// Source を開いた場合（Branches 画面の `s`）は更新しない（最初に入って来た画面を保つ）。
    pub browse_return: Screen,
    pub branches: SelectList<Branch>,
    /// Branches 一覧のページ状態。`[`/`]` でページ間移動、`g` でページ番号ジャンプ。
    pub branches_page_info: PageInfo,
    /// ブランチ一覧の再訪キャッシュ（キー = (repo slug, ページ番号)）。
    /// [`App::load_branches_page`] が再訪時に即表示するために使い、`Msg::BranchesLoaded`
    /// 受信のたびに最新化する（stale-while-revalidate）。
    pub branches_cache: RevisitCache<(String, u32), (Vec<Branch>, PageInfo)>,
    pub commits: SelectList<Commit>,
    /// Commits 画面が表示中の revision（ブランチ名/ハッシュ、既定は `None`）。
    pub commits_revision: Option<String>,
    pub current_commit: Option<Commit>,
    /// CommitDetail のメッセージスクロール量。
    pub commit_scroll: u16,
    pub source: Option<SourceState>,
    pub file_view: Option<LogView>,
    /// FileView で開いているファイルのパス（受信結果の照合キー）。
    pub open_file_path: Option<String>,
    /// 開いているファイルの mimetype（バイナリ判定に使う）。
    pub open_file_mimetype: Option<String>,
    pub status: Status,
    pub show_help: bool,
    /// Workspaces/Repositories/PullRequests でのインクリメンタル検索の編集中フラグ
    /// （`/` で開始、対象のリストは `screen` から決まる）。
    pub search_editing: bool,
    /// ジャンプパレット（`Ctrl+K`）の状態。開いている間は最優先でキー入力を奪う。
    pub jump_palette: Option<JumpPaletteState>,
    /// ページ番号ジャンプ（`g`）の入力状態。開いている間は最優先でキー入力を奪う。
    pub page_jump: Option<PageJumpModal>,
    /// 直近開いた PR（新しい順）。ジャンプパレットの候補に使う。
    pub recent_prs: Vec<RecentPr>,
}

/// PR 詳細本文の固定ヘッダ行数（`ui::render_pr_meta_body` が積む先頭 4 行:
/// タイトル / 状態・ブランチ / author 情報 / 空行）。`detail_scroll` の上限計算に使う。
/// `ui::render_pr_meta_body` のヘッダ構成を変えたらここも合わせて更新すること。
/// （承認/変更要求パネルの行数は可変なので別途 [`participant_panel_line_count`] で加算する）。
const PR_DETAIL_HEADER_LINES: usize = 4;

/// 承認/変更要求パネルの行数（`ui::render_pr_meta_body` が積む参加者パネルと対応させる）。
/// 承認者がいれば 1 行、変更要求者がいれば 1 行、どちらも無ければ 0 行。
fn participant_panel_line_count(pr: &PullRequest) -> usize {
    let mut lines = 0;
    if !pr.approved_names().is_empty() {
        lines += 1;
    }
    if !pr.changes_requested_names().is_empty() {
        lines += 1;
    }
    lines
}

impl App {
    /// 設定と（あれば）認証済みクライアントから初期状態を作る。
    ///
    /// `client` が `Some` のときは Onboarding をスキップできる。実際の画面確定は
    /// [`App::init_command`] で行う。
    pub fn new(config: Config, client: Option<BitbucketClient>) -> Self {
        let mut onboarding = OnboardingState::default();
        if let Some(email) = &config.email {
            onboarding.email = TextInput::from_str(email);
            onboarding.field.0 = Field::Token;
        }
        let me = Me {
            display_name: config.display_name.clone(),
            ..Me::default()
        };
        let theme_name = config
            .theme
            .as_deref()
            .map(ThemeName::from_config_str)
            .unwrap_or_default();
        let theme = theme_name.theme();
        Self {
            screen: Screen::Onboarding,
            config,
            client,
            theme,
            theme_name,
            me,
            onboarding,
            workspaces: SelectList::default(),
            workspaces_page_info: PageInfo::default(),
            workspaces_cache: RevisitCache::default(),
            repositories: SelectList::default(),
            repositories_sort: ListSort::default(),
            repositories_page_info: PageInfo::default(),
            repositories_cache: RevisitCache::default(),
            selected_workspace: None,
            selected_repo: None,
            repo_main_branch: None,
            pull_requests: SelectList::default(),
            pull_requests_sort: ListSort::default(),
            pull_requests_page_info: PageInfo::default(),
            pull_requests_cache: RevisitCache::default(),
            pr_state_filter: PrStateFilter::Open,
            current_pr: None,
            pr_detail_cache: RevisitCache::default(),
            diffstat: SelectList::default(),
            comments: Vec::new(),
            detail_scroll: 0,
            detail_viewport: 0,
            diff: None,
            comment_editor: None,
            merge_modal: None,
            pipelines: SelectList::default(),
            current_pipeline: None,
            pipeline_steps: SelectList::default(),
            step_log: None,
            open_step_uuid: None,
            confirm_modal: None,
            auto_refresh: true,
            diff_return: Screen::PullRequestDetail,
            browse_return: Screen::Repositories,
            branches: SelectList::default(),
            branches_page_info: PageInfo::default(),
            branches_cache: RevisitCache::default(),
            commits: SelectList::default(),
            commits_revision: None,
            current_commit: None,
            commit_scroll: 0,
            source: None,
            file_view: None,
            open_file_path: None,
            open_file_mimetype: None,
            status: Status::Idle,
            show_help: false,
            search_editing: false,
            jump_palette: None,
            page_jump: None,
            recent_prs: Vec::new(),
        }
    }

    /// 起動直後に実行すべきコマンドを返し、初期画面を確定する。
    ///
    /// 認証済みなら Workspaces へ進み 1 ページ目の取得を開始、未認証なら Onboarding に留まる。
    pub fn init_command(&mut self) -> Command {
        if self.client.is_some() {
            self.screen = Screen::Workspaces;
            return self.load_workspaces_page(1);
        }
        self.screen = Screen::Onboarding;
        Command::None
    }

    /// メッセージを適用し、必要な副作用を返す。
    pub fn update(&mut self, msg: Msg) -> Command {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::AuthValidated { email, token, user } => self.on_auth_validated(email, token, user),
            Msg::AuthFailed(error) => {
                self.onboarding.validating = false;
                self.onboarding.error = Some(error.to_string());
                Command::None
            }
            Msg::WorkspacesLoaded {
                workspaces,
                page_info,
            } => {
                // ページ単位でキャッシュを最新化する（他ページへ移動していても次回再訪時に活かす）。
                self.workspaces_cache
                    .insert(page_info.page, (workspaces.clone(), page_info));
                // 取得中に別ページへ移動していた場合（要求ページと不一致）は画面反映のみ破棄。
                if self.workspaces_page_info.page == page_info.page {
                    self.status = Status::Idle;
                    // 同一文脈（同じページ）への再検証: 選択位置は識別子（slug）で追従し、
                    // 見つからなければインデックスをクランプする（先頭へは戻さない）。
                    self.workspaces
                        .set_items_keep_selection_by(workspaces, |workspace| {
                            workspace.slug.clone()
                        });
                    self.workspaces_page_info = page_info;
                }
                Command::None
            }
            Msg::RepositoriesLoaded {
                workspace,
                sort,
                repos,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（裏で他ワークスペース/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.repositories_cache.insert(
                    (workspace.clone(), sort, page_info.page),
                    (repos.clone(), page_info),
                );
                // 取得中に別ワークスペース/ページ/ソートへ切り替えていた場合は画面反映のみ破棄。
                if self.selected_workspace.as_deref() == Some(workspace.as_str())
                    && self.repositories_sort == sort
                    && self.repositories_page_info.page == page_info.page
                {
                    self.status = Status::Idle;
                    // 同一文脈への再検証: 選択位置は識別子（full_name）で追従する。
                    self.repositories
                        .set_items_keep_selection_by(repos, |repo| repo.full_name.clone());
                    self.repositories_page_info = page_info;
                }
                Command::None
            }
            Msg::LoadFailed(error) => {
                self.status = Status::Error(error.to_string());
                Command::None
            }
            Msg::PullRequestsLoaded {
                repo,
                filter,
                sort,
                prs,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（他 repo/フィルタ/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.pull_requests_cache.insert(
                    (repo.clone(), filter, sort, page_info.page),
                    (prs.clone(), page_info),
                );
                if self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.pr_state_filter == filter
                    && self.pull_requests_sort == sort
                    && self.pull_requests_page_info.page == page_info.page
                {
                    self.status = Status::Idle;
                    // 同一文脈への再検証: 選択位置は識別子（PR id）で追従する。
                    self.pull_requests
                        .set_items_keep_selection_by(prs, |pr| pr.id);
                    self.pull_requests_page_info = page_info;
                }
                Command::None
            }
            Msg::PrDetailLoaded { id, pr } => {
                if self.current_pr_id() == Some(id) {
                    self.clear_loading();
                    let pr = *pr;
                    self.current_pr = Some(pr.clone());
                    self.update_pr_detail_cache(id, move |entry| entry.pr = pr);
                }
                Command::None
            }
            Msg::DiffStatLoaded { id, entries } => {
                if self.current_pr_id() == Some(id) {
                    self.diffstat.set_items(entries.clone());
                    self.update_pr_detail_cache(id, move |entry| entry.diffstat = entries);
                }
                Command::None
            }
            Msg::CommentsLoaded { id, comments } => {
                if self.current_pr_id() == Some(id) {
                    let comments: Vec<Comment> = comments
                        .into_iter()
                        .filter(|comment| !comment.deleted)
                        .collect();
                    self.comments = comments.clone();
                    self.update_pr_detail_cache(id, move |entry| entry.comments = comments);
                }
                Command::None
            }
            Msg::DiffLoaded { id, text } => {
                if self.current_pr_id() == Some(id) {
                    self.clear_loading();
                    self.diff = Some(DiffState {
                        parsed: parse_diff(&text),
                        scroll: 0,
                        viewport: 0,
                        title: format!("#{id}"),
                        rendered_lines: None,
                        file_index: 0,
                        focus: DiffFocus::Body,
                    });
                }
                Command::None
            }
            Msg::ReviewActionDone { id, message } => {
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(message);
                    // 承認/変更要求の状態は PR 一覧の ✔n/m バッジにも出るため、一覧キャッシュは
                    // 無効化する（詳細側は refresh_detail() の再取得結果が update_pr_detail_cache
                    // 経由で自動的に最新化される）。
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_detail();
                }
                Command::None
            }
            Msg::CommentPosted { id } => {
                self.comment_editor = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success("コメントを投稿しました".to_string());
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_comments();
                }
                Command::None
            }
            Msg::MergeDone { id } => {
                self.merge_modal = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(format!("PR #{id} をマージしました"));
                    // マージで PR の state が変わり一覧の掲載フィルタ（OPEN/MERGED/…）を
                    // またぐため、一覧キャッシュは無効化して次回表示を必ず再取得させる。
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_detail();
                }
                Command::None
            }
            Msg::ActionFailed(error) => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.submitting = false;
                }
                if let Some(modal) = self.merge_modal.as_mut() {
                    modal.submitting = false;
                }
                if let Some(modal) = self.confirm_modal.as_mut() {
                    modal.submitting = false;
                }
                self.status = Status::Error(error.to_string());
                Command::None
            }
            Msg::Tick => self.on_tick(),
            Msg::PipelinesLoaded { repo, pipelines } => {
                if self.repo_slug().as_deref() == Some(repo.as_str()) {
                    self.clear_loading();
                    self.pipelines.set_items_keep_selection(pipelines);
                }
                Command::None
            }
            Msg::PipelineLoaded { uuid, pipeline } => {
                if self.current_pipeline_uuid() == Some(uuid.as_str()) {
                    self.clear_loading();
                    self.current_pipeline = Some(*pipeline);
                }
                Command::None
            }
            Msg::PipelineStepsLoaded { uuid, steps } => {
                if self.current_pipeline_uuid() == Some(uuid.as_str()) {
                    self.pipeline_steps.set_items_keep_selection(steps);
                }
                Command::None
            }
            Msg::StepLogLoaded { step_uuid, text } => {
                if self.open_step_uuid.as_deref() == Some(step_uuid.as_str()) {
                    self.clear_loading();
                    let title = self.step_log_title(&step_uuid);
                    // 同じステップの再取得ではスクロール位置を維持する（擬似 tail）。
                    let prev_scroll = self
                        .step_log
                        .as_ref()
                        .filter(|view| view.step_uuid == step_uuid)
                        .map(|view| view.scroll);
                    let mut view = match text {
                        Some(text) => LogView::from_text(step_uuid, title, &text),
                        None => LogView::missing(step_uuid, title),
                    };
                    if let Some(scroll) = prev_scroll {
                        view.scroll = scroll;
                    }
                    self.step_log = Some(view);
                }
                Command::None
            }
            Msg::PipelineActionDone { action } => {
                self.confirm_modal = None;
                self.auto_refresh = true;
                match action {
                    PipelineAction::Stop => {
                        self.status = Status::Success("パイプラインを停止しました".to_string());
                        // Loading 表示を出さず（成功メッセージを残して）静かに再取得する。
                        self.refresh_pipeline_view_silent()
                    }
                    PipelineAction::Rerun => {
                        self.status = Status::Success("パイプラインを再実行しました".to_string());
                        // 新しい実行が一覧の先頭に現れるため一覧へ戻し、静かに再取得する。
                        self.screen = Screen::Pipelines;
                        self.refresh_pipelines_silent()
                    }
                }
            }
            Msg::BranchesLoaded {
                repo,
                branches,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（裏で他 repo/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.branches_cache.insert(
                    (repo.clone(), page_info.page),
                    (branches.clone(), page_info),
                );
                // 取得中に別 repo/ページへ切り替えていた場合は画面反映のみ破棄。
                if self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.branches_page_info.page == page_info.page
                {
                    self.clear_loading();
                    // 同一文脈への再検証: 選択位置は識別子（ブランチ名）で追従する。
                    self.branches
                        .set_items_keep_selection_by(branches, |branch| branch.name.clone());
                    self.branches_page_info = page_info;
                }
                Command::None
            }
            Msg::CommitsLoaded { revision, commits } => {
                if self.commits_revision == revision {
                    self.clear_loading();
                    self.commits.set_items(commits);
                }
                Command::None
            }
            Msg::CommitDetailLoaded { hash, commit } => {
                if self.current_commit_hash() == Some(hash.as_str()) {
                    self.clear_loading();
                    self.current_commit = Some(*commit);
                }
                Command::None
            }
            Msg::CommitDiffLoaded { spec, text } => {
                if self.current_commit_hash() == Some(spec.as_str()) {
                    self.clear_loading();
                    self.diff = Some(DiffState {
                        parsed: parse_diff(&text),
                        scroll: 0,
                        viewport: 0,
                        title: short_hash_str(&spec),
                        rendered_lines: None,
                        file_index: 0,
                        focus: DiffFocus::Body,
                    });
                }
                Command::None
            }
            Msg::SourceLoaded {
                reference,
                path,
                mut entries,
            } => {
                let matches = self
                    .source
                    .as_ref()
                    .is_some_and(|source| source.reference == reference && source.path == path);
                if matches {
                    self.clear_loading();
                    sort_src_entries(&mut entries);
                    if let Some(source) = self.source.as_mut() {
                        source.entries.set_items(entries);
                    }
                }
                Command::None
            }
            Msg::FileLoaded { path, text } => {
                if self.open_file_path.as_deref() == Some(path.as_str()) {
                    self.clear_loading();
                    let mimetype = self.open_file_mimetype.clone();
                    self.file_view = Some(LogView::from_file(
                        path.clone(),
                        path,
                        mimetype.as_deref(),
                        &text,
                    ));
                }
                Command::None
            }
        }
    }

    /// Loading 表示だけを解除する（Success/Error は上書きしない）。
    fn clear_loading(&mut self) {
        if matches!(self.status, Status::Loading(_)) {
            self.status = Status::Idle;
        }
    }

    /// 認証成功時: token を Keychain へ、email/表示名を config へ保存し、Workspaces へ遷移。
    fn on_auth_validated(&mut self, email: String, token: String, user: User) -> Command {
        self.onboarding.validating = false;
        self.onboarding.error = None;

        if let Err(error) = auth::save_token(&email, &token) {
            self.onboarding.error = Some(format!("token の保存に失敗しました: {error}"));
            return Command::None;
        }

        self.config.email = Some(email.clone());
        self.config.display_name = user.display_name.clone();
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない。ログに残しつつ続行する。
            tracing::warn!(%error, "config.toml の保存に失敗しました");
        }

        self.me = Me {
            account_id: user.account_id.clone(),
            uuid: user.uuid.clone(),
            display_name: user.display_name.clone(),
        };

        let client = match BitbucketClient::new(email, token) {
            Ok(client) => client,
            Err(error) => {
                self.onboarding.error = Some(error.to_string());
                return Command::None;
            }
        };
        self.client = Some(client);
        self.screen = Screen::Workspaces;
        self.load_workspaces_page(1)
    }

    /// キー入力の処理。グローバルキー（Ctrl+C / Ctrl+T / ジャンプパレット / ヘルプ / モーダル）
    /// を先に捌く。
    fn on_key(&mut self, key: KeyEvent) -> Command {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::Quit;
        }
        if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.cycle_theme();
        }

        // ジャンプパレット（`Ctrl+K`）: 開いている間は最優先で入力を奪う。
        if self.jump_palette.is_some() {
            return self.on_key_jump_palette(key);
        }
        // 開くトリガーは show_help と同格（他のモーダル/検索編集中/Onboarding では無効）。
        // Onboarding だけは対象外: `Ctrl+K` は emacs 風の「行末まで削除」で既に使用中で、
        // 認証前は保持済みデータも無くジャンプ先が無いため衝突を避ける。
        if key.code == KeyCode::Char('k')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.screen != Screen::Onboarding
            && !self.show_help
            && !self.search_editing
            && self.comment_editor.is_none()
            && self.merge_modal.is_none()
            && self.confirm_modal.is_none()
        {
            return self.open_jump_palette();
        }

        if self.show_help {
            // ヘルプ表示中は任意のキーで閉じる。
            self.show_help = false;
            return Command::None;
        }

        // モーダル/エディタは画面キーより優先。
        if self.comment_editor.is_some() {
            return self.on_key_comment_editor(key);
        }
        if self.merge_modal.is_some() {
            return self.on_key_merge_modal(key);
        }
        if self.confirm_modal.is_some() {
            return self.on_key_confirm_modal(key);
        }
        if self.page_jump.is_some() {
            return self.on_key_page_jump(key);
        }
        if self.search_editing {
            return self.on_key_search_editing(key);
        }

        match self.screen {
            Screen::Onboarding => self.on_key_onboarding(key),
            Screen::Workspaces => self.on_key_workspaces(key),
            Screen::Repositories => self.on_key_repositories(key),
            Screen::PullRequests => self.on_key_pull_requests(key),
            Screen::PullRequestDetail => self.on_key_pull_request_detail(key),
            Screen::Diff => self.on_key_diff(key),
            Screen::Pipelines => self.on_key_pipelines(key),
            Screen::PipelineDetail => self.on_key_pipeline_detail(key),
            Screen::StepLog => self.on_key_step_log(key),
            Screen::Branches => self.on_key_branches(key),
            Screen::Commits => self.on_key_commits(key),
            Screen::CommitDetail => self.on_key_commit_detail(key),
            Screen::Source => self.on_key_source(key),
            Screen::FileView => self.on_key_file_view(key),
        }
    }

    /// テーマを次へ巡回する（`Ctrl+T`）。`config.toml` へ永続化し、Diff の着色済み行
    /// キャッシュ（[`DiffState::rendered_lines`]）を無効化して次回描画で新テーマ色を
    /// 再構築させる（無効化しないと旧テーマの色のまま表示され続ける）。
    fn cycle_theme(&mut self) -> Command {
        self.theme_name = self.theme_name.next();
        self.theme = self.theme_name.theme();

        if let Some(diff) = self.diff.as_mut() {
            diff.rendered_lines = None;
        }

        self.config.theme = Some(self.theme_name.as_str().to_string());
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない（他の config 保存箇所と同じ方針）。
            tracing::warn!(%error, "テーマ設定の保存に失敗しました");
        }
        Command::None
    }

    fn on_key_onboarding(&mut self, key: KeyEvent) -> Command {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.onboarding.error = None;
                Command::None
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                self.onboarding.toggle_field();
                Command::None
            }
            KeyCode::Enter => self.submit_onboarding(),
            KeyCode::Backspace => {
                self.onboarding.backspace();
                Command::None
            }
            KeyCode::Delete => {
                self.onboarding.delete();
                Command::None
            }
            // カーソル移動（矢印 / Home / End、および emacs 風 Ctrl+A/E/B/F）。
            KeyCode::Left => {
                self.onboarding.move_left();
                Command::None
            }
            KeyCode::Right => {
                self.onboarding.move_right();
                Command::None
            }
            KeyCode::Home => {
                self.onboarding.move_home();
                Command::None
            }
            KeyCode::End => {
                self.onboarding.move_end();
                Command::None
            }
            // 行/語削除（Ctrl+U 先頭まで / Ctrl+K 末尾まで / Ctrl+W 直前の語）。
            KeyCode::Char('u') if ctrl => {
                self.onboarding.kill_to_start();
                Command::None
            }
            KeyCode::Char('k') if ctrl => {
                self.onboarding.kill_to_end();
                Command::None
            }
            KeyCode::Char('w') if ctrl => {
                self.onboarding.kill_word_before();
                Command::None
            }
            KeyCode::Char('a') if ctrl => {
                self.onboarding.move_home();
                Command::None
            }
            KeyCode::Char('e') if ctrl => {
                self.onboarding.move_end();
                Command::None
            }
            KeyCode::Char('b') if ctrl => {
                self.onboarding.move_left();
                Command::None
            }
            KeyCode::Char('f') if ctrl => {
                self.onboarding.move_right();
                Command::None
            }
            KeyCode::Char('d') if ctrl => {
                self.onboarding.delete();
                Command::None
            }
            KeyCode::Char('h') if ctrl => {
                self.onboarding.backspace();
                Command::None
            }
            // 通常文字の入力（Ctrl/Alt 修飾は上で処理済みなので除外）。
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.onboarding.insert_char(ch);
                Command::None
            }
            _ => Command::None,
        }
    }

    /// Enter 押下時の Onboarding 進行。email フィールドなら token へ移動、token なら検証開始。
    fn submit_onboarding(&mut self) -> Command {
        if self.onboarding.validating {
            return Command::None;
        }
        if self.onboarding.field.0 == Field::Email {
            self.onboarding.field.0 = Field::Token;
            return Command::None;
        }
        if !self.onboarding.is_submittable() {
            self.onboarding.error =
                Some("メールアドレスと API token の両方を入力してください".to_string());
            return Command::None;
        }
        self.onboarding.validating = true;
        self.onboarding.error = None;
        Command::ValidateAuth {
            email: self.onboarding.email.value().trim().to_string(),
            token: self.onboarding.token.value(),
        }
    }

    fn on_key_workspaces(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Char('/') => {
                self.search_editing = true;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.workspaces.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.workspaces.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.workspaces.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.workspaces.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Enter => self.enter_workspace(),
            _ => Command::None,
        }
    }

    /// ワークスペース決定時: 選択中のワークスペースへ入る。
    fn enter_workspace(&mut self) -> Command {
        let Some(workspace) = self.workspaces.selected() else {
            return Command::None;
        };
        let slug = workspace.slug.clone();
        self.jump_to_workspace(slug)
    }

    /// 指定ワークスペースへ入る（既定ワークスペースの保存・リポジトリ 1 ページ目の取得開始
    /// まで行う）。一覧での選択決定（[`App::enter_workspace`]）とジャンプパレットの双方から
    /// 呼ぶ共通経路。
    fn jump_to_workspace(&mut self, slug: String) -> Command {
        self.selected_workspace = Some(slug.clone());
        self.config.default_workspace = Some(slug);
        if let Err(error) = self.config.save() {
            tracing::warn!(%error, "既定ワークスペースの保存に失敗しました");
        }

        self.screen = Screen::Repositories;
        self.load_repositories_page(1)
    }

    /// リポジトリ一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::repositories_cache`]、キー = (workspace slug, ソート, ページ番号)）が
    /// あれば即座に一覧を表示しつつ、裏で `Command::LoadRepositories` を発行して最新化する
    /// （stale-while-revalidate）。キャッシュが無ければ一覧をクリアして Loading 表示を出す。
    /// [`App::jump_to_workspace`]（新規入場は 1 ページ目から）・`[`/`]`/`g`（ページ移動）・
    /// `S`（ソート変更、1 ページ目から）の共通経路。
    fn load_repositories_page(&mut self, page: u32) -> Command {
        let Some(workspace) = self.selected_workspace.clone() else {
            self.status = Status::Error("ワークスペースが未選択です".to_string());
            return Command::None;
        };
        let sort = self.repositories_sort;

        let cache_key = (workspace.clone(), sort, page);
        match self.repositories_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_repositories(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.repositories.set_items(Vec::new());
                self.repositories_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!(
                    "{workspace} のリポジトリを取得中…（{} ・{page} ページ目）",
                    sort.label()
                ));
            }
        }

        match &self.client {
            Some(client) => Command::LoadRepositories {
                client: client.clone(),
                workspace,
                sort,
                page,
            },
            None => {
                self.status = Status::Error("認証クライアントが未初期化です".to_string());
                Command::None
            }
        }
    }

    /// 取得結果を `repositories` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （ワークスペース変更・ページ変更・ソート変更）はここで選択を先頭にリセットする
    /// （キャッシュヒットによる即時表示（[`App::load_repositories_page`]）専用。バックグラウンド
    /// 再検証結果は `Msg::RepositoriesLoaded` 側で選択を保持したまま部分更新する）。
    ///
    /// 検索フィルタは（`SelectList::set_items` により）ここで必ずクリアされる。ページが変われば
    /// 表示される 40 件が入れ替わるため、前ページのフィルタを引き継がない設計判断。
    fn apply_repositories(&mut self, repos: Vec<Repository>, page_info: PageInfo) {
        self.repositories.set_items(repos);
        self.repositories_page_info = page_info;
    }

    fn on_key_repositories(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Char('/') => {
                self.search_editing = true;
                Command::None
            }
            KeyCode::Char('S') => self.cycle_repositories_sort(),
            KeyCode::Esc => {
                self.screen = Screen::Workspaces;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.repositories.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.repositories.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.repositories.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.repositories.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Enter => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.enter_repository(repo)
            }
            KeyCode::Char('p') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.open_pipelines()
            }
            KeyCode::Char('b') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.browse_return = Screen::Repositories;
                self.open_branches()
            }
            KeyCode::Char('s') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.browse_return = Screen::Repositories;
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            _ => Command::None,
        }
    }

    /// 選択リポジトリを確定する（`ws/repo` と既定ブランチを保持する）。
    fn select_repo(&mut self, repo: &Repository) {
        self.selected_repo = Some(repo.full_name.clone());
        self.repo_main_branch = repo.main_branch_name().map(str::to_string);
    }

    /// リポジトリを選択して PR 一覧へ入る。一覧での決定（Enter）とジャンプパレットの
    /// 双方から呼ぶ共通経路。
    fn enter_repository(&mut self, repo: Repository) -> Command {
        self.select_repo(&repo);
        self.open_pull_requests()
    }

    /// Source ルートに使う既定ブランチ名（未取得なら `main` にフォールバック）。
    fn default_source_ref(&self) -> String {
        self.repo_main_branch
            .clone()
            .unwrap_or_else(|| "main".to_string())
    }

    /// Repositories 一覧のサーバソートを次へ巡回し、1 ページ目から再取得する
    /// （Bitbucket ブラウザ版と同じ 4 種類。[`ListSort`]）。
    fn cycle_repositories_sort(&mut self) -> Command {
        self.repositories_sort = self.repositories_sort.next();
        self.load_repositories_page(1)
    }

    /// PR 一覧画面へ遷移し、OPEN の一覧の 1 ページ目取得を開始する。
    fn open_pull_requests(&mut self) -> Command {
        self.screen = Screen::PullRequests;
        self.pr_state_filter = PrStateFilter::Open;
        self.current_pr = None;
        self.load_pull_requests_page(1)
    }

    /// PR 一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::pull_requests_cache`]、キー = (repo slug, state フィルタ, ソート,
    /// ページ番号)）があれば即座に一覧を表示しつつ、裏で `Command::LoadPullRequests` を
    /// 発行して最新化する（stale-while-revalidate）。キャッシュが無ければ一覧をクリアして
    /// Loading 表示を出す。リポジトリへの新規入場・フィルタ切り替え・ソート変更（いずれも
    /// 1 ページ目から）・ページ移動（`[`/`]`/`g`）・手動リロード（`r`、現在ページを再取得）の
    /// 共通経路。
    fn load_pull_requests_page(&mut self, page: u32) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        let sort = self.pull_requests_sort;

        let cache_key = (repo.clone(), self.pr_state_filter, sort, page);
        match self.pull_requests_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_pull_requests(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.pull_requests.set_items(Vec::new());
                self.pull_requests_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!(
                    "PR 一覧を取得中…（{}・{}・{page} ページ目）",
                    self.pr_state_filter.label(),
                    sort.label()
                ));
            }
        }

        Command::LoadPullRequests {
            client,
            workspace,
            repo,
            filter: self.pr_state_filter,
            sort,
            page,
        }
    }

    /// 現在のフィルタ・現在ページで PR 一覧を再取得する（`r` キー）。
    fn reload_pull_requests(&mut self) -> Command {
        self.load_pull_requests_page(self.pull_requests_page_info.page)
    }

    /// 取得結果を `pull_requests` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （リポジトリ変更・フィルタ変更・ページ変更・ソート変更）はここで選択を先頭にリセット
    /// する（キャッシュヒットによる即時表示（[`App::load_pull_requests_page`]）専用。
    /// バックグラウンド再検証結果は `Msg::PullRequestsLoaded` 側で選択を保持したまま
    /// 部分更新する）。
    ///
    /// 検索フィルタは（`SelectList::set_items` により）ここで必ずクリアされる。ページが変われば
    /// 表示される 40 件が入れ替わるため、前ページのフィルタを引き継がない設計判断。
    fn apply_pull_requests(&mut self, prs: Vec<PullRequest>, page_info: PageInfo) {
        self.pull_requests.set_items(prs);
        self.pull_requests_page_info = page_info;
    }

    fn on_key_pull_requests(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Char('/') => {
                self.search_editing = true;
                Command::None
            }
            KeyCode::Char('S') => self.cycle_pull_requests_sort(),
            KeyCode::Esc => {
                self.screen = Screen::Repositories;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.pull_requests.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pull_requests.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.pull_requests.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pull_requests.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Char('o') => self.set_pr_filter(PrStateFilter::Open),
            KeyCode::Char('m') => self.set_pr_filter(PrStateFilter::Merged),
            KeyCode::Char('d') => self.set_pr_filter(PrStateFilter::Declined),
            KeyCode::Char('a') => self.set_pr_filter(PrStateFilter::All),
            KeyCode::Char('r') => self.reload_pull_requests(),
            KeyCode::Char('P') => self.open_pipelines(),
            KeyCode::Char('b') => {
                self.browse_return = Screen::PullRequests;
                self.open_branches()
            }
            KeyCode::Char('s') => {
                self.browse_return = Screen::PullRequests;
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            KeyCode::Enter => self.open_pr_detail(),
            _ => Command::None,
        }
    }

    /// 状態フィルタを切り替え、1 ページ目から読み込み直す（新しいフィルタ文脈のため）。
    fn set_pr_filter(&mut self, filter: PrStateFilter) -> Command {
        self.pr_state_filter = filter;
        self.load_pull_requests_page(1)
    }

    /// PullRequests 一覧のサーバソートを次へ巡回し、1 ページ目から再取得する。
    fn cycle_pull_requests_sort(&mut self) -> Command {
        self.pull_requests_sort = self.pull_requests_sort.next();
        self.load_pull_requests_page(1)
    }

    // ---- サーバサイド・ページネーション（Workspaces/Repositories/PullRequests 共通） ----

    /// ワークスペース一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::workspaces_cache`]、キー = ページ番号）があれば即座に一覧を表示し
    /// つつ、裏で `Command::LoadWorkspaces` を発行して最新化する（stale-while-revalidate）。
    /// キャッシュが無ければ一覧をクリアして Loading 表示を出す。起動時（1 ページ目）と
    /// `[`/`]`/`g`（ページ移動）の双方から呼ぶ共通経路。
    fn load_workspaces_page(&mut self, page: u32) -> Command {
        match self.workspaces_cache.get(&page).cloned() {
            Some((cached, info)) => {
                self.apply_workspaces(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.workspaces.set_items(Vec::new());
                self.workspaces_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status =
                    Status::Loading(format!("ワークスペースを取得中…（{page} ページ目）"));
            }
        }

        match &self.client {
            Some(client) => Command::LoadWorkspaces {
                client: client.clone(),
                page,
            },
            None => {
                self.status = Status::Error("認証クライアントが未初期化です".to_string());
                Command::None
            }
        }
    }

    /// 取得結果を `workspaces` へ反映する（ページ状態の更新を含む）。新規取得
    /// （`Msg::WorkspacesLoaded`）とキャッシュからの即時表示（[`App::load_workspaces_page`]）の
    /// 両方から呼ぶ共通経路。検索フィルタは（`SelectList::set_items` により）ここで必ずクリア
    /// される（ページが変われば表示される 40 件が入れ替わるため）。
    fn apply_workspaces(&mut self, workspaces: Vec<Workspace>, page_info: PageInfo) {
        self.workspaces.set_items(workspaces);
        self.workspaces_page_info = page_info;
    }

    /// 現在の画面がページング対象（Workspaces/Repositories/PullRequests/Branches）なら、その
    /// ページ状態を返す。それ以外の画面では `None`（ページ移動キーは何もしない）。
    fn page_info(&self) -> Option<PageInfo> {
        match self.screen {
            Screen::Workspaces => Some(self.workspaces_page_info),
            Screen::Repositories => Some(self.repositories_page_info),
            Screen::PullRequests => Some(self.pull_requests_page_info),
            Screen::Branches => Some(self.branches_page_info),
            _ => None,
        }
    }

    /// 指定ページ番号のデータを、現在の画面に応じて読み込む。
    fn load_page(&mut self, page: u32) -> Command {
        match self.screen {
            Screen::Workspaces => self.load_workspaces_page(page),
            Screen::Repositories => self.load_repositories_page(page),
            Screen::PullRequests => self.load_pull_requests_page(page),
            Screen::Branches => self.load_branches_page(page),
            _ => Command::None,
        }
    }

    /// 前ページへ（`[`）。1 ページ目では何もしない（パニックせずクランプ）。
    fn prev_page(&mut self) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        if info.page <= 1 {
            return Command::None;
        }
        self.load_page(info.page - 1)
    }

    /// 次ページへ（`]`）。`has_next`（次ページ無し）が `false` なら何もしない。
    fn next_page(&mut self) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        if !info.has_next {
            return Command::None;
        }
        self.load_page(info.page + 1)
    }

    /// 指定ページ番号へジャンプする（ページ番号ジャンプの入力確定時）。既知の総ページ数が
    /// あればその範囲へクランプし、無ければ 1 未満のみクランプする（範囲外はサーバ応答
    /// （`has_next=false` 等）に委ねる。パニックしない）。
    fn goto_page(&mut self, page: u32) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        let target = match info.total_pages {
            Some(total) if total > 0 => page.clamp(1, total),
            _ => page.max(1),
        };
        self.load_page(target)
    }

    /// ページ番号ジャンプの入力プロンプトを開く（`g`）。ページング対象外の画面では何もしない。
    fn open_page_jump(&mut self) -> Command {
        if self.page_info().is_some() {
            self.page_jump = Some(PageJumpModal::default());
        }
        Command::None
    }

    /// ページ番号ジャンプの入力プロンプトのキー処理。数字のみ受け付け、`Enter` で確定
    /// （[`App::goto_page`]）、`Esc`/`Backspace` で取消/1 文字削除する。
    fn on_key_page_jump(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.page_jump = None;
                Command::None
            }
            KeyCode::Enter => {
                let Some(modal) = self.page_jump.take() else {
                    return Command::None;
                };
                match modal.input.parse::<u32>() {
                    Ok(page) if page >= 1 => self.goto_page(page),
                    _ => {
                        self.status =
                            Status::Error("1 以上のページ番号を入力してください".to_string());
                        Command::None
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(modal) = self.page_jump.as_mut() {
                    modal.input.pop();
                }
                Command::None
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                // 極端な桁数の入力を防ぐ安全上限（6 桁あれば実用上十分）。
                if let Some(modal) = self.page_jump.as_mut()
                    && modal.input.len() < 6
                {
                    modal.input.push(ch);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// PR 詳細本文の表示行数（ヘッダ 4 行 + 承認/変更要求パネル行 + 本文行数。
    /// 本文が無い場合はプレースホルダの 1 行）。
    ///
    /// `ui::render_pr_meta_body` が積む行と対応する（折り返し前の行数。`Wrap` による折り返しは
    /// 数えないため、狭い端末では厳密な末尾より手前でクランプされ得るが、無制限スクロールという
    /// バグを防ぐには十分）。
    fn detail_body_line_count(&self) -> usize {
        let Some(pr) = self.current_pr.as_ref() else {
            return 0;
        };
        let body_lines = pr.body().map_or(1, |body| body.lines().count().max(1));
        PR_DETAIL_HEADER_LINES + participant_panel_line_count(pr) + body_lines
    }

    /// `detail_scroll` の上限（本文が直近描画のビューポートに収まる位置）。
    fn detail_max_scroll(&self) -> u16 {
        let total = self.detail_body_line_count();
        let viewport = self.detail_viewport.max(1);
        total.saturating_sub(viewport).min(u16::MAX as usize) as u16
    }

    /// `detail_scroll` をビューポートに合わせてクランプする（`DiffState::max_scroll` /
    /// `LogView::clamp_scroll` と同じパターン）。`ui` が毎フレーム再クランプに使うため `pub`。
    pub fn clamp_detail_scroll(&mut self) {
        let max = self.detail_max_scroll();
        if self.detail_scroll > max {
            self.detail_scroll = max;
        }
    }

    /// 選択中の PR の詳細画面へ遷移し、詳細/diffstat/コメントの取得を開始する。
    fn open_pr_detail(&mut self) -> Command {
        let Some(pr) = self.pull_requests.selected().cloned() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.open_pr_detail_with(client, workspace, repo, pr)
    }

    /// 他のワークスペース/リポジトリの PR へ直接ジャンプする（ジャンプパレット用）。
    /// `selected_workspace`/`selected_repo` を明示的に切り替えてから通常の詳細取得経路
    /// （[`App::open_pr_detail_with`]）に合流する。
    fn jump_to_pr(
        &mut self,
        workspace: String,
        repo_full_name: String,
        pr: PullRequest,
    ) -> Command {
        self.selected_workspace = Some(workspace);
        self.selected_repo = Some(repo_full_name);
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.open_pr_detail_with(client, workspace, repo, pr)
    }

    /// PR 詳細画面へ遷移し、詳細/diffstat/コメントの取得を発行する共通経路。
    /// 一覧での決定（[`App::open_pr_detail`]）とジャンプパレット（[`App::jump_to_pr`]）の
    /// 双方から呼ぶ。
    ///
    /// キャッシュ（[`App::pr_detail_cache`]、キー = (repo full_name, PR id)）があれば
    /// 詳細/diffstat/コメントを即座に表示しつつ、裏で 3 つの `Command::Load*` を発行して
    /// 最新化する（stale-while-revalidate）。承認状態やコメント数はレビュー中に変わり得る
    /// ため、キャッシュ命中時も必ず裏で再取得する。
    fn open_pr_detail_with(
        &mut self,
        client: BitbucketClient,
        workspace: String,
        repo: String,
        pr: PullRequest,
    ) -> Command {
        let id = pr.id;
        self.current_pr = Some(pr.clone());
        self.diff = None;
        self.detail_scroll = 0;
        self.screen = Screen::PullRequestDetail;
        self.record_recent_pr(pr);

        let cache_key = self.selected_repo.clone().map(|full_name| (full_name, id));
        match cache_key.and_then(|key| self.pr_detail_cache.get(&key).cloned()) {
            Some(cached) => {
                self.current_pr = Some(cached.pr);
                self.diffstat.set_items(cached.diffstat);
                self.comments = cached.comments;
                self.status = Status::Idle;
            }
            None => {
                self.diffstat.set_items(Vec::new());
                self.comments = Vec::new();
                self.status = Status::Loading(format!("PR #{id} を取得中…"));
            }
        }

        Command::Batch(vec![
            Command::LoadPrDetail {
                client: client.clone(),
                workspace: workspace.clone(),
                repo: repo.clone(),
                id,
            },
            Command::LoadDiffStat {
                client: client.clone(),
                workspace: workspace.clone(),
                repo: repo.clone(),
                id,
            },
            Command::LoadComments {
                client,
                workspace,
                repo,
                id,
            },
        ])
    }

    /// 「直近開いた PR」リストへ記録する（先頭に追加・同一 PR の重複は除去・上限 20 件）。
    /// ジャンプパレットの候補に使う。`selected_workspace`/`selected_repo` が未確定なら
    /// 何もしない（レビュー系操作ができない状態と同じ扱い）。
    fn record_recent_pr(&mut self, pr: PullRequest) {
        let Some(workspace) = self.selected_workspace.clone() else {
            return;
        };
        let Some(repo_full_name) = self.selected_repo.clone() else {
            return;
        };
        let id = pr.id;
        self.recent_prs
            .retain(|recent| !(recent.repo_full_name == repo_full_name && recent.pr.id == id));
        self.recent_prs.insert(
            0,
            RecentPr {
                workspace,
                repo_full_name,
                pr,
            },
        );
        const MAX_RECENT_PRS: usize = 20;
        self.recent_prs.truncate(MAX_RECENT_PRS);
    }

    fn on_key_pull_request_detail(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::PullRequests;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.diffstat.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.diffstat.select_prev();
                Command::None
            }
            KeyCode::PageDown => {
                self.detail_scroll = self.detail_scroll.saturating_add(5);
                self.clamp_detail_scroll();
                Command::None
            }
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
                Command::None
            }
            KeyCode::Char('J') => {
                self.detail_scroll = self.detail_scroll.saturating_add(10);
                self.clamp_detail_scroll();
                Command::None
            }
            KeyCode::Char('K') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(10);
                Command::None
            }
            KeyCode::Char('d') => self.open_diff(),
            KeyCode::Char('c') => {
                self.comment_editor = Some(CommentEditor::default());
                Command::None
            }
            KeyCode::Char('a') => self.toggle_approve(),
            KeyCode::Char('x') => self.toggle_request_changes(),
            KeyCode::Char('M') => self.open_merge_modal(),
            KeyCode::Char('o') => self.open_pr_in_browser(),
            _ => Command::None,
        }
    }

    /// 現在の PR をデフォルトブラウザで開く（macOS の `open` コマンドを子プロセスで起動）。
    ///
    /// TUI を抜けず、子プロセスの stdout/stderr は端末を汚さないよう `Stdio::null()` へ
    /// 捨てる。起動の成否のみを `Status` に反映する（終了を待たない = ブロックしない）。
    fn open_pr_in_browser(&mut self) -> Command {
        let Some(pr) = self.current_pr.as_ref() else {
            self.status = Status::Error("PR が選択されていません".to_string());
            return Command::None;
        };
        let Some(url) = pr.html_url() else {
            self.status = Status::Error("この PR のブラウザ URL が不明です".to_string());
            return Command::None;
        };
        let result = std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match result {
            Ok(_) => Status::Success("ブラウザで開きました".to_string()),
            Err(err) => Status::Error(format!("ブラウザを開けませんでした: {err}")),
        };
        Command::None
    }

    /// Diff 画面へ遷移する。未取得なら取得を開始し、取得済みなら再利用する。
    fn open_diff(&mut self) -> Command {
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        self.diff_return = Screen::PullRequestDetail;
        self.screen = Screen::Diff;
        if self.diff.is_some() {
            self.clear_loading();
            return Command::None;
        }
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("PR #{id} の diff を取得中…"));
        Command::LoadDiff {
            client,
            workspace,
            repo,
            id,
        }
    }

    /// approve をトグルする（現在の自分の承認状態から POST/DELETE を決める）。
    fn toggle_approve(&mut self) -> Command {
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let approve = !self.i_approved();
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(
            if approve {
                "承認を送信中…"
            } else {
                "承認取り消しを送信中…"
            }
            .to_string(),
        );
        Command::Approve {
            client,
            workspace,
            repo,
            id,
            approve,
        }
    }

    /// request-changes をトグルする。
    fn toggle_request_changes(&mut self) -> Command {
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let request = !self.i_requested_changes();
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(
            if request {
                "変更要求を送信中…"
            } else {
                "変更要求の取り消しを送信中…"
            }
            .to_string(),
        );
        Command::RequestChanges {
            client,
            workspace,
            repo,
            id,
            request,
        }
    }

    /// merge 確認モーダルを開く（OPEN の PR のみ）。
    fn open_merge_modal(&mut self) -> Command {
        let Some(pr) = self.current_pr.as_ref() else {
            return Command::None;
        };
        if !pr.is_open() {
            self.status = Status::Error("OPEN 状態の PR のみマージできます".to_string());
            return Command::None;
        }
        let close_source_branch = pr.close_source_branch.unwrap_or(false);
        self.merge_modal = Some(MergeModal::new(close_source_branch));
        Command::None
    }

    fn on_key_merge_modal(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.merge_modal = None;
                Command::None
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                if let Some(modal) = self.merge_modal.as_mut()
                    && !modal.submitting
                {
                    modal.cycle_strategy();
                }
                Command::None
            }
            KeyCode::Char(' ') => {
                if let Some(modal) = self.merge_modal.as_mut()
                    && !modal.submitting
                {
                    modal.close_source_branch = !modal.close_source_branch;
                }
                Command::None
            }
            KeyCode::Enter => self.confirm_merge(),
            _ => Command::None,
        }
    }

    /// モーダルの選択内容で merge を実行する。
    fn confirm_merge(&mut self) -> Command {
        let Some(modal) = self.merge_modal.as_ref() else {
            return Command::None;
        };
        if modal.submitting {
            return Command::None;
        }
        let params = MergeParams {
            merge_strategy: modal.strategy(),
            message: None,
            close_source_branch: modal.close_source_branch,
        };
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        if let Some(modal) = self.merge_modal.as_mut() {
            modal.submitting = true;
        }
        self.status = Status::Loading(format!("PR #{id} をマージ中…"));
        Command::Merge {
            client,
            workspace,
            repo,
            id,
            params,
        }
    }

    fn on_key_comment_editor(&mut self, key: KeyEvent) -> Command {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => {
                self.comment_editor = None;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Char('s') if ctrl => self.submit_comment(),
            KeyCode::Enter => {
                if let Some(editor) = self.comment_editor.as_mut()
                    && !editor.submitting
                {
                    editor.text.push('\n');
                }
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(editor) = self.comment_editor.as_mut()
                    && !editor.submitting
                {
                    editor.text.pop();
                }
                Command::None
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                if let Some(editor) = self.comment_editor.as_mut()
                    && !editor.submitting
                {
                    editor.text.push(ch);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// コメントエディタの内容を投稿する。
    fn submit_comment(&mut self) -> Command {
        let Some(editor) = self.comment_editor.as_ref() else {
            return Command::None;
        };
        if editor.submitting || !editor.is_submittable() {
            return Command::None;
        }
        let raw = editor.text.trim_end().to_string();
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        if let Some(editor) = self.comment_editor.as_mut() {
            editor.submitting = true;
        }
        self.status = Status::Loading("コメントを送信中…".to_string());
        Command::CreateComment {
            client,
            workspace,
            repo,
            id,
            raw,
        }
    }

    fn on_key_diff(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => return Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                return Command::None;
            }
            KeyCode::Esc => {
                self.screen = self.diff_return;
                return Command::None;
            }
            KeyCode::Tab => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.toggle_focus();
                }
                return Command::None;
            }
            _ => {}
        }

        let Some(diff) = self.diff.as_mut() else {
            return Command::None;
        };

        // ファイル一覧サイドバーにフォーカス中は ↑↓/jk がファイル選択（本文が追従）。
        // `n`/`N` は境界ジャンプとして本文フォーカス時と同じ挙動（サイドバー選択も同期）。
        if diff.focus == DiffFocus::Files {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => diff.select_file_next(),
                KeyCode::Up | KeyCode::Char('k') => diff.select_file_prev(),
                KeyCode::Char('n') => diff.next_file(),
                KeyCode::Char('N') => diff.prev_file(),
                _ => {}
            }
            return Command::None;
        }

        let page = diff.viewport.max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => diff.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k') => diff.scroll_up(1),
            KeyCode::Char('J') => diff.scroll_down(10),
            KeyCode::Char('K') => diff.scroll_up(10),
            KeyCode::PageDown | KeyCode::Char('f') => diff.scroll_down(page),
            KeyCode::PageUp | KeyCode::Char('b') => diff.scroll_up(page),
            KeyCode::Char('g') | KeyCode::Home => diff.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => diff.scroll_to_bottom(),
            KeyCode::Char('n') => diff.next_file(),
            KeyCode::Char('N') => diff.prev_file(),
            _ => {}
        }
        Command::None
    }

    // ---- パイプライン監視（M2） ----

    /// Pipelines 一覧画面へ遷移し、一覧取得を開始する。
    fn open_pipelines(&mut self) -> Command {
        self.screen = Screen::Pipelines;
        self.pipelines.set_items(Vec::new());
        self.current_pipeline = None;
        self.reload_pipelines()
    }

    /// パイプライン一覧を取得する（Loading 表示あり・手動 `r` / 再実行後に使う）。
    fn reload_pipelines(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading("パイプライン一覧を取得中…".to_string());
        Command::LoadPipelines {
            client,
            workspace,
            repo,
        }
    }

    /// パイプライン一覧を静かに再取得する（自動ポーリング用・Loading 表示なし）。
    fn refresh_pipelines_silent(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            return Command::None;
        };
        Command::LoadPipelines {
            client,
            workspace,
            repo,
        }
    }

    fn on_key_pipelines(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::Repositories;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.pipelines.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pipelines.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.pipelines.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pipelines.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('r') => self.reload_pipelines(),
            KeyCode::Char('a') => self.toggle_auto_refresh(),
            KeyCode::Char('S') => {
                let pipeline = self.pipelines.selected().cloned();
                self.open_stop_confirm(pipeline)
            }
            KeyCode::Char('R') => {
                let pipeline = self.pipelines.selected().cloned();
                self.open_rerun_confirm(pipeline)
            }
            KeyCode::Enter => self.open_pipeline_detail(),
            _ => Command::None,
        }
    }

    /// 選択中パイプラインの詳細画面へ遷移し、詳細とステップ一覧の取得を開始する。
    fn open_pipeline_detail(&mut self) -> Command {
        let Some(pipeline) = self.pipelines.selected().cloned() else {
            return Command::None;
        };
        let label = pipeline.build_label();
        let uuid = pipeline.uuid.clone();
        self.current_pipeline = Some(pipeline);
        self.pipeline_steps.set_items(Vec::new());
        self.step_log = None;
        self.open_step_uuid = None;
        self.screen = Screen::PipelineDetail;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("パイプライン {label} を取得中…"));
        self.pipeline_detail_commands(client, workspace, repo, uuid)
    }

    /// パイプライン詳細を再取得する（Loading 表示あり・手動 `r` / 停止後に使う）。
    fn reload_pipeline_detail(&mut self) -> Command {
        let Some(uuid) = self.current_pipeline_uuid().map(str::to_string) else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading("パイプラインを再取得中…".to_string());
        self.pipeline_detail_commands(client, workspace, repo, uuid)
    }

    /// パイプライン詳細を静かに再取得する（自動ポーリング用・Loading 表示なし）。
    fn refresh_pipeline_detail_silent(&mut self) -> Command {
        let Some(uuid) = self.current_pipeline_uuid().map(str::to_string) else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            return Command::None;
        };
        self.pipeline_detail_commands(client, workspace, repo, uuid)
    }

    /// 詳細＋ステップ一覧を一括取得する [`Command::Batch`] を組み立てる。
    fn pipeline_detail_commands(
        &self,
        client: BitbucketClient,
        workspace: String,
        repo: String,
        uuid: String,
    ) -> Command {
        Command::Batch(vec![
            Command::LoadPipeline {
                client: client.clone(),
                workspace: workspace.clone(),
                repo: repo.clone(),
                uuid: uuid.clone(),
            },
            Command::LoadPipelineSteps {
                client,
                workspace,
                repo,
                uuid,
            },
        ])
    }

    fn on_key_pipeline_detail(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::Pipelines;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.pipeline_steps.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pipeline_steps.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.pipeline_steps.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pipeline_steps.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('r') => self.reload_pipeline_detail(),
            KeyCode::Char('a') => self.toggle_auto_refresh(),
            KeyCode::Char('S') => {
                let pipeline = self.current_pipeline.clone();
                self.open_stop_confirm(pipeline)
            }
            KeyCode::Char('R') => {
                let pipeline = self.current_pipeline.clone();
                self.open_rerun_confirm(pipeline)
            }
            KeyCode::Enter => self.open_step_log(),
            _ => Command::None,
        }
    }

    /// 選択中ステップのログ画面へ遷移し、ログ取得を開始する。
    fn open_step_log(&mut self) -> Command {
        let Some(step) = self.pipeline_steps.selected().cloned() else {
            return Command::None;
        };
        let Some(pipeline_uuid) = self.current_pipeline_uuid().map(str::to_string) else {
            return Command::None;
        };
        let step_uuid = step.uuid.clone();
        self.open_step_uuid = Some(step_uuid.clone());
        self.step_log = None;
        self.screen = Screen::StepLog;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading("ログを取得中…".to_string());
        Command::LoadStepLog {
            client,
            workspace,
            repo,
            pipeline_uuid,
            step_uuid,
        }
    }

    /// 開いているステップのログを再取得する（進行中の擬似 tail・スクロール位置は維持）。
    fn reload_step_log(&mut self) -> Command {
        let Some(step_uuid) = self.open_step_uuid.clone() else {
            return Command::None;
        };
        let Some(pipeline_uuid) = self.current_pipeline_uuid().map(str::to_string) else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading("ログを再取得中…".to_string());
        Command::LoadStepLog {
            client,
            workspace,
            repo,
            pipeline_uuid,
            step_uuid,
        }
    }

    fn on_key_step_log(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => return Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                return Command::None;
            }
            KeyCode::Esc => {
                self.screen = Screen::PipelineDetail;
                self.status = Status::Idle;
                return Command::None;
            }
            KeyCode::Char('r') => return self.reload_step_log(),
            _ => {}
        }

        let Some(log) = self.step_log.as_mut() else {
            return Command::None;
        };
        let page = log.viewport.max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => log.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k') => log.scroll_up(1),
            KeyCode::Char('J') => log.scroll_down(10),
            KeyCode::Char('K') => log.scroll_up(10),
            KeyCode::PageDown | KeyCode::Char('f') => log.scroll_down(page),
            KeyCode::PageUp | KeyCode::Char('b') => log.scroll_up(page),
            KeyCode::Char('g') | KeyCode::Home => log.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => log.scroll_to_bottom(),
            _ => {}
        }
        Command::None
    }

    fn on_key_confirm_modal(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.confirm_modal = None;
                Command::None
            }
            KeyCode::Enter => self.confirm_pipeline_action(),
            _ => Command::None,
        }
    }

    /// stop 確認モーダルを開く（進行中のパイプラインのみ）。
    fn open_stop_confirm(&mut self, pipeline: Option<Pipeline>) -> Command {
        let Some(pipeline) = pipeline else {
            return Command::None;
        };
        if !pipeline.is_active() {
            self.status = Status::Error("進行中のパイプラインのみ停止できます".to_string());
            return Command::None;
        }
        self.confirm_modal = Some(ConfirmModal::new(PipelineAction::Stop, &pipeline));
        Command::None
    }

    /// re-run 確認モーダルを開く。
    fn open_rerun_confirm(&mut self, pipeline: Option<Pipeline>) -> Command {
        let Some(pipeline) = pipeline else {
            return Command::None;
        };
        self.confirm_modal = Some(ConfirmModal::new(PipelineAction::Rerun, &pipeline));
        Command::None
    }

    /// 確認モーダルの内容で stop / re-run を実行する。
    fn confirm_pipeline_action(&mut self) -> Command {
        let Some(modal) = self.confirm_modal.as_ref() else {
            return Command::None;
        };
        if modal.submitting {
            return Command::None;
        }
        let action = modal.action;
        let uuid = modal.pipeline_uuid.clone();
        let target = modal.target.clone();
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        match action {
            PipelineAction::Stop => {
                if let Some(modal) = self.confirm_modal.as_mut() {
                    modal.submitting = true;
                }
                self.status = Status::Loading("パイプラインを停止中…".to_string());
                Command::StopPipeline {
                    client,
                    workspace,
                    repo,
                    uuid,
                }
            }
            PipelineAction::Rerun => {
                let Some(target) = target else {
                    self.confirm_modal = None;
                    self.status =
                        Status::Error("再実行に必要な target 情報がありません".to_string());
                    return Command::None;
                };
                if let Some(modal) = self.confirm_modal.as_mut() {
                    modal.submitting = true;
                }
                self.status = Status::Loading("パイプラインを再実行中…".to_string());
                Command::TriggerPipeline {
                    client,
                    workspace,
                    repo,
                    target: Box::new(target),
                }
            }
        }
    }

    /// 自動リフレッシュを切り替える。
    fn toggle_auto_refresh(&mut self) -> Command {
        self.auto_refresh = !self.auto_refresh;
        self.status = Status::Success(format!(
            "自動更新: {}",
            if self.auto_refresh { "ON" } else { "OFF" }
        ));
        Command::None
    }

    /// 自動ポーリングの tick。進行中のパイプラインがある間だけリフレッシュを発行する。
    fn on_tick(&mut self) -> Command {
        if !self.auto_refresh {
            return Command::None;
        }
        match self.screen {
            Screen::Pipelines => {
                if self.pipelines.items.iter().any(Pipeline::is_active) {
                    self.refresh_pipelines_silent()
                } else {
                    Command::None
                }
            }
            Screen::PipelineDetail => {
                let pipeline_active = self
                    .current_pipeline
                    .as_ref()
                    .is_some_and(Pipeline::is_active);
                let steps_active = self
                    .pipeline_steps
                    .items
                    .iter()
                    .any(PipelineStep::is_active);
                if pipeline_active || steps_active {
                    self.refresh_pipeline_detail_silent()
                } else {
                    Command::None
                }
            }
            _ => Command::None,
        }
    }

    /// 現在の画面に応じてパイプライン状態を静かに再取得する（stop 実行後の反映用）。
    ///
    /// Loading 表示を出さないため直前の成功メッセージが残る。停止したパイプラインは進行中の
    /// うちは自動ポーリングでも更新され続ける。
    fn refresh_pipeline_view_silent(&mut self) -> Command {
        match self.screen {
            Screen::Pipelines => self.refresh_pipelines_silent(),
            Screen::PipelineDetail => self.refresh_pipeline_detail_silent(),
            _ => Command::None,
        }
    }

    /// 現在のパイプラインの uuid。
    fn current_pipeline_uuid(&self) -> Option<&str> {
        self.current_pipeline
            .as_ref()
            .map(|pipeline| pipeline.uuid.as_str())
    }

    /// StepLog の見出し（`#123 / ステップ名`）を組み立てる。
    fn step_log_title(&self, step_uuid: &str) -> String {
        let build = self
            .current_pipeline
            .as_ref()
            .map(Pipeline::build_label)
            .unwrap_or_default();
        let name = self
            .pipeline_steps
            .items
            .iter()
            .find(|step| step.uuid == step_uuid)
            .map(PipelineStep::name_str)
            .unwrap_or("ステップ");
        if build.is_empty() {
            name.to_string()
        } else {
            format!("{build} / {name}")
        }
    }

    // ---- リポジトリブラウズ（M3） ----

    /// Branches 一覧画面へ遷移し、1 ページ目の取得を開始する。
    fn open_branches(&mut self) -> Command {
        self.screen = Screen::Branches;
        self.load_branches_page(1)
    }

    /// ブランチ一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::branches_cache`]、キー = (repo slug, ページ番号)）があれば即座に
    /// 一覧を表示しつつ、裏で `Command::LoadBranches` を発行して最新化する
    /// （stale-while-revalidate）。キャッシュが無ければ一覧をクリアして Loading 表示を出す。
    /// [`App::open_branches`]（新規入場は 1 ページ目から）・`[`/`]`/`g`（ページ移動）・
    /// `r`（手動リロード、現在ページを再取得）の共通経路。
    fn load_branches_page(&mut self, page: u32) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };

        let cache_key = (repo.clone(), page);
        match self.branches_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_branches(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.branches.set_items(Vec::new());
                self.branches_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!("ブランチ一覧を取得中…（{page} ページ目）"));
            }
        }

        Command::LoadBranches {
            client,
            workspace,
            repo,
            page,
        }
    }

    /// 現在ページでブランチ一覧を再取得する（`r` キー）。
    fn reload_branches(&mut self) -> Command {
        self.load_branches_page(self.branches_page_info.page)
    }

    /// 取得結果を `branches` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （リポジトリ変更・ページ変更）はここで選択を先頭にリセットする（キャッシュヒットに
    /// よる即時表示（[`App::load_branches_page`]）専用。バックグラウンド再検証結果は
    /// `Msg::BranchesLoaded` 側で選択を保持したまま部分更新する）。
    fn apply_branches(&mut self, branches: Vec<Branch>, page_info: PageInfo) {
        self.branches.set_items(branches);
        self.branches_page_info = page_info;
    }

    fn on_key_branches(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = self.browse_return;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.branches.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.branches.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.branches.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.branches.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Char('r') => self.reload_branches(),
            KeyCode::Char('s') => match self.selected_branch_name() {
                Some(name) => self.open_source_root(name),
                None => Command::None,
            },
            KeyCode::Enter => match self.selected_branch_name() {
                Some(name) => self.open_commits(name),
                None => Command::None,
            },
            _ => Command::None,
        }
    }

    /// 選択中ブランチの名前（無ければ `None`）。
    fn selected_branch_name(&self) -> Option<String> {
        self.branches
            .selected()
            .and_then(|branch| branch.name.clone())
    }

    /// 指定 revision の Commits 画面へ遷移し、履歴取得を開始する。
    fn open_commits(&mut self, revision: String) -> Command {
        self.screen = Screen::Commits;
        self.commits_revision = Some(revision);
        self.commits.set_items(Vec::new());
        self.current_commit = None;
        self.reload_commits()
    }

    /// 現在の revision でコミット履歴を再取得する。
    fn reload_commits(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        let revision = self.commits_revision.clone();
        self.status = Status::Loading(format!(
            "コミット履歴を取得中…（{}）",
            revision.as_deref().unwrap_or("既定ブランチ")
        ));
        Command::LoadCommits {
            client,
            workspace,
            repo,
            revision,
        }
    }

    fn on_key_commits(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::Branches;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.commits.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.commits.select_prev();
                Command::None
            }
            KeyCode::Char('J') => {
                self.commits.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.commits.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('r') => self.reload_commits(),
            KeyCode::Enter => self.open_commit_detail(),
            _ => Command::None,
        }
    }

    /// 選択中コミットの詳細画面へ遷移し、詳細取得を開始する。
    fn open_commit_detail(&mut self) -> Command {
        let Some(commit) = self.commits.selected().cloned() else {
            return Command::None;
        };
        let Some(hash) = commit.hash.clone() else {
            self.status = Status::Error("コミット hash がありません".to_string());
            return Command::None;
        };
        self.current_commit = Some(commit);
        self.commit_scroll = 0;
        self.diff = None;
        self.screen = Screen::CommitDetail;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("コミット {} を取得中…", short_hash_str(&hash)));
        Command::LoadCommitDetail {
            client,
            workspace,
            repo,
            hash,
        }
    }

    fn on_key_commit_detail(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::Commits;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.commit_scroll = self.commit_scroll.saturating_add(1);
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.commit_scroll = self.commit_scroll.saturating_sub(1);
                Command::None
            }
            KeyCode::PageDown => {
                self.commit_scroll = self.commit_scroll.saturating_add(5);
                Command::None
            }
            KeyCode::PageUp => {
                self.commit_scroll = self.commit_scroll.saturating_sub(5);
                Command::None
            }
            KeyCode::Char('J') => {
                self.commit_scroll = self.commit_scroll.saturating_add(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.commit_scroll = self.commit_scroll.saturating_sub(10);
                Command::None
            }
            KeyCode::Char('d') => self.open_commit_diff(),
            _ => Command::None,
        }
    }

    /// 現在のコミットの diff を M1 の Diff ビューアで開く。
    fn open_commit_diff(&mut self) -> Command {
        let Some(hash) = self.current_commit_hash().map(str::to_string) else {
            self.status = Status::Error("コミット hash がありません".to_string());
            return Command::None;
        };
        self.diff = None;
        self.diff_return = Screen::CommitDetail;
        self.screen = Screen::Diff;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!(
            "コミット {} の diff を取得中…",
            short_hash_str(&hash)
        ));
        Command::LoadCommitDiff {
            client,
            workspace,
            repo,
            spec: hash,
        }
    }

    /// 現在のコミットの hash。
    fn current_commit_hash(&self) -> Option<&str> {
        self.current_commit
            .as_ref()
            .and_then(|commit| commit.hash.as_deref())
    }

    /// 既定ブランチの Source ルートへ遷移する。
    fn open_source_root(&mut self, reference: String) -> Command {
        self.open_source(reference, String::new())
    }

    /// 指定 `reference` / `path` の Source 画面へ遷移し、列挙取得を開始する。
    fn open_source(&mut self, reference: String, path: String) -> Command {
        self.screen = Screen::Source;
        self.source = Some(SourceState {
            reference,
            path,
            entries: SelectList::default(),
        });
        self.reload_source()
    }

    /// 現在の Source（reference/path）の列挙を再取得する。
    fn reload_source(&mut self) -> Command {
        let Some(source) = self.source.as_ref() else {
            return Command::None;
        };
        let reference = source.reference.clone();
        let path = source.path.clone();
        let location = source.location();
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("ソースを取得中…（{location}）"));
        Command::LoadSource {
            client,
            workspace,
            repo,
            reference,
            path,
        }
    }

    fn on_key_source(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            // Backspace / Esc = 親ディレクトリへ（ルートで repo へ戻る）。
            KeyCode::Esc | KeyCode::Backspace => self.source_up(),
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_next();
                }
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_prev();
                }
                Command::None
            }
            KeyCode::Char('J') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_next_by(10);
                }
                Command::None
            }
            KeyCode::Char('K') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_prev_by(10);
                }
                Command::None
            }
            KeyCode::Char('r') => self.reload_source(),
            KeyCode::Enter => self.source_enter(),
            _ => Command::None,
        }
    }

    /// 選択中エントリを開く（ディレクトリなら潜る / ファイルなら FileView）。
    fn source_enter(&mut self) -> Command {
        let Some(source) = self.source.as_ref() else {
            return Command::None;
        };
        let Some(entry) = source.entries.selected() else {
            return Command::None;
        };
        let reference = source.reference.clone();
        let path = entry.path_str().to_string();
        if entry.is_dir() {
            self.open_source(reference, path)
        } else {
            let mimetype = entry.mimetype.clone();
            self.open_file(reference, path, mimetype)
        }
    }

    /// 親ディレクトリへ戻る（ルートなら「ブラウズの戻り先」画面へ）。
    fn source_up(&mut self) -> Command {
        let parent = self.source.as_ref().and_then(|source| {
            parent_dir(&source.path).map(|parent| (source.reference.clone(), parent))
        });
        match parent {
            Some((reference, path)) => self.open_source(reference, path),
            None => {
                self.source = None;
                self.screen = self.browse_return;
                self.status = Status::Idle;
                Command::None
            }
        }
    }

    /// ファイル内容の FileView 画面へ遷移し、取得を開始する。
    fn open_file(&mut self, reference: String, path: String, mimetype: Option<String>) -> Command {
        self.open_file_path = Some(path.clone());
        self.open_file_mimetype = mimetype;
        self.file_view = None;
        self.screen = Screen::FileView;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("{path} を取得中…"));
        Command::LoadFile {
            client,
            workspace,
            repo,
            reference,
            path,
        }
    }

    fn on_key_file_view(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => return Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                return Command::None;
            }
            KeyCode::Esc => {
                self.screen = Screen::Source;
                self.status = Status::Idle;
                return Command::None;
            }
            _ => {}
        }

        let Some(view) = self.file_view.as_mut() else {
            return Command::None;
        };
        let page = view.viewport.max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => view.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k') => view.scroll_up(1),
            KeyCode::Char('J') => view.scroll_down(10),
            KeyCode::Char('K') => view.scroll_up(10),
            KeyCode::PageDown | KeyCode::Char('f') => view.scroll_down(page),
            KeyCode::PageUp | KeyCode::Char('b') => view.scroll_up(page),
            KeyCode::Char('g') | KeyCode::Home => view.scroll_to_top(),
            KeyCode::Char('G') | KeyCode::End => view.scroll_to_bottom(),
            _ => {}
        }
        Command::None
    }

    /// 詳細を再取得する（承認/マージ後の状態反映用。Loading 表示はしない）。
    fn refresh_detail(&mut self) -> Command {
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            return Command::None;
        };
        Command::LoadPrDetail {
            client,
            workspace,
            repo,
            id,
        }
    }

    /// コメント一覧を再取得する。
    fn refresh_comments(&mut self) -> Command {
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            return Command::None;
        };
        Command::LoadComments {
            client,
            workspace,
            repo,
            id,
        }
    }

    /// PR 詳細キャッシュ（[`App::pr_detail_cache`]）の該当エントリを取得-or-作成して
    /// `update` で部分更新する。キーは (`selected_repo`, `id`)。エントリが無い場合は
    /// `self.current_pr`（呼び出し元のガードで `id` と一致していることが確認済み）を
    /// 土台にして新規作成する。`selected_repo`/`current_pr` のどちらかが無ければ何もしない
    /// （レビュー系操作ができない状態と同じ扱い）。
    fn update_pr_detail_cache<F>(&mut self, id: u64, update: F)
    where
        F: FnOnce(&mut PrDetailCache),
    {
        let Some(repo_full_name) = self.selected_repo.clone() else {
            return;
        };
        let key = (repo_full_name, id);
        if let Some(entry) = self.pr_detail_cache.get_mut(&key) {
            update(entry);
            return;
        }
        let Some(current_pr) = self.current_pr.clone() else {
            return;
        };
        let mut entry = PrDetailCache {
            pr: current_pr,
            diffstat: Vec::new(),
            comments: Vec::new(),
        };
        update(&mut entry);
        self.pr_detail_cache.insert(key, entry);
    }

    /// 現在のリポジトリに紐づく PR 一覧キャッシュを全フィルタ分まとめて無効化する。
    ///
    /// 承認/変更要求/コメント/マージ成功後に呼ぶ。フィルタ別に承認バッジや PR の
    /// 掲載フィルタ（マージで OPEN→MERGED 等）を厳密に差分更新するより、対象 repo の
    /// キャッシュをまとめて削除して次回表示を必ず再取得（キャッシュミス）させる方が単純で
    /// 確実なため、この方式を採る。
    /// 現在の repo のキャッシュを、フィルタ・ソート・ページ番号を問わずすべて無効化する
    /// （承認/変更要求/コメント投稿/マージ後、一覧のバッジ・掲載フィルタが変わり得るため）。
    fn invalidate_pull_requests_cache_for_current_repo(&mut self) {
        let Some(repo) = self.repo_slug() else {
            return;
        };
        self.pull_requests_cache
            .retain(|(cached_repo, _filter, _sort, _page)| cached_repo != &repo);
    }

    /// 自分が現在この PR を承認しているか（participant を自分と照合）。
    fn i_approved(&self) -> bool {
        self.current_pr.as_ref().is_some_and(|pr| {
            pr.participants
                .iter()
                .any(|p| p.approved && user_is_me(p.user.as_ref(), &self.me))
        })
    }

    /// 自分が現在この PR に変更要求を出しているか。
    fn i_requested_changes(&self) -> bool {
        self.current_pr.as_ref().is_some_and(|pr| {
            pr.participants.iter().any(|p| {
                p.state.as_deref() == Some("changes_requested")
                    && user_is_me(p.user.as_ref(), &self.me)
            })
        })
    }

    /// 現在の PR の id。
    fn current_pr_id(&self) -> Option<u64> {
        self.current_pr.as_ref().map(|pr| pr.id)
    }

    /// `selected_repo`（`ws/repo`）から repo slug を取り出す。
    fn repo_slug(&self) -> Option<String> {
        self.selected_repo
            .as_deref()
            .and_then(|full| full.rsplit('/').next())
            .map(str::to_string)
    }

    /// レビュー系 API に必要な（client, workspace, repo slug）を揃える。
    fn review_context(&self) -> Option<(BitbucketClient, String, String)> {
        Some((
            self.client.clone()?,
            self.selected_workspace.clone()?,
            self.repo_slug()?,
        ))
    }

    // ---- インクリメンタル検索（`/`） ----

    /// 現在の画面がインクリメンタル検索に対応するリストを持つ場合、そのフィルタ文字列を返す。
    fn current_filter_text(&self) -> Option<String> {
        match self.screen {
            Screen::Workspaces => Some(self.workspaces.filter.clone()),
            Screen::Repositories => Some(self.repositories.filter.clone()),
            Screen::PullRequests => Some(self.pull_requests.filter.clone()),
            _ => None,
        }
    }

    /// 現在の画面のリストへフィルタ文字列を適用する（検索対象文字列は型ごとに決める）。
    fn set_current_list_filter(&mut self, filter: String) {
        match self.screen {
            Screen::Workspaces => {
                self.workspaces.set_filter(filter, |workspace: &Workspace| {
                    format!("{} {}", workspace.display_name(), workspace.slug)
                });
            }
            Screen::Repositories => {
                self.repositories.set_filter(filter, |repo: &Repository| {
                    format!("{} {}", repo.full_name, repo.name)
                });
            }
            Screen::PullRequests => {
                self.pull_requests.set_filter(filter, |pr: &PullRequest| {
                    format!("{} #{}", pr.title_str(), pr.id)
                });
            }
            _ => {}
        }
    }

    fn on_key_search_editing(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.search_editing = false;
                self.set_current_list_filter(String::new());
                Command::None
            }
            KeyCode::Enter => {
                // フィルタは維持したまま、通常のリスト操作へ戻る。
                self.search_editing = false;
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(mut filter) = self.current_filter_text() {
                    filter.pop();
                    self.set_current_list_filter(filter);
                }
                Command::None
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(mut filter) = self.current_filter_text() {
                    filter.push(ch);
                    self.set_current_list_filter(filter);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    // ---- ジャンプパレット（`Ctrl+K`） ----

    /// ジャンプパレットを開く（保持済みデータからの候補一覧を組み立てる）。
    fn open_jump_palette(&mut self) -> Command {
        let mut entries = SelectList::default();
        entries.set_items(self.build_jump_entries());
        self.jump_palette = Some(JumpPaletteState { entries });
        Command::None
    }

    /// ジャンプパレットの候補を、保持済みデータ（ワークスペース/リポジトリ/直近開いた PR）と
    /// 画面ジャンプから組み立てる。ワークスペース/リポジトリは現在保持している順（画面の
    /// 一覧と同じ順序）、PR は開いた順（新しい順）で並べる。
    fn build_jump_entries(&self) -> Vec<JumpEntry> {
        let mut entries = Vec::new();

        entries.push(JumpEntry {
            label: "ワークスペース一覧へ".to_string(),
            search_key: "workspaces ワークスペース一覧".to_string(),
            action: JumpAction::Screen(Screen::Workspaces),
        });
        if self.selected_workspace.is_some() {
            entries.push(JumpEntry {
                label: "リポジトリ一覧へ（現在のワークスペース）".to_string(),
                search_key: "repositories リポジトリ一覧".to_string(),
                action: JumpAction::Screen(Screen::Repositories),
            });
        }
        if self.selected_repo.is_some() {
            entries.push(JumpEntry {
                label: "PR 一覧へ（現在のリポジトリ）".to_string(),
                search_key: "pull requests PR プルリクエスト一覧".to_string(),
                action: JumpAction::Screen(Screen::PullRequests),
            });
        }

        for workspace in self.workspaces.items.iter().cloned() {
            entries.push(JumpEntry {
                label: format!("WS: {} ({})", workspace.display_name(), workspace.slug),
                search_key: format!("{} {}", workspace.display_name(), workspace.slug),
                action: JumpAction::Workspace(workspace.slug.clone()),
            });
        }

        for repo in self.repositories.items.iter().cloned() {
            entries.push(JumpEntry {
                label: format!("Repo: {}", repo.full_name),
                search_key: format!("{} {}", repo.full_name, repo.name),
                action: JumpAction::Repository(Box::new(repo)),
            });
        }

        for recent in &self.recent_prs {
            entries.push(JumpEntry {
                label: format!(
                    "PR #{} {} ({})",
                    recent.pr.id,
                    recent.pr.title_str(),
                    recent.repo_full_name
                ),
                search_key: format!(
                    "{} #{} {}",
                    recent.pr.title_str(),
                    recent.pr.id,
                    recent.repo_full_name
                ),
                action: JumpAction::RecentPr(Box::new(recent.clone())),
            });
        }

        entries
    }

    fn on_key_jump_palette(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.jump_palette = None;
                Command::None
            }
            KeyCode::Down => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    palette.entries.select_next();
                }
                Command::None
            }
            KeyCode::Up => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    palette.entries.select_prev();
                }
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    let mut filter = palette.entries.filter.clone();
                    filter.pop();
                    palette.entries.set_filter(filter, jump_entry_key);
                }
                Command::None
            }
            KeyCode::Enter => self.confirm_jump_palette(),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(palette) = self.jump_palette.as_mut() {
                    let mut filter = palette.entries.filter.clone();
                    filter.push(ch);
                    palette.entries.set_filter(filter, jump_entry_key);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// 選択中の候補で遷移を実行する。
    fn confirm_jump_palette(&mut self) -> Command {
        let Some(palette) = self.jump_palette.as_ref() else {
            return Command::None;
        };
        let Some(entry) = palette.entries.selected().cloned() else {
            return Command::None;
        };
        self.jump_palette = None;
        match entry.action {
            JumpAction::Screen(screen) => {
                self.screen = screen;
                Command::None
            }
            JumpAction::Workspace(slug) => self.jump_to_workspace(slug),
            JumpAction::Repository(repo) => self.enter_repository(*repo),
            JumpAction::RecentPr(recent) => {
                let recent = *recent;
                self.jump_to_pr(recent.workspace, recent.repo_full_name, recent.pr)
            }
        }
    }
}

/// participant のユーザーが自分かどうかをベストエフォートで判定する。
///
/// uuid / account_id / display_name のいずれかが一致すれば自分とみなす。`Me` 側が `None` の
/// フィールドは比較対象にしない（両者 `None` を誤って一致とみなさないため）。
fn user_is_me(user: Option<&User>, me: &Me) -> bool {
    let Some(user) = user else {
        return false;
    };
    (me.uuid.is_some() && user.uuid == me.uuid)
        || (me.account_id.is_some() && user.account_id == me.account_id)
        || (me.display_name.is_some() && user.display_name == me.display_name)
}

/// ディレクトリパスの親を返す。ルート（空文字）なら `None`（= repo へ戻る合図）。
///
/// 末尾スラッシュは無視する。`"a/b/c"` → `"a/b"`、`"a"` → `""`、`""` → `None`。
fn parent_dir(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    match trimmed.rfind('/') {
        Some(index) => Some(trimmed[..index].to_string()),
        None => Some(String::new()),
    }
}

/// ソースエントリを「ディレクトリ→ファイル」の順、各グループ内は名前昇順に並べ替える。
fn sort_src_entries(entries: &mut [SrcEntry]) {
    entries.sort_by(|a, b| {
        // ディレクトリを先に（true が先）。
        b.is_dir()
            .cmp(&a.is_dir())
            .then_with(|| a.name().to_lowercase().cmp(&b.name().to_lowercase()))
    });
}

/// hash 文字列の短縮形（先頭 8 文字）。
fn short_hash_str(hash: &str) -> String {
    hash.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn app() -> App {
        App::new(Config::default(), None)
    }

    fn client() -> BitbucketClient {
        BitbucketClient::new("me@example.com".to_string(), "token".to_string())
            .expect("client builds")
    }

    /// テスト用の `PageInfo`（総ページ数・次ページ有無を明示指定）。
    fn page_info(page: u32, total_pages: Option<u32>, has_next: bool) -> PageInfo {
        PageInfo {
            page,
            total_pages,
            has_next,
        }
    }

    /// 1 ページに収まる小さな一覧を想定した `PageInfo`（1 ページ目・総 1 ページ・次ページ無し）。
    fn single_page() -> PageInfo {
        page_info(1, Some(1), false)
    }

    /// レビュー系操作ができる状態（client + workspace + repo）を用意した App。
    fn review_app() -> App {
        let mut app = app();
        app.client = Some(client());
        app.selected_workspace = Some("acme".to_string());
        app.selected_repo = Some("acme/widget".to_string());
        app
    }

    fn make_repo(full_name: &str, main_branch: Option<&str>) -> Repository {
        let name = full_name.rsplit('/').next().unwrap_or(full_name);
        let mb = match main_branch {
            Some(branch) => format!(r#", "mainbranch": {{ "name": "{branch}" }}"#),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "full_name": "{full_name}", "name": "{name}", "is_private": false{mb} }}"#
        );
        serde_json::from_str(&json).expect("valid repo json")
    }

    fn make_pr(id: u64, state: &str) -> PullRequest {
        let json = format!(
            r#"{{ "id": {id}, "title": "PR {id}", "state": "{state}",
                  "author": {{ "display_name": "Alice" }},
                  "source": {{ "branch": {{ "name": "feature" }} }},
                  "destination": {{ "branch": {{ "name": "main" }} }},
                  "close_source_branch": true, "participants": [] }}"#
        );
        serde_json::from_str(&json).expect("valid pr json")
    }

    fn make_diffstat_entry(path: &str) -> DiffStatEntry {
        let json = format!(
            r#"{{ "status": "modified", "lines_added": 1, "lines_removed": 0,
                  "new": {{ "path": "{path}" }} }}"#
        );
        serde_json::from_str(&json).expect("valid diffstat entry json")
    }

    fn make_comment(id: u64, raw: &str) -> Comment {
        let json = format!(
            r#"{{ "id": {id}, "content": {{ "raw": "{raw}" }},
                  "user": {{ "display_name": "Alice" }}, "deleted": false }}"#
        );
        serde_json::from_str(&json).expect("valid comment json")
    }

    fn make_branch(name: &str, hash: &str) -> Branch {
        let json = format!(
            r#"{{ "name": "{name}", "target": {{ "hash": "{hash}",
                  "date": "2026-07-01T00:00:00Z", "message": "msg subject" }} }}"#
        );
        serde_json::from_str(&json).expect("valid branch json")
    }

    fn make_commit(hash: &str, message: &str) -> Commit {
        let json = format!(
            r#"{{ "hash": "{hash}", "message": "{message}",
                  "date": "2026-07-02T00:00:00Z", "author": {{ "raw": "Alice" }} }}"#
        );
        serde_json::from_str(&json).expect("valid commit json")
    }

    fn make_src_entry(entry_type: &str, path: &str) -> SrcEntry {
        let json = format!(r#"{{ "type": "{entry_type}", "path": "{path}" }}"#);
        serde_json::from_str(&json).expect("valid src entry json")
    }

    fn make_pipeline(uuid: &str, build: u64, state: &str, result: Option<&str>) -> Pipeline {
        let result_json = match result {
            Some(result_name) => format!(r#", "result": {{ "name": "{result_name}" }}"#),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "uuid": "{uuid}", "build_number": {build},
                  "state": {{ "name": "{state}"{result_json} }},
                  "target": {{ "type": "pipeline_ref_target", "ref_type": "branch",
                              "ref_name": "main", "selector": {{ "type": "default" }} }},
                  "trigger": {{ "name": "PUSH" }} }}"#
        );
        serde_json::from_str(&json).expect("valid pipeline json")
    }

    fn make_step(uuid: &str, name: &str, state: &str, result: Option<&str>) -> PipelineStep {
        let result_json = match result {
            Some(result_name) => format!(r#", "result": {{ "name": "{result_name}" }}"#),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "uuid": "{uuid}", "name": "{name}",
                  "state": {{ "name": "{state}"{result_json} }} }}"#
        );
        serde_json::from_str(&json).expect("valid step json")
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let mut app = app();
        assert!(matches!(
            app.update(Msg::Key(ctrl(KeyCode::Char('c')))),
            Command::Quit
        ));
    }

    #[test]
    fn ctrl_t_cycles_theme_and_persists_name() {
        let mut app = app();
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha);
        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha.next());
        assert_eq!(app.theme, app.theme_name.theme());
        assert_eq!(app.config.theme.as_deref(), Some(app.theme_name.as_str()));
    }

    #[test]
    fn ctrl_t_cycles_theme_globally_even_while_help_is_shown() {
        let mut app = app();
        app.show_help = true;
        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha.next());
        // グローバルキーとして扱うため、ヘルプの開閉状態には影響しない。
        assert!(app.show_help);
    }

    #[test]
    fn ctrl_t_invalidates_diff_rendered_line_cache() {
        let mut app = app();
        app.diff = Some(DiffState {
            parsed: parse_diff(" context\n"),
            scroll: 0,
            viewport: 0,
            title: "#1".to_string(),
            rendered_lines: Some(Vec::new()),
            file_index: 0,
            focus: DiffFocus::Body,
        });

        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));

        assert!(
            app.diff
                .as_ref()
                .expect("diff は保持されたまま")
                .rendered_lines
                .is_none(),
            "テーマ切替後は着色済み行キャッシュを無効化するべき"
        );
    }

    #[test]
    fn onboarding_typing_and_field_switch() {
        let mut app = app();
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        app.update(Msg::Key(key(KeyCode::Char('@'))));
        assert_eq!(app.onboarding.email.value(), "a@");
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.onboarding.field.0, Field::Token);
        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert_eq!(app.onboarding.token.value(), "t");
    }

    #[test]
    fn onboarding_submit_requires_both_fields() {
        let mut app = app();
        app.onboarding.field.0 = Field::Token;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(matches!(cmd, Command::None));
        assert!(app.onboarding.error.is_some());
    }

    #[test]
    fn onboarding_submit_emits_validate_command() {
        let mut app = app();
        app.onboarding.email = TextInput::from_str("user@example.com");
        app.onboarding.token = TextInput::from_str("secret");
        app.onboarding.field.0 = Field::Token;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        match cmd {
            Command::ValidateAuth { email, token } => {
                assert_eq!(email, "user@example.com");
                assert_eq!(token, "secret");
            }
            other => panic!("expected ValidateAuth, got {other:?}"),
        }
        assert!(app.onboarding.validating);
    }

    #[test]
    fn auth_failed_shows_error_and_stops_validating() {
        let mut app = app();
        app.onboarding.validating = true;
        app.update(Msg::AuthFailed(ApiError::Auth));
        assert!(!app.onboarding.validating);
        assert_eq!(app.onboarding.error, Some(ApiError::Auth.to_string()));
    }

    #[test]
    fn workspaces_loaded_selects_first() {
        let mut app = app();
        app.screen = Screen::Workspaces;
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
                Workspace {
                    slug: "a".to_string(),
                    name: Some("A".to_string()),
                    uuid: None,
                },
                Workspace {
                    slug: "b".to_string(),
                    name: None,
                    uuid: None,
                },
            ],
            page_info: single_page(),
        });
        assert_eq!(app.workspaces.state.selected(), Some(0));
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.workspaces.state.selected(), Some(1));
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.workspaces.state.selected(), Some(1));
    }

    #[test]
    fn repositories_loaded_ignored_for_stale_workspace() {
        let mut app = app();
        app.selected_workspace = Some("current".to_string());
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "stale".to_string(),
            repos: vec![make_repo("x/y", None)],
            page_info: single_page(),
        });
        assert!(app.repositories.items.is_empty());
    }

    #[test]
    fn selecting_repository_loads_pull_requests() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/widget", None)],
            page_info: single_page(),
        });
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequests);
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        match cmd {
            Command::LoadPullRequests {
                workspace,
                repo,
                filter,
                ..
            } => {
                assert_eq!(workspace, "acme");
                assert_eq!(repo, "widget");
                assert_eq!(filter, PrStateFilter::Open);
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_loaded_sets_items_when_fresh() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 2);
        assert_eq!(app.pull_requests.state.selected(), Some(0));
    }

    #[test]
    fn pull_requests_loaded_ignored_for_stale_filter() {
        let mut app = review_app();
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Merged,
            prs: vec![make_pr(1, "MERGED")],
            page_info: single_page(),
        });
        assert!(app.pull_requests.items.is_empty());
    }

    #[test]
    fn filter_key_reloads_with_new_state() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert_eq!(app.pr_state_filter, PrStateFilter::Merged);
        match cmd {
            Command::LoadPullRequests { filter, .. } => assert_eq!(filter, PrStateFilter::Merged),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn entering_detail_emits_batch_of_loads() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(7, "OPEN")]);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert_eq!(app.current_pr.as_ref().map(|pr| pr.id), Some(7));
        match cmd {
            Command::Batch(cmds) => {
                assert_eq!(cmds.len(), 3);
                assert!(matches!(cmds[0], Command::LoadPrDetail { id: 7, .. }));
                assert!(matches!(cmds[1], Command::LoadDiffStat { id: 7, .. }));
                assert!(matches!(cmds[2], Command::LoadComments { id: 7, .. }));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn detail_d_opens_diff_and_loads() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(9, "OPEN"));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert_eq!(app.screen, Screen::Diff);
        assert!(matches!(cmd, Command::LoadDiff { id: 9, .. }));
    }

    #[test]
    fn detail_o_without_html_url_reports_error_and_stays_on_screen() {
        // html_url が無い PR では実プロセスを起動せず Status::Error にするだけ
        // （`open` コマンドは呼ばれない分岐を検証する。実起動の分岐は
        // `PullRequest::html_url` 側の単体テストで代替する）。
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(20, "OPEN"));
        assert!(app.current_pr.as_ref().expect("pr").html_url().is_none());
        let cmd = app.update(Msg::Key(key(KeyCode::Char('o'))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn detail_o_without_current_pr_reports_error() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = None;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('o'))));
        assert!(matches!(cmd, Command::None));
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn detail_scroll_page_down_clamps_to_body_end() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        // 本文なし PR: ヘッダ 4 行 + プレースホルダ 1 行 = 5 行。viewport 2 → 上限 3。
        app.current_pr = Some(make_pr(11, "OPEN"));
        app.detail_viewport = 2;

        for _ in 0..10 {
            app.update(Msg::Key(key(KeyCode::PageDown)));
        }
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn detail_scroll_page_up_stops_at_zero() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(12, "OPEN"));
        app.detail_viewport = 2;

        app.update(Msg::Key(key(KeyCode::PageUp)));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn clamp_detail_scroll_limits_to_body_line_count() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(14, "OPEN"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // 5 行（ヘッダ 4 + プレースホルダ 1） - viewport 2 = 上限 3。
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn clamp_detail_scroll_accounts_for_multiline_body() {
        let mut app = review_app();
        let json = r#"{ "id": 15, "state": "OPEN", "description": "line1\nline2\nline3",
            "participants": [] }"#;
        app.current_pr = Some(serde_json::from_str(json).expect("valid pr json"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // ヘッダ 4 行 + 本文 3 行 = 7 行 - viewport 2 = 上限 5。
        assert_eq!(app.detail_scroll, 5);
    }

    #[test]
    fn clamp_detail_scroll_accounts_for_participant_panel() {
        let mut app = review_app();
        let json = r#"{ "id": 16, "state": "OPEN", "description": "line1\nline2",
            "participants": [
                { "user": { "display_name": "Bob" }, "approved": true, "state": "approved" },
                { "user": { "display_name": "Carol" }, "approved": false, "state": "changes_requested" }
            ] }"#;
        app.current_pr = Some(serde_json::from_str(json).expect("valid pr json"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // ヘッダ 4 行 + 承認/変更要求パネル 2 行 + 本文 2 行 = 8 行 - viewport 2 = 上限 6。
        assert_eq!(app.detail_scroll, 6);
    }

    #[test]
    fn diff_loaded_parses_and_scrolls() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".to_string(),
        });
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.parsed.len(), 4);
        // 既定フォーカスは本文なので、サイドバー導入前と同じく ↑↓/jk が直接スクロールする。
        assert_eq!(diff.focus, DiffFocus::Body);
        // ビューポート未設定でも 1 行スクロールできる。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 1);
    }

    /// 2 ファイル分の diff テキスト（各ファイル 3 行のコンテキスト行）。サイドバー関連の
    /// テストで使う。
    fn multi_file_diff_text() -> String {
        "diff --git a/one.txt b/one.txt\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,3 +1,3 @@\n\
 one line 0\n\
 one line 1\n\
 one line 2\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -1,3 +1,3 @@\n\
 two line 0\n\
 two line 1\n\
 two line 2\n"
            .to_string()
    }

    #[test]
    fn diff_tab_toggles_focus_between_files_and_body() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Files);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);
    }

    #[test]
    fn diff_sidebar_selection_moves_scroll_to_file_start() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        let second_file_start = app.diff.as_ref().expect("diff").parsed.files[1].start;

        app.update(Msg::Key(key(KeyCode::Tab))); // フォーカスをファイル一覧へ。
        app.update(Msg::Key(key(KeyCode::Char('j')))); // 1 つ下（2 番目のファイル）を選択。

        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 1);
        assert_eq!(diff.scroll, second_file_start);
    }

    #[test]
    fn diff_sidebar_selection_stays_within_bounds() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        app.update(Msg::Key(key(KeyCode::Tab)));

        // 先頭で上へ: 変化しない。
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.diff.as_ref().expect("diff").file_index, 0);

        // 末尾を超えて下へ連打しても最後のファイルで止まる（2 ファイルのみ）。
        for _ in 0..5 {
            app.update(Msg::Key(key(KeyCode::Down)));
        }
        assert_eq!(app.diff.as_ref().expect("diff").file_index, 1);
    }

    #[test]
    fn diff_body_focus_ignores_sidebar_navigation_and_scrolls_instead() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });

        // 本文フォーカスのまま（Tab を押していない）↓ を押すと本文が 1 行スクロールし、
        // サイドバー選択（file_index）は動かない。
        app.update(Msg::Key(key(KeyCode::Down)));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.scroll, 1);
        assert_eq!(diff.file_index, 0);
    }

    #[test]
    fn diff_next_file_key_syncs_sidebar_file_index_regardless_of_focus() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });

        // 本文フォーカスのまま `n` で次ファイルへジャンプしても file_index が同期する。
        app.update(Msg::Key(key(KeyCode::Char('n'))));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 1);
        assert_eq!(diff.scroll, diff.parsed.files[1].start);

        app.update(Msg::Key(key(KeyCode::Char('N'))));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 0);
        assert_eq!(diff.scroll, diff.parsed.files[0].start);
    }

    #[test]
    fn approve_toggles_to_true_when_not_yet_approved() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(3, "OPEN"));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('a'))));
        match cmd {
            Command::Approve { id, approve, .. } => {
                assert_eq!(id, 3);
                assert!(approve);
            }
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn approve_toggles_to_false_when_already_approved() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.me = Me {
            display_name: Some("Alice".to_string()),
            ..Me::default()
        };
        let json = r#"{ "id": 4, "state": "OPEN", "participants": [
            { "user": { "display_name": "Alice" }, "approved": true } ] }"#;
        app.current_pr = Some(serde_json::from_str(json).expect("pr json"));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('a'))));
        match cmd {
            Command::Approve { approve, .. } => assert!(!approve),
            other => panic!("expected Approve, got {other:?}"),
        }
    }

    #[test]
    fn merge_requires_modal_confirmation() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(5, "OPEN"));
        // 'M' はモーダルを開くだけで merge しない。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('M'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.merge_modal.is_some());
        // モーダルで Enter して初めて merge。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        match cmd {
            Command::Merge { id, params, .. } => {
                assert_eq!(id, 5);
                assert_eq!(params.merge_strategy, MergeStrategy::MergeCommit);
            }
            other => panic!("expected Merge, got {other:?}"),
        }
    }

    #[test]
    fn merge_modal_cycles_strategy_and_toggles_close() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(6, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('M'))));
        let initial_close = app.merge_modal.as_ref().expect("modal").close_source_branch;
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(
            app.merge_modal.as_ref().expect("modal").strategy(),
            MergeStrategy::Squash
        );
        app.update(Msg::Key(key(KeyCode::Char(' '))));
        assert_eq!(
            app.merge_modal.as_ref().expect("modal").close_source_branch,
            !initial_close
        );
    }

    #[test]
    fn merge_on_non_open_pr_is_rejected() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(8, "MERGED"));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('M'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.merge_modal.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn comment_editor_typing_and_submit() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(2, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert!(app.comment_editor.is_some());
        for ch in "LGTM".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        assert_eq!(app.comment_editor.as_ref().expect("editor").text, "LGTM");
        let cmd = app.update(Msg::Key(ctrl(KeyCode::Char('s'))));
        match cmd {
            Command::CreateComment { id, raw, .. } => {
                assert_eq!(id, 2);
                assert_eq!(raw, "LGTM");
            }
            other => panic!("expected CreateComment, got {other:?}"),
        }
    }

    #[test]
    fn comment_editor_esc_cancels() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(2, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.comment_editor.is_none());
    }

    #[test]
    fn review_action_done_reports_success_and_refreshes() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(3, "OPEN"));
        let cmd = app.update(Msg::ReviewActionDone {
            id: 3,
            message: "承認しました".to_string(),
        });
        assert_eq!(app.status, Status::Success("承認しました".to_string()));
        assert!(matches!(cmd, Command::LoadPrDetail { id: 3, .. }));
    }

    #[test]
    fn merge_done_reports_success_and_refreshes() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(3, "OPEN"));
        app.merge_modal = Some(MergeModal::new(true));
        let cmd = app.update(Msg::MergeDone { id: 3 });
        assert!(app.merge_modal.is_none());
        assert!(matches!(app.status, Status::Success(_)));
        assert!(matches!(cmd, Command::LoadPrDetail { id: 3, .. }));
    }

    #[test]
    fn action_failed_sets_error_and_clears_submitting() {
        let mut app = review_app();
        app.merge_modal = Some(MergeModal {
            strategy: 0,
            close_source_branch: false,
            submitting: true,
        });
        app.update(Msg::ActionFailed(ApiError::Forbidden("nope".to_string())));
        assert!(matches!(app.status, Status::Error(_)));
        assert!(!app.merge_modal.as_ref().expect("modal").submitting);
    }

    #[test]
    fn pr_detail_loaded_updates_current_pr() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(11, "OPEN"));
        app.status = Status::Loading("...".to_string());
        app.update(Msg::PrDetailLoaded {
            id: 11,
            pr: Box::new(make_pr(11, "MERGED")),
        });
        assert_eq!(app.current_pr.as_ref().expect("pr").state_str(), "MERGED");
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn help_toggle_and_dismiss() {
        let mut app = app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(key(KeyCode::Char('?'))));
        assert!(app.show_help);
        app.update(Msg::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }));
        assert!(!app.show_help);
    }

    // ---- M2: パイプライン監視 ----

    #[test]
    fn repositories_p_opens_pipelines() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('p'))));
        assert_eq!(app.screen, Screen::Pipelines);
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        match cmd {
            Command::LoadPipelines { repo, .. } => assert_eq!(repo, "widget"),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_capital_p_opens_pipelines() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('P'))));
        assert_eq!(app.screen, Screen::Pipelines);
        assert!(matches!(cmd, Command::LoadPipelines { .. }));
    }

    #[test]
    fn pipelines_loaded_sets_items_for_matching_repo() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![
                make_pipeline("{p1}", 1, "IN_PROGRESS", None),
                make_pipeline("{p2}", 2, "COMPLETED", Some("SUCCESSFUL")),
            ],
        });
        assert_eq!(app.pipelines.items.len(), 2);
        assert_eq!(app.pipelines.state.selected(), Some(0));
    }

    #[test]
    fn pipelines_loaded_ignored_for_stale_repo() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.update(Msg::PipelinesLoaded {
            repo: "other".to_string(),
            pipelines: vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)],
        });
        assert!(app.pipelines.items.is_empty());
    }

    #[test]
    fn refresh_keeps_selection_index() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines.set_items(vec![
            make_pipeline("{p1}", 1, "IN_PROGRESS", None),
            make_pipeline("{p2}", 2, "IN_PROGRESS", None),
        ]);
        app.pipelines.select_next();
        assert_eq!(app.pipelines.state.selected(), Some(1));
        // ポーリング更新で選択位置が保たれる。
        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![
                make_pipeline("{p1}", 1, "COMPLETED", Some("SUCCESSFUL")),
                make_pipeline("{p2}", 2, "IN_PROGRESS", None),
            ],
        });
        assert_eq!(app.pipelines.state.selected(), Some(1));
    }

    #[test]
    fn entering_pipeline_detail_emits_batch() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 7, "IN_PROGRESS", None)]);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PipelineDetail);
        assert_eq!(
            app.current_pipeline.as_ref().map(|p| p.uuid.clone()),
            Some("{p1}".to_string())
        );
        match cmd {
            Command::Batch(cmds) => {
                assert_eq!(cmds.len(), 2);
                assert!(matches!(&cmds[0], Command::LoadPipeline { uuid, .. } if uuid == "{p1}"));
                assert!(
                    matches!(&cmds[1], Command::LoadPipelineSteps { uuid, .. } if uuid == "{p1}")
                );
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn pipeline_detail_enter_opens_step_log() {
        let mut app = review_app();
        app.screen = Screen::PipelineDetail;
        app.current_pipeline = Some(make_pipeline("{p1}", 3, "IN_PROGRESS", None));
        app.pipeline_steps
            .set_items(vec![make_step("{s1}", "Build", "IN_PROGRESS", None)]);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::StepLog);
        assert_eq!(app.open_step_uuid.as_deref(), Some("{s1}"));
        match cmd {
            Command::LoadStepLog {
                pipeline_uuid,
                step_uuid,
                ..
            } => {
                assert_eq!(pipeline_uuid, "{p1}");
                assert_eq!(step_uuid, "{s1}");
            }
            other => panic!("expected LoadStepLog, got {other:?}"),
        }
    }

    #[test]
    fn step_log_loaded_parses_and_scrolls() {
        let mut app = review_app();
        app.current_pipeline = Some(make_pipeline("{p1}", 3, "IN_PROGRESS", None));
        app.pipeline_steps
            .set_items(vec![make_step("{s1}", "Build", "IN_PROGRESS", None)]);
        app.open_step_uuid = Some("{s1}".to_string());
        app.screen = Screen::StepLog;
        app.update(Msg::StepLogLoaded {
            step_uuid: "{s1}".to_string(),
            text: Some("line1\nline2\nline3\n".to_string()),
        });
        let log = app.step_log.as_ref().expect("log present");
        assert_eq!(log.lines.len(), 3);
        assert!(!log.missing);
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.step_log.as_ref().expect("log").scroll, 1);
    }

    #[test]
    fn step_log_missing_shows_placeholder() {
        let mut app = review_app();
        app.current_pipeline = Some(make_pipeline("{p1}", 3, "IN_PROGRESS", None));
        app.pipeline_steps
            .set_items(vec![make_step("{s1}", "Build", "IN_PROGRESS", None)]);
        app.open_step_uuid = Some("{s1}".to_string());
        app.screen = Screen::StepLog;
        app.update(Msg::StepLogLoaded {
            step_uuid: "{s1}".to_string(),
            text: None,
        });
        let log = app.step_log.as_ref().expect("log present");
        assert!(log.missing);
        assert_eq!(log.lines, vec!["(ログなし)".to_string()]);
    }

    #[test]
    fn stop_requires_active_and_modal_confirmation() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 3, "IN_PROGRESS", None)]);
        // 'S' はモーダルを開くだけで停止しない。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.confirm_modal.is_some());
        // モーダルで Enter して初めて停止。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        match cmd {
            Command::StopPipeline { uuid, .. } => assert_eq!(uuid, "{p1}"),
            other => panic!("expected StopPipeline, got {other:?}"),
        }
    }

    #[test]
    fn stop_rejected_on_completed_pipeline() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines.set_items(vec![make_pipeline(
            "{p1}",
            3,
            "COMPLETED",
            Some("SUCCESSFUL"),
        )]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.confirm_modal.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn rerun_requires_modal_and_emits_trigger_with_target() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 3, "COMPLETED", Some("FAILED"))]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('R'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.confirm_modal.is_some());
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        match cmd {
            Command::TriggerPipeline { target, .. } => {
                assert_eq!(target.ref_name.as_deref(), Some("main"));
            }
            other => panic!("expected TriggerPipeline, got {other:?}"),
        }
    }

    #[test]
    fn confirm_modal_esc_cancels() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 3, "IN_PROGRESS", None)]);
        app.update(Msg::Key(key(KeyCode::Char('S'))));
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.confirm_modal.is_none());
    }

    #[test]
    fn tick_refreshes_pipelines_when_active() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)]);
        assert!(matches!(
            app.update(Msg::Tick),
            Command::LoadPipelines { .. }
        ));
    }

    #[test]
    fn tick_noop_when_all_complete() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines.set_items(vec![make_pipeline(
            "{p1}",
            1,
            "COMPLETED",
            Some("SUCCESSFUL"),
        )]);
        assert!(matches!(app.update(Msg::Tick), Command::None));
    }

    #[test]
    fn tick_noop_when_auto_refresh_off() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.auto_refresh = false;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)]);
        assert!(matches!(app.update(Msg::Tick), Command::None));
    }

    #[test]
    fn tick_refreshes_detail_when_step_active() {
        let mut app = review_app();
        app.screen = Screen::PipelineDetail;
        app.current_pipeline = Some(make_pipeline("{p1}", 1, "COMPLETED", Some("SUCCESSFUL")));
        app.pipeline_steps
            .set_items(vec![make_step("{s1}", "Build", "IN_PROGRESS", None)]);
        assert!(matches!(app.update(Msg::Tick), Command::Batch(_)));
    }

    #[test]
    fn pipeline_action_done_stop_refreshes_and_clears_modal() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)]);
        app.confirm_modal = Some(ConfirmModal {
            action: PipelineAction::Stop,
            pipeline_uuid: "{p1}".to_string(),
            build_label: "#1".to_string(),
            target: None,
            submitting: true,
        });
        let cmd = app.update(Msg::PipelineActionDone {
            action: PipelineAction::Stop,
        });
        assert!(app.confirm_modal.is_none());
        assert!(matches!(app.status, Status::Success(_)));
        assert!(matches!(cmd, Command::LoadPipelines { .. }));
    }

    #[test]
    fn pipeline_action_done_rerun_navigates_to_list() {
        let mut app = review_app();
        app.screen = Screen::PipelineDetail;
        app.current_pipeline = Some(make_pipeline("{p1}", 1, "COMPLETED", Some("FAILED")));
        let cmd = app.update(Msg::PipelineActionDone {
            action: PipelineAction::Rerun,
        });
        assert_eq!(app.screen, Screen::Pipelines);
        assert!(matches!(cmd, Command::LoadPipelines { .. }));
    }

    #[test]
    fn toggle_auto_refresh_flips_flag() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        assert!(app.auto_refresh);
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert!(!app.auto_refresh);
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert!(app.auto_refresh);
    }

    // ---- M3: リポジトリブラウズ ----

    #[test]
    fn repositories_b_opens_branches() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        assert_eq!(app.repo_main_branch.as_deref(), Some("main"));
        assert!(matches!(cmd, Command::LoadBranches { .. }));
    }

    #[test]
    fn repositories_s_opens_source_root_with_main_branch() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("develop"))]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.repo_main_branch.as_deref(), Some("develop"));
        let source = app.source.as_ref().expect("source state");
        assert_eq!(source.reference, "develop");
        assert!(source.path.is_empty());
        match cmd {
            Command::LoadSource {
                reference, path, ..
            } => {
                assert_eq!(reference, "develop");
                assert_eq!(path, "");
            }
            other => panic!("expected LoadSource, got {other:?}"),
        }
    }

    #[test]
    fn repositories_s_falls_back_to_main_without_mainbranch() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('s'))));
        match cmd {
            Command::LoadSource { reference, .. } => assert_eq!(reference, "main"),
            other => panic!("expected LoadSource, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_b_and_s_enter_browse() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.repo_main_branch = Some("trunk".to_string());
        let cmd = app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        assert!(matches!(cmd, Command::LoadBranches { .. }));

        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);
        match cmd {
            Command::LoadSource { reference, .. } => assert_eq!(reference, "trunk"),
            other => panic!("expected LoadSource, got {other:?}"),
        }
    }

    // ---- ブラウズの戻り先（B: Branches/Source が「入って来た画面」へ戻る） ----

    #[test]
    fn branches_esc_returns_to_repositories_when_opened_from_repositories() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
    }

    #[test]
    fn branches_esc_returns_to_pull_requests_when_opened_from_pull_requests() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn source_root_esc_returns_to_repositories_when_opened_from_repositories() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.source.is_none());
    }

    #[test]
    fn source_root_esc_returns_to_pull_requests_when_opened_from_pull_requests() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.repo_main_branch = Some("trunk".to_string());
        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        // ルートでの Backspace も Esc と同じ経路（`source_up`）を通る。
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn source_opened_from_branches_keeps_original_browse_origin() {
        // Branches の `s` は「ブラウズの戻り先」を更新しない（最初に入って来た画面のままにする）。
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        app.branches.set_items(vec![make_branch("dev", "aaaa1111")]);

        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        app.update(Msg::Key(key(KeyCode::Esc)));
        // Branches ではなく、最初にブラウズへ入った PullRequests へ戻る。
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn branches_to_commits_to_commit_detail_esc_steps_one_level_at_a_time() {
        // Branches→Commits→CommitDetail の途中段は「戻り先」を使わず、常に 1 段ずつ戻る。
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        app.branches.set_items(vec![make_branch("dev", "aaaa1111")]);

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Commits);
        app.commits.set_items(vec![make_commit("bbbb2222", "msg")]);

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::CommitDetail);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Commits);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Branches);

        // ここでようやく「ブラウズの戻り先」（PullRequests）が使われる。
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn branches_loaded_sets_items_for_matching_repo() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![
                make_branch("main", "aaaaaaaa1111"),
                make_branch("dev", "bbbbbbbb2222"),
            ],
            page_info: single_page(),
        });
        assert_eq!(app.branches.items.len(), 2);
        assert_eq!(app.branches.state.selected(), Some(0));
    }

    #[test]
    fn branches_loaded_ignored_for_stale_repo() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "other".to_string(),
            branches: vec![make_branch("main", "aaaa")],
            page_info: single_page(),
        });
        assert!(app.branches.items.is_empty());
    }

    #[test]
    fn branches_enter_opens_commits() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches
            .set_items(vec![make_branch("feature/x", "cccccccc3333")]);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Commits);
        assert_eq!(app.commits_revision.as_deref(), Some("feature/x"));
        match cmd {
            Command::LoadCommits { revision, .. } => {
                assert_eq!(revision.as_deref(), Some("feature/x"));
            }
            other => panic!("expected LoadCommits, got {other:?}"),
        }
    }

    #[test]
    fn branches_s_opens_source_root_of_branch() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches.set_items(vec![make_branch("dev", "dddd4444")]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);
        match cmd {
            Command::LoadSource {
                reference, path, ..
            } => {
                assert_eq!(reference, "dev");
                assert_eq!(path, "");
            }
            other => panic!("expected LoadSource, got {other:?}"),
        }
    }

    #[test]
    fn commits_loaded_matches_revision() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("aaaa1111", "x"), make_commit("bbbb2222", "y")],
        });
        assert_eq!(app.commits.items.len(), 2);
    }

    #[test]
    fn commits_loaded_ignored_for_stale_revision() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.update(Msg::CommitsLoaded {
            revision: Some("other".to_string()),
            commits: vec![make_commit("zzzz9999", "z")],
        });
        assert!(app.commits.items.is_empty());
    }

    #[test]
    fn commit_enter_opens_detail() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits
            .set_items(vec![make_commit("abcdef123456", "subject")]);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::CommitDetail);
        assert_eq!(app.current_commit_hash(), Some("abcdef123456"));
        match cmd {
            Command::LoadCommitDetail { hash, .. } => assert_eq!(hash, "abcdef123456"),
            other => panic!("expected LoadCommitDetail, got {other:?}"),
        }
    }

    #[test]
    fn commit_detail_d_opens_commit_diff() {
        let mut app = review_app();
        app.screen = Screen::CommitDetail;
        app.current_commit = Some(make_commit("abcdef123456", "subject"));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(app.diff_return, Screen::CommitDetail);
        match cmd {
            Command::LoadCommitDiff { spec, .. } => assert_eq!(spec, "abcdef123456"),
            other => panic!("expected LoadCommitDiff, got {other:?}"),
        }
    }

    #[test]
    fn commit_diff_loaded_parses_scrolls_and_esc_returns() {
        let mut app = review_app();
        app.current_commit = Some(make_commit("abcdef123456", "subject"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::CommitDetail;
        app.update(Msg::CommitDiffLoaded {
            spec: "abcdef123456".to_string(),
            text: "diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b\n".to_string(),
        });
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.parsed.len(), 4);
        assert_eq!(diff.title, "abcdef12");
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 1);
        // commit 由来の Diff は Esc で CommitDetail へ戻る。
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::CommitDetail);
    }

    #[test]
    fn commit_diff_loaded_ignored_for_stale_spec() {
        let mut app = review_app();
        app.current_commit = Some(make_commit("abcdef123456", "subject"));
        app.screen = Screen::Diff;
        app.update(Msg::CommitDiffLoaded {
            spec: "0000".to_string(),
            text: "diff --git a/x b/x\n".to_string(),
        });
        assert!(app.diff.is_none());
    }

    #[test]
    fn pr_diff_esc_returns_to_pull_request_detail() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert_eq!(app.screen, Screen::Diff);
        assert_eq!(app.diff_return, Screen::PullRequestDetail);
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequestDetail);
    }

    #[test]
    fn source_loaded_sorts_dirs_first_then_files() {
        let mut app = review_app();
        app.screen = Screen::Source;
        app.source = Some(SourceState {
            reference: "main".to_string(),
            path: String::new(),
            entries: SelectList::default(),
        });
        app.update(Msg::SourceLoaded {
            reference: "main".to_string(),
            path: String::new(),
            entries: vec![
                make_src_entry("commit_file", "README.md"),
                make_src_entry("commit_directory", "src"),
                make_src_entry("commit_file", "Cargo.toml"),
                make_src_entry("commit_directory", "docs"),
            ],
        });
        let entries = &app.source.as_ref().expect("source").entries.items;
        assert_eq!(entries.len(), 4);
        assert_eq!(entries[0].name(), "docs");
        assert_eq!(entries[1].name(), "src");
        assert_eq!(entries[2].name(), "Cargo.toml");
        assert_eq!(entries[3].name(), "README.md");
    }

    #[test]
    fn source_loaded_ignored_for_stale_path() {
        let mut app = review_app();
        app.screen = Screen::Source;
        app.source = Some(SourceState {
            reference: "main".to_string(),
            path: "src".to_string(),
            entries: SelectList::default(),
        });
        app.update(Msg::SourceLoaded {
            reference: "main".to_string(),
            path: "docs".to_string(),
            entries: vec![make_src_entry("commit_file", "docs/x.md")],
        });
        assert!(
            app.source
                .as_ref()
                .expect("source")
                .entries
                .items
                .is_empty()
        );
    }

    #[test]
    fn source_enter_descends_into_directory() {
        let mut app = review_app();
        app.screen = Screen::Source;
        let mut state = SourceState {
            reference: "main".to_string(),
            path: String::new(),
            entries: SelectList::default(),
        };
        state
            .entries
            .set_items(vec![make_src_entry("commit_directory", "src")]);
        app.source = Some(state);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.source.as_ref().expect("source").path, "src");
        match cmd {
            Command::LoadSource { path, .. } => assert_eq!(path, "src"),
            other => panic!("expected LoadSource, got {other:?}"),
        }
    }

    #[test]
    fn source_enter_opens_file_view() {
        let mut app = review_app();
        app.screen = Screen::Source;
        let mut state = SourceState {
            reference: "main".to_string(),
            path: "src".to_string(),
            entries: SelectList::default(),
        };
        state
            .entries
            .set_items(vec![make_src_entry("commit_file", "src/main.rs")]);
        app.source = Some(state);
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::FileView);
        assert_eq!(app.open_file_path.as_deref(), Some("src/main.rs"));
        match cmd {
            Command::LoadFile { path, .. } => assert_eq!(path, "src/main.rs"),
            other => panic!("expected LoadFile, got {other:?}"),
        }
    }

    #[test]
    fn source_up_navigates_to_parent_directory() {
        let mut app = review_app();
        app.screen = Screen::Source;
        app.source = Some(SourceState {
            reference: "main".to_string(),
            path: "src/tui".to_string(),
            entries: SelectList::default(),
        });
        let cmd = app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.source.as_ref().expect("source").path, "src");
        assert!(matches!(cmd, Command::LoadSource { .. }));
    }

    #[test]
    fn source_up_at_root_returns_to_repositories() {
        let mut app = review_app();
        app.screen = Screen::Source;
        app.source = Some(SourceState {
            reference: "main".to_string(),
            path: String::new(),
            entries: SelectList::default(),
        });
        let cmd = app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.source.is_none());
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn file_loaded_builds_scrollable_view() {
        let mut app = review_app();
        app.screen = Screen::FileView;
        app.open_file_path = Some("src/main.rs".to_string());
        app.open_file_mimetype = Some("text/x-rust".to_string());
        app.update(Msg::FileLoaded {
            path: "src/main.rs".to_string(),
            text: "a\nb\nc\nd\n".to_string(),
        });
        let view = app.file_view.as_ref().expect("file view");
        assert!(!view.missing);
        assert_eq!(view.lines.len(), 4);
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.file_view.as_ref().expect("view").scroll, 1);
    }

    #[test]
    fn file_loaded_binary_shows_placeholder() {
        let mut app = review_app();
        app.screen = Screen::FileView;
        app.open_file_path = Some("logo.png".to_string());
        app.open_file_mimetype = Some("image/png".to_string());
        app.update(Msg::FileLoaded {
            path: "logo.png".to_string(),
            text: "\u{0}\u{0}PNG".to_string(),
        });
        let view = app.file_view.as_ref().expect("file view");
        assert!(view.missing);
        assert_eq!(view.lines, vec!["(バイナリ表示不可)".to_string()]);
    }

    #[test]
    fn file_loaded_ignored_for_stale_path() {
        let mut app = review_app();
        app.screen = Screen::FileView;
        app.open_file_path = Some("a.rs".to_string());
        app.update(Msg::FileLoaded {
            path: "b.rs".to_string(),
            text: "x".to_string(),
        });
        assert!(app.file_view.is_none());
    }

    #[test]
    fn parent_dir_navigates_up() {
        assert_eq!(parent_dir(""), None);
        assert_eq!(parent_dir("src"), Some(String::new()));
        assert_eq!(parent_dir("src/tui"), Some("src".to_string()));
        assert_eq!(parent_dir("src/tui/"), Some("src".to_string()));
        assert_eq!(parent_dir("a/b/c"), Some("a/b".to_string()));
    }

    #[test]
    fn sort_src_entries_puts_dirs_first_alphabetically() {
        let mut entries = vec![
            make_src_entry("commit_file", "b.rs"),
            make_src_entry("commit_directory", "z_dir"),
            make_src_entry("commit_file", "a.rs"),
            make_src_entry("commit_directory", "a_dir"),
        ];
        sort_src_entries(&mut entries);
        assert_eq!(entries[0].name(), "a_dir");
        assert_eq!(entries[1].name(), "z_dir");
        assert_eq!(entries[2].name(), "a.rs");
        assert_eq!(entries[3].name(), "b.rs");
    }

    // ---- SelectList 検索（#1） ----

    #[test]
    fn select_list_filter_narrows_matches_case_insensitively() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["Alpha", "Beta", "Gamma"]);
        list.set_filter("PH".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        assert_eq!(list.selected(), Some(&"Alpha"));
    }

    #[test]
    fn select_list_empty_filter_restores_full_set() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a", "b", "c"]);
        list.set_filter("b".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![1]);
        list.set_filter(String::new(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 1, 2]);
    }

    #[test]
    fn select_list_selection_clamps_to_matches_range() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "a2", "b1", "a3"]);
        list.state.select(Some(3)); // "a3" を選択中。
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        // matches = [0, 1, 3]（a1, a2, a3）。選択位置はこの範囲にクランプされる。
        assert_eq!(list.matches, vec![0, 1, 3]);
        assert!(
            list.state
                .selected()
                .is_some_and(|pos| pos < list.matches.len())
        );
    }

    #[test]
    fn select_list_select_next_prev_operate_within_matches() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "b1", "a2", "b2"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 2]);
        assert_eq!(list.selected(), Some(&"a1"));
        list.select_next();
        assert_eq!(list.selected(), Some(&"a2"));
        // 末尾で停止する（"b1"/"b2" はフィルタ対象外なので飛ばされない）。
        list.select_next();
        assert_eq!(list.selected(), Some(&"a2"));
    }

    #[test]
    fn select_list_set_items_clears_filter() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a", "b"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        list.set_items(vec!["x", "y", "z"]);
        assert!(list.filter.is_empty());
        assert_eq!(list.matches, vec![0, 1, 2]);
    }

    #[test]
    fn select_list_set_items_keep_selection_preserves_identity_matches_for_unfiltered_screens() {
        // pipelines/branches/commits/source 等、検索を使わない画面の既存挙動を保証する回帰テスト。
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items(vec![1, 2, 3]);
        list.state.select(Some(2));
        list.set_items_keep_selection(vec![10, 20]);
        assert_eq!(list.matches, vec![0, 1]);
        assert_eq!(list.state.selected(), Some(1)); // 3 件→2 件でクランプ。
        assert_eq!(list.selected(), Some(&20));
    }

    // ---- Shift+J/K（10 件/10 行移動、#D） ----

    #[test]
    fn select_list_select_next_by_moves_ten_and_clamps_at_end() {
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items((0..25).collect());
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(10));
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(20));
        // 末尾（インデックス 24）でクランプする。
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(24));
    }

    #[test]
    fn select_list_select_prev_by_moves_ten_and_clamps_at_start() {
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items((0..25).collect());
        list.state.select(Some(15));
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), Some(5));
        // 先頭でクランプする（負にはならない）。
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), Some(0));
    }

    #[test]
    fn select_list_select_next_by_respects_filter_matches() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "b1", "a2", "b2", "a3"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 2, 4]);
        list.select_next_by(10);
        // フィルタ後は 3 件しかないため末尾（"a3"）でクランプする。
        assert_eq!(list.selected(), Some(&"a3"));
    }

    #[test]
    fn select_list_select_next_prev_by_are_noop_on_empty_list() {
        let mut list: SelectList<i32> = SelectList::default();
        list.select_next_by(10);
        assert_eq!(list.state.selected(), None);
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), None);
    }

    #[test]
    fn repositories_shift_j_k_move_by_ten_and_clamp() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        let repos: Vec<Repository> = (0..25)
            .map(|i| make_repo(&format!("acme/repo{i}"), None))
            .collect();
        app.repositories.set_items(repos);

        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.repositories.state.selected(), Some(10));
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.repositories.state.selected(), Some(0));
    }

    #[test]
    fn pull_request_detail_shift_j_moves_ten_and_clamps_to_body_end() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        // 本文なし PR: ヘッダ 4 行 + 本文プレースホルダ 1 行 = 5 行。viewport=1 なら上限は 4。
        app.detail_viewport = 1;
        app.current_pr = Some(make_pr(1, "OPEN"));
        app.detail_scroll = 0;

        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.detail_scroll, 4);
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn diff_shift_j_k_scroll_body_by_ten() {
        let mut app = review_app();
        app.screen = Screen::Diff;
        let lines = (0..50)
            .map(|i| format!("+line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("diff --git a/x b/x\n@@ -1,1 +1,50 @@\n{lines}\n");
        app.diff = Some(DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 5,
            title: "#1".to_string(),
            rendered_lines: None,
            file_index: 0,
            focus: DiffFocus::Body,
        });

        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 10);
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 0);
    }

    // ---- インクリメンタル検索（App 統合・#1） ----

    #[test]
    fn slash_key_enters_search_editing_and_filters_pull_requests_by_typing() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests
            .set_items(vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")]);

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        assert!(app.search_editing);

        for ch in "PR 2".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        assert_eq!(app.pull_requests.matches.len(), 1);
        assert_eq!(app.pull_requests.selected().map(|pr| pr.id), Some(2));

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(!app.search_editing);
        assert_eq!(app.pull_requests.filter, "PR 2");
        // Enter 確定後もフィルタは維持される。
        assert_eq!(app.pull_requests.matches.len(), 1);
    }

    #[test]
    fn esc_during_search_clears_filter_and_restores_full_list() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories.set_items(vec![
            make_repo("acme/foo", None),
            make_repo("acme/bar", None),
        ]);

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert_eq!(app.repositories.matches.len(), 1);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(!app.search_editing);
        assert!(app.repositories.filter.is_empty());
        assert_eq!(app.repositories.matches.len(), 2);
    }

    #[test]
    fn backspace_during_search_removes_last_filter_char() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![
            Workspace {
                slug: "acme".to_string(),
                name: None,
                uuid: None,
            },
            Workspace {
                slug: "other".to_string(),
                name: None,
                uuid: None,
            },
        ]);
        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert_eq!(app.workspaces.filter, "ac");
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.workspaces.filter, "a");
    }

    // ---- ソート（サーバサイド、#1） ----

    #[test]
    fn repositories_and_pull_requests_sort_default_to_recently_updated() {
        let app = review_app();
        assert_eq!(app.repositories_sort, ListSort::RecentlyUpdated);
        assert_eq!(app.pull_requests_sort, ListSort::RecentlyUpdated);
    }

    #[test]
    fn cycle_repositories_sort_cycles_through_all_four_and_reloads_page_one() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        // 3 ページ目にいても、ソート変更は常に 1 ページ目から取得し直す。
        app.repositories_page_info = page_info(3, Some(5), true);

        let expected = [
            ListSort::LeastRecentlyUpdated,
            ListSort::Newest,
            ListSort::Oldest,
            ListSort::RecentlyUpdated,
        ];
        for sort in expected {
            let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
            assert_eq!(app.repositories_sort, sort);
            match cmd {
                Command::LoadRepositories {
                    sort: cmd_sort,
                    page,
                    ..
                } => {
                    assert_eq!(cmd_sort, sort);
                    assert_eq!(page, 1);
                }
                other => panic!("expected LoadRepositories, got {other:?}"),
            }
        }
    }

    #[test]
    fn cycle_pull_requests_sort_reloads_page_one_preserving_filter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Merged;
        app.pull_requests_page_info = page_info(4, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        match cmd {
            Command::LoadPullRequests {
                sort, page, filter, ..
            } => {
                assert_eq!(sort, ListSort::LeastRecentlyUpdated);
                assert_eq!(page, 1);
                assert_eq!(filter, PrStateFilter::Merged);
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn repositories_cache_is_keyed_by_sort_and_does_not_leak_across_sorts() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "zeta");

        // ソート変更: 別ソートの 1 ページ目はキャッシュ未命中 → Loading（一覧クリア）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "alpha");

        // 元のソートへ戻す（4 回巡回して一周）とキャッシュ命中で即時表示される。
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Char('S'))));
        }
        assert_eq!(app.repositories_sort, ListSort::RecentlyUpdated);
        assert_eq!(app.repositories.items[0].name, "zeta");
        assert_eq!(app.status, Status::Idle);
    }

    // ---- 選択位置の保持（E: j/k 移動中に stale-while-revalidate で先頭へ戻るバグの修正） ----

    #[test]
    fn workspaces_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
                Workspace {
                    slug: "alpha".to_string(),
                    name: None,
                    uuid: None,
                },
                Workspace {
                    slug: "beta".to_string(),
                    name: None,
                    uuid: None,
                },
            ],
            page_info: single_page(),
        });
        // ユーザーが 2 番目（"beta"）を選択した状態で j/k 移動中とみなす。
        app.workspaces.state.select(Some(1));

        // 裏側の再検証（同一ページ）が同じ内容で届く。
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
                Workspace {
                    slug: "alpha".to_string(),
                    name: None,
                    uuid: None,
                },
                Workspace {
                    slug: "beta".to_string(),
                    name: None,
                    uuid: None,
                },
            ],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、"beta" のまま維持される。
        assert_eq!(
            app.workspaces.selected().map(|w| w.slug.as_str()),
            Some("beta")
        );
    }

    #[test]
    fn repositories_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![
                make_repo("acme/alpha", None),
                make_repo("acme/beta", None),
                make_repo("acme/gamma", None),
            ],
            page_info: single_page(),
        });
        // ユーザーが 2 番目（"beta"）を選択した状態で j/k 移動中とみなす。
        app.repositories.state.select(Some(1));

        // 裏側の再検証（同一 workspace/sort/page）が同じ内容で届く。
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![
                make_repo("acme/alpha", None),
                make_repo("acme/beta", None),
                make_repo("acme/gamma", None),
            ],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、"beta" のまま維持される。
        assert_eq!(
            app.repositories.selected().map(|r| r.full_name.as_str()),
            Some("acme/beta")
        );
    }

    #[test]
    fn repositories_loaded_same_context_revalidation_follows_identity_even_if_order_changes() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None), make_repo("acme/beta", None)],
            page_info: single_page(),
        });
        app.repositories.state.select(Some(1)); // "beta" を選択中。

        // 再検証結果で並び順が入れ替わっても（サーバソート下で他者の更新により順序が変動する
        // ことを想定）、識別子（full_name）で "beta" を追従する。
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/beta", None), make_repo("acme/alpha", None)],
            page_info: single_page(),
        });

        assert_eq!(
            app.repositories.selected().map(|r| r.full_name.as_str()),
            Some("acme/beta")
        );
        assert_eq!(app.repositories.state.selected(), Some(0)); // "beta" は新しい並びで先頭。
    }

    #[test]
    fn repositories_context_change_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None), make_repo("acme/beta", None)],
            page_info: page_info(1, Some(2), true),
        });
        app.repositories.state.select(Some(1));

        // 別ページへ移動（新しい文脈）: 先頭へリセットされる。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/gamma", None)],
            page_info: page_info(2, Some(2), false),
        });

        assert_eq!(app.repositories.state.selected(), Some(0));
    }

    #[test]
    fn pull_requests_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN"), make_pr(3, "OPEN")],
            page_info: single_page(),
        });
        // ユーザーが PR #2 を選択した状態で j/k 移動中とみなす。
        app.pull_requests.state.select(Some(1));

        // 裏側の再検証（同一 repo/filter/sort/page）が同じ内容で届く。
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN"), make_pr(3, "OPEN")],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、PR #2 のまま維持される。
        assert_eq!(app.pull_requests.selected().map(|pr| pr.id), Some(2));
    }

    #[test]
    fn pull_requests_context_change_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        app.pull_requests.state.select(Some(1));

        // フィルタ切替（新しい文脈）: 先頭へリセットされる。
        app.update(Msg::Key(key(KeyCode::Char('m'))));
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Merged,
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(9, "MERGED")],
            page_info: single_page(),
        });

        assert_eq!(app.pull_requests.state.selected(), Some(0));
    }

    // ---- ジャンプパレット（#2） ----

    #[test]
    fn ctrl_k_opens_jump_palette_from_any_authenticated_screen() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.jump_palette.is_some());
    }

    #[test]
    fn ctrl_k_does_not_open_on_onboarding_screen() {
        let mut app = app();
        assert_eq!(app.screen, Screen::Onboarding);
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(app.jump_palette.is_none());
    }

    #[test]
    fn ctrl_k_does_not_open_while_another_modal_or_search_is_active() {
        let mut comment_editor_app = review_app();
        comment_editor_app.screen = Screen::PullRequestDetail;
        comment_editor_app.comment_editor = Some(CommentEditor::default());
        comment_editor_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(comment_editor_app.jump_palette.is_none());

        let mut search_app = review_app();
        search_app.screen = Screen::PullRequests;
        search_app.update(Msg::Key(key(KeyCode::Char('/'))));
        search_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(search_app.jump_palette.is_none());
        assert!(search_app.search_editing);
    }

    #[test]
    fn jump_palette_esc_closes_without_navigating() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(app.jump_palette.is_some());
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::Workspaces);
    }

    #[test]
    fn jump_palette_typing_narrows_candidates() {
        let mut app = review_app();
        app.workspaces.set_items(vec![
            Workspace {
                slug: "acme".to_string(),
                name: Some("Acme".to_string()),
                uuid: None,
            },
            Workspace {
                slug: "other".to_string(),
                name: None,
                uuid: None,
            },
        ]);
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "other".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let palette = app.jump_palette.as_ref().expect("palette open");
        assert!(!palette.entries.matches.is_empty());
        assert!(
            palette
                .entries
                .visible()
                .all(|entry| entry.label.to_lowercase().contains("other"))
        );
    }

    #[test]
    fn jump_palette_backspace_widens_candidates_again() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        let narrowed = app
            .jump_palette
            .as_ref()
            .expect("open")
            .entries
            .matches
            .len();
        app.update(Msg::Key(key(KeyCode::Backspace)));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        let widened = app
            .jump_palette
            .as_ref()
            .expect("open")
            .entries
            .matches
            .len();
        assert!(widened >= narrowed);
    }

    #[test]
    fn jump_palette_screen_entries_depend_on_navigation_state() {
        let mut app = app();
        app.client = Some(client());
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        let palette = app.jump_palette.as_ref().expect("open");
        assert!(
            !palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::Repositories)))
        );
        app.update(Msg::Key(key(KeyCode::Esc)));

        app.selected_workspace = Some("acme".to_string());
        app.selected_repo = Some("acme/widget".to_string());
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        let palette = app.jump_palette.as_ref().expect("open");
        assert!(
            palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::Repositories)))
        );
        assert!(
            palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::PullRequests)))
        );
    }

    #[test]
    fn jump_palette_enter_on_screen_entry_switches_screen_only() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        // 先頭候補は常に「ワークスペース一覧へ」。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::Workspaces);
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn jump_palette_enter_on_workspace_entry_calls_jump_to_workspace() {
        let mut app = app();
        app.client = Some(client());
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "acme".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.selected_workspace.as_deref(), Some("acme"));
        assert_eq!(app.screen, Screen::Repositories);
        match cmd {
            Command::LoadRepositories { workspace, .. } => assert_eq!(workspace, "acme"),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn jump_palette_enter_on_repository_entry_calls_enter_repository() {
        let mut app = review_app();
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        app.screen = Screen::Repositories;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "widget".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        assert_eq!(app.screen, Screen::PullRequests);
        match cmd {
            Command::LoadPullRequests { repo, .. } => assert_eq!(repo, "widget"),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn opening_pr_detail_records_recent_pr() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(42, "OPEN")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.recent_prs.len(), 1);
        assert_eq!(app.recent_prs[0].pr.id, 42);
        assert_eq!(app.recent_prs[0].repo_full_name, "acme/widget");
    }

    #[test]
    fn jump_palette_enter_on_recent_pr_jumps_across_repositories() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(42, "OPEN")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequestDetail);

        // 別のリポジトリへ移動したとみなす（PR #42 は元のリポジトリのまま）。
        app.selected_repo = Some("acme/other".to_string());
        app.current_pr = None;
        app.screen = Screen::Repositories;

        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "42".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        assert_eq!(app.current_pr.as_ref().map(|pr| pr.id), Some(42));
        match cmd {
            Command::Batch(cmds) => {
                assert_eq!(cmds.len(), 3);
                assert!(matches!(cmds[0], Command::LoadPrDetail { id: 42, .. }));
                assert!(matches!(cmds[1], Command::LoadDiffStat { id: 42, .. }));
                assert!(matches!(cmds[2], Command::LoadComments { id: 42, .. }));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    // ---- キャッシュ（stale-while-revalidate） ----

    #[test]
    fn revisit_cache_evicts_oldest_entry_once_over_capacity() {
        let mut cache: RevisitCache<u32, u32> = RevisitCache::default();
        for i in 0..REVISIT_CACHE_MAX_ENTRIES as u32 {
            cache.insert(i, i);
        }
        assert!(cache.get(&0).is_some());

        // 上限を超える 1 件を追加すると、最も古い(0)が追い出される。
        cache.insert(REVISIT_CACHE_MAX_ENTRIES as u32, 999);
        assert!(cache.get(&0).is_none());
        assert!(cache.get(&1).is_some());
        assert_eq!(cache.get(&(REVISIT_CACHE_MAX_ENTRIES as u32)), Some(&999));
    }

    #[test]
    fn revisit_cache_overwriting_existing_key_does_not_evict() {
        let mut cache: RevisitCache<&str, u32> = RevisitCache::default();
        cache.insert("a", 1);
        cache.insert("a", 2);
        assert_eq!(cache.get(&"a"), Some(&2));
    }

    #[test]
    fn revisit_cache_retain_drops_entries_failing_predicate() {
        let mut cache: RevisitCache<&str, u32> = RevisitCache::default();
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.retain(|key| *key != "a");
        assert!(cache.get(&"a").is_none());
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn revisit_cache_retain_can_match_across_multiple_keys() {
        // `(String, PrStateFilter, u32)` のようなタプルキーで、一部フィールドだけが一致すれば
        // 一括で無効化できること（ページ番号を問わず repo 単位で無効化する用途）。
        let mut cache: RevisitCache<(String, u32), u32> = RevisitCache::default();
        cache.insert(("widget".to_string(), 1), 1);
        cache.insert(("widget".to_string(), 2), 2);
        cache.insert(("other".to_string(), 1), 3);
        cache.retain(|(repo, _page)| repo != "widget");
        assert!(cache.get(&("widget".to_string(), 1)).is_none());
        assert!(cache.get(&("widget".to_string(), 2)).is_none());
        assert_eq!(cache.get(&("other".to_string(), 1)), Some(&3));
    }

    #[test]
    fn revisiting_workspace_shows_cached_repositories_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);

        // 初回入場: キャッシュなし → 一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/widget", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items.len(), 1);

        // 別画面を経由して再訪する。
        app.screen = Screen::Workspaces;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 即座に一覧が表示され、Loading にはならない。
        assert_eq!(app.repositories.items.len(), 1);
        assert_eq!(app.status, Status::Idle);
        // それでも裏では再取得コマンドを発行する（stale-while-revalidate）。
        assert!(matches!(cmd, Command::LoadRepositories { .. }));
    }

    #[test]
    fn revisiting_workspace_keeps_current_sort_setting_and_shows_cached_data_for_it() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: single_page(),
        });

        // ソートを変更（サーバソートなので新しい問い合わせが必要＝キャッシュ未命中）。
        app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "alpha");

        // 別画面 → 再訪: サーバソートは「取得順」のようなクライアント側概念ではなく明示的な
        // 選択なので、再訪してもリセットされず維持される。維持されたソートのキャッシュに
        // 命中して即座に表示される。
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(key(KeyCode::Enter)));

        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.repositories.items[0].name, "alpha");
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn reentering_repository_shows_cached_pull_requests_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequests);
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 1);

        // Repositories へ戻り、同じ repo へ再入場する。
        app.screen = Screen::Repositories;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 即座に表示され、Loading にはならない。
        assert_eq!(app.pull_requests.items.len(), 1);
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn pull_requests_cache_is_keyed_by_filter_and_does_not_leak_across_filters() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });

        // Merged へ切り替え: Open 用キャッシュを誤って使わないこと。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        match cmd {
            Command::LoadPullRequests { filter, .. } => {
                assert_eq!(filter, PrStateFilter::Merged);
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_cache_restore_keeps_current_sort_setting() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(9, "OPEN")],
            page_info: single_page(),
        });
        app.update(Msg::Key(key(KeyCode::Char('S')))); // LeastRecentlyUpdated へ。
        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            sort: ListSort::LeastRecentlyUpdated,
            prs: vec![make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        // 別画面 → 再訪（手動リロードでもキャッシュ命中の経路を通る）。ソート設定は維持される。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('r'))));

        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.pull_requests.items[0].id, 2);
    }

    #[test]
    fn reopening_pr_detail_shows_cached_data_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(7, "OPEN")]);

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(app.diffstat.items.is_empty());
        assert!(app.comments.is_empty());
        assert!(matches!(cmd, Command::Batch(_)));

        app.update(Msg::PrDetailLoaded {
            id: 7,
            pr: Box::new(make_pr(7, "OPEN")),
        });
        app.update(Msg::DiffStatLoaded {
            id: 7,
            entries: vec![make_diffstat_entry("src/lib.rs")],
        });
        app.update(Msg::CommentsLoaded {
            id: 7,
            comments: vec![make_comment(1, "LGTM")],
        });
        assert_eq!(app.diffstat.items.len(), 1);
        assert_eq!(app.comments.len(), 1);

        // 一覧へ戻り、同じ PR を再度開く。
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 詳細/diffstat/コメントが即座に表示され、Loading にはならない。
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert_eq!(app.status, Status::Idle);
        assert_eq!(app.diffstat.items.len(), 1);
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.current_pr.as_ref().map(|pr| pr.id), Some(7));
        // それでも裏では詳細/diffstat/コメントの再取得コマンドを発行する。
        match cmd {
            Command::Batch(cmds) => assert_eq!(cmds.len(), 3),
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn review_action_success_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });

        // 再訪してキャッシュ命中（Idle）を確認しておく。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(app.status, Status::Idle);

        app.current_pr = Some(make_pr(1, "OPEN"));
        app.update(Msg::ReviewActionDone {
            id: 1,
            message: "承認しました".to_string(),
        });

        // 一覧キャッシュが無効化され、再訪すると Loading（キャッシュミス）に戻ること。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn merge_success_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(3, "OPEN")],
            page_info: single_page(),
        });

        app.current_pr = Some(make_pr(3, "OPEN"));
        app.merge_modal = Some(MergeModal::new(false));
        app.update(Msg::MergeDone { id: 3 });

        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn comment_posted_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(5, "OPEN")],
            page_info: single_page(),
        });

        app.current_pr = Some(make_pr(5, "OPEN"));
        app.comment_editor = Some(CommentEditor::default());
        app.update(Msg::CommentPosted { id: 5 });

        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    // ---- サーバサイド・ページネーション（1 ページ 40 件） ----

    #[test]
    fn repositories_next_page_dispatches_load_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadRepositories { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn repositories_next_page_is_noop_when_has_next_is_false() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(3, Some(3), false);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.repositories_page_info.page, 3);
    }

    #[test]
    fn repositories_prev_page_is_noop_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.repositories_page_info.page, 1);
    }

    #[test]
    fn repositories_prev_page_dispatches_load_with_decremented_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(2, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        match cmd {
            Command::LoadRepositories { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_next_page_preserves_current_filter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Merged;
        app.pull_requests_page_info = page_info(2, Some(5), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadPullRequests { page, filter, .. } => {
                assert_eq!(page, 3);
                assert_eq!(filter, PrStateFilter::Merged);
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn workspaces_next_page_dispatches_load_workspaces_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces_page_info = page_info(1, Some(2), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadWorkspaces { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadWorkspaces, got {other:?}"),
        }
    }

    #[test]
    fn page_nav_keys_do_nothing_on_non_paged_screens() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(1, "OPEN"));
        assert!(matches!(
            app.update(Msg::Key(key(KeyCode::Char(']')))),
            Command::None
        ));
        assert!(matches!(
            app.update(Msg::Key(key(KeyCode::Char('[')))),
            Command::None
        ));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_prompt_opens_with_g_and_closes_with_esc() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.page_jump.is_some());
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_does_not_open_on_non_paged_screen() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(1, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_digit_input_and_enter_navigates_clamped_to_total_pages() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        for ch in "99".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        assert_eq!(app.page_jump.as_ref().expect("open").input, "99");

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.page_jump.is_none());
        match cmd {
            // 総ページ数(3)でクランプされる。
            Command::LoadRepositories { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn page_jump_backspace_removes_last_digit() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        app.update(Msg::Key(key(KeyCode::Char('1'))));
        app.update(Msg::Key(key(KeyCode::Char('2'))));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.page_jump.as_ref().expect("open").input, "1");
    }

    #[test]
    fn page_jump_ignores_non_digit_characters() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert_eq!(app.page_jump.as_ref().expect("open").input, "");
    }

    #[test]
    fn page_jump_enter_with_empty_input_reports_error_and_closes_modal() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(matches!(cmd, Command::None));
        assert!(app.page_jump.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn goto_page_clamps_below_one_to_one_when_total_unknown() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests_page_info = page_info(2, None, true);
        let cmd = app.goto_page(0);
        match cmd {
            Command::LoadPullRequests { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn workspaces_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces_page_info = page_info(2, None, false);
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![Workspace {
                slug: "stale".to_string(),
                name: None,
                uuid: None,
            }],
            page_info: page_info(1, None, true),
        });
        assert!(app.workspaces.items.is_empty());
    }

    #[test]
    fn repositories_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(2, None, false);
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/stale", None)],
            page_info: page_info(1, None, true),
        });
        assert!(app.repositories.items.is_empty());
    }

    #[test]
    fn pull_requests_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.pull_requests_page_info = page_info(2, None, false);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN")],
            page_info: page_info(1, None, true),
        });
        assert!(app.pull_requests.items.is_empty());
    }

    #[test]
    fn repositories_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/foo", None)],
            page_info: page_info(1, Some(2), true),
        });

        // 2 ページ目へ移動: 未キャッシュなので一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/bar", None)],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.repositories.items[0].name, "bar");

        // 1 ページ目へ戻る: キャッシュ命中で即座に表示（Loading にならない）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.repositories.items[0].name, "foo");
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadRepositories { .. }));
    }

    #[test]
    fn pull_requests_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN")],
            page_info: page_info(1, Some(2), true),
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(2, "OPEN")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.pull_requests.items[0].id, 1);
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn changing_page_clears_confirmed_search_filter() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/foo", None), make_repo("acme/bar", None)],
            page_info: page_info(1, Some(2), true),
        });

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Enter))); // 検索確定（Enter 後もフィルタは維持される）。
        assert_eq!(app.repositories.filter, "f");

        // ページ移動: 表示される 40 件が入れ替わるため、前ページの検索フィルタは引き継がない。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.repositories.filter.is_empty());
    }

    #[test]
    fn changing_page_preserves_current_sort_setting() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: page_info(1, Some(2), true),
        });
        app.update(Msg::Key(key(KeyCode::Char('S')))); // LeastRecentlyUpdated へ（1 ページ目に戻る）。
        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: page_info(1, Some(2), true),
        });

        // ページ移動（`]`）はソートを変えない。次ページの取得も同じソートで発行される。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadRepositories { sort, page, .. } => {
                assert_eq!(sort, ListSort::LeastRecentlyUpdated);
                assert_eq!(page, 2);
            }
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/gamma", None)],
            page_info: page_info(2, Some(2), false),
        });

        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.repositories_page_info.page, 2);
    }

    #[test]
    fn switching_pr_filter_resets_to_page_one() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.pull_requests_page_info = page_info(3, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('m')))); // Merged へ切替
        match cmd {
            Command::LoadPullRequests { page, filter, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter, PrStateFilter::Merged);
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn reload_key_reuses_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::Open;
        app.pull_requests_page_info = page_info(2, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        match cmd {
            Command::LoadPullRequests { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    // ---- Branches のサーバサイド・ページネーション ----

    #[test]
    fn branches_next_page_dispatches_load_branches_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn branches_prev_page_does_nothing_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn branches_page_jump_navigates_clamped_to_total_pages() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        for ch in "99".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.page_jump.is_none());
        match cmd {
            // 総ページ数(3)でクランプされる。
            Command::LoadBranches { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn branches_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(2, None, false);
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("stale", "aaaa")],
            page_info: page_info(1, None, true),
        });
        assert!(app.branches.items.is_empty());
    }

    #[test]
    fn branches_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("main", "aaaa1111")],
            page_info: page_info(1, Some(2), true),
        });

        // 2 ページ目へ移動: 未キャッシュなので一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.branches.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadBranches { .. }));

        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("dev", "bbbb2222")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.branches.items[0].name_str(), "dev");

        // 1 ページ目へ戻る: キャッシュ命中で即座に表示（Loading にならない）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.branches.items[0].name_str(), "main");
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadBranches { .. }));
    }

    #[test]
    fn branches_revalidation_in_same_context_keeps_selection_by_name() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![
                make_branch("main", "aaaa1111"),
                make_branch("dev", "bbbb2222"),
            ],
            page_info: single_page(),
        });
        app.branches.state.select(Some(1)); // "dev" を選択中。

        // 同一文脈（同じ repo/ページ）の再検証: 順序が変わっても "dev" への選択を維持する。
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![
                make_branch("dev", "bbbb2222"),
                make_branch("main", "aaaa1111"),
            ],
            page_info: single_page(),
        });
        assert_eq!(
            app.branches.selected().map(|branch| branch.name_str()),
            Some("dev")
        );
    }

    #[test]
    fn branches_new_navigation_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(2), true);
        app.branches.set_items(vec![
            make_branch("main", "aaaa1111"),
            make_branch("dev", "bbbb2222"),
        ]);
        app.branches.state.select(Some(1));

        // 新規ナビゲーション（ページ変更）: 選択は先頭にリセットされる。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("feature/x", "cccc3333")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.branches.state.selected(), Some(0));
    }

    #[test]
    fn opening_branches_from_repositories_starts_at_page_one() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn reloading_branches_reuses_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(2, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }
}

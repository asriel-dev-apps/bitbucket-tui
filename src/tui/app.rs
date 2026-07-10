//! アプリ状態・画面遷移・`update()`。
//!
//! bubbletea の `Model`/`Msg`/`Cmd` に相当する構造。`update()` は状態を更新し、副作用を
//! [`Command`] として返す。実際の非同期実行（API 呼び出しの spawn）は `event` モジュールが行う。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::api::{
    ApiError, BitbucketClient, Branch, Comment, Commit, DiffStatEntry, MergeParams, MergeStrategy,
    Pipeline, PipelineStep, PipelineTarget, PullRequest, Repository, SrcEntry, User, Workspace,
};
use crate::auth;
use crate::config::Config;
use crate::tui::diff::{ParsedDiff, parse as parse_diff};
use crate::tui::logview::LogView;
use crate::tui::onboarding::{Field, OnboardingState, TextInput};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// Diff 画面の表示状態（スクロール・ファイル境界ジャンプ）。
#[derive(Debug, Clone, Default)]
pub struct DiffState {
    pub parsed: ParsedDiff,
    /// 先頭からのスクロール行数。
    pub scroll: usize,
    /// 直近描画時のビューポート高さ（スクロール上限計算に使う。`ui` が毎フレーム更新）。
    pub viewport: usize,
    /// 見出し（例: `#12`）。
    pub title: String,
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

    fn next_file(&mut self) {
        if let Some(&position) = self
            .parsed
            .file_starts
            .iter()
            .find(|&&start| start > self.scroll)
        {
            self.scroll = position.min(self.max_scroll());
        }
    }

    fn prev_file(&mut self) {
        if let Some(&position) = self
            .parsed
            .file_starts
            .iter()
            .rev()
            .find(|&&start| start < self.scroll)
        {
            self.scroll = position;
        }
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
    /// ワークスペース一覧の取得完了。
    WorkspacesLoaded(Vec<Workspace>),
    /// リポジトリ一覧の取得完了。
    RepositoriesLoaded {
        workspace: String,
        repos: Vec<Repository>,
    },
    /// ワークスペース/リポジトリ取得の失敗。
    LoadFailed(ApiError),
    /// PR 一覧の取得完了。
    PullRequestsLoaded {
        repo: String,
        filter: PrStateFilter,
        prs: Vec<PullRequest>,
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
    /// ブランチ一覧の取得完了。
    BranchesLoaded { repo: String, branches: Vec<Branch> },
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
    /// ワークスペース一覧を取得する。
    LoadWorkspaces { client: BitbucketClient },
    /// 指定ワークスペースのリポジトリ一覧を取得する。
    LoadRepositories {
        client: BitbucketClient,
        workspace: String,
    },
    /// PR 一覧を取得する。
    LoadPullRequests {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        filter: PrStateFilter,
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
    /// ブランチ一覧を取得する。
    LoadBranches {
        client: BitbucketClient,
        workspace: String,
        repo: String,
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
/// `T: Default` を要求しないよう `Default` は手動実装する。
#[derive(Debug)]
pub struct SelectList<T> {
    pub items: Vec<T>,
    pub state: ListState,
}

impl<T> Default for SelectList<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
        }
    }
}

impl<T> SelectList<T> {
    /// 要素を差し替え、選択位置を先頭（空なら未選択）にリセットする。
    pub fn set_items(&mut self, items: Vec<T>) {
        self.state
            .select(if items.is_empty() { None } else { Some(0) });
        self.items = items;
    }

    /// 要素を差し替えつつ、選択インデックスを可能な限り維持する（新しい件数にクランプ）。
    ///
    /// 自動ポーリングでの一覧リフレッシュ時に、選択位置が毎回先頭へ戻らないようにするために使う。
    pub fn set_items_keep_selection(&mut self, items: Vec<T>) {
        let selection = if items.is_empty() {
            None
        } else {
            Some(self.state.selected().unwrap_or(0).min(items.len() - 1))
        };
        self.state.select(selection);
        self.items = items;
    }

    /// 選択を 1 つ下へ（末尾で停止）。
    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let next = match self.state.selected() {
            Some(index) if index + 1 < self.items.len() => index + 1,
            Some(index) => index,
            None => 0,
        };
        self.state.select(Some(next));
    }

    /// 選択を 1 つ上へ（先頭で停止）。
    pub fn select_prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        let prev = match self.state.selected() {
            Some(0) | None => 0,
            Some(index) => index - 1,
        };
        self.state.select(Some(prev));
    }

    /// 現在選択中の要素。
    pub fn selected(&self) -> Option<&T> {
        self.state
            .selected()
            .and_then(|index| self.items.get(index))
    }
}

/// アプリ全体の状態。
pub struct App {
    pub screen: Screen,
    pub config: Config,
    pub client: Option<BitbucketClient>,
    pub me: Me,
    pub onboarding: OnboardingState,
    pub workspaces: SelectList<Workspace>,
    pub repositories: SelectList<Repository>,
    pub selected_workspace: Option<String>,
    pub selected_repo: Option<String>,
    /// 選択リポジトリの既定ブランチ名（`mainbranch.name`）。Source ルートに使う。
    pub repo_main_branch: Option<String>,
    pub pull_requests: SelectList<PullRequest>,
    pub pr_state_filter: PrStateFilter,
    pub current_pr: Option<PullRequest>,
    pub diffstat: SelectList<DiffStatEntry>,
    pub comments: Vec<Comment>,
    pub detail_scroll: u16,
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
    pub branches: SelectList<Branch>,
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
        Self {
            screen: Screen::Onboarding,
            config,
            client,
            me,
            onboarding,
            workspaces: SelectList::default(),
            repositories: SelectList::default(),
            selected_workspace: None,
            selected_repo: None,
            repo_main_branch: None,
            pull_requests: SelectList::default(),
            pr_state_filter: PrStateFilter::Open,
            current_pr: None,
            diffstat: SelectList::default(),
            comments: Vec::new(),
            detail_scroll: 0,
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
            branches: SelectList::default(),
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
        }
    }

    /// 起動直後に実行すべきコマンドを返し、初期画面を確定する。
    ///
    /// 認証済みなら Workspaces へ進み一覧取得を開始、未認証なら Onboarding に留まる。
    pub fn init_command(&mut self) -> Command {
        if let Some(client) = &self.client {
            self.screen = Screen::Workspaces;
            self.status = Status::Loading("ワークスペースを取得中…".to_string());
            return Command::LoadWorkspaces {
                client: client.clone(),
            };
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
            Msg::WorkspacesLoaded(workspaces) => {
                self.status = Status::Idle;
                self.workspaces.set_items(workspaces);
                Command::None
            }
            Msg::RepositoriesLoaded { workspace, repos } => {
                // 取得中に別ワークスペースへ切り替えていた場合は破棄。
                if self.selected_workspace.as_deref() == Some(workspace.as_str()) {
                    self.status = Status::Idle;
                    self.repositories.set_items(repos);
                }
                Command::None
            }
            Msg::LoadFailed(error) => {
                self.status = Status::Error(error.to_string());
                Command::None
            }
            Msg::PullRequestsLoaded { repo, filter, prs } => {
                if self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.pr_state_filter == filter
                {
                    self.status = Status::Idle;
                    self.pull_requests.set_items(prs);
                }
                Command::None
            }
            Msg::PrDetailLoaded { id, pr } => {
                if self.current_pr_id() == Some(id) {
                    self.clear_loading();
                    self.current_pr = Some(*pr);
                }
                Command::None
            }
            Msg::DiffStatLoaded { id, entries } => {
                if self.current_pr_id() == Some(id) {
                    self.diffstat.set_items(entries);
                }
                Command::None
            }
            Msg::CommentsLoaded { id, comments } => {
                if self.current_pr_id() == Some(id) {
                    self.comments = comments
                        .into_iter()
                        .filter(|comment| !comment.deleted)
                        .collect();
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
                    });
                }
                Command::None
            }
            Msg::ReviewActionDone { id, message } => {
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(message);
                    return self.refresh_detail();
                }
                Command::None
            }
            Msg::CommentPosted { id } => {
                self.comment_editor = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success("コメントを投稿しました".to_string());
                    return self.refresh_comments();
                }
                Command::None
            }
            Msg::MergeDone { id } => {
                self.merge_modal = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(format!("PR #{id} をマージしました"));
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
            Msg::BranchesLoaded { repo, branches } => {
                if self.repo_slug().as_deref() == Some(repo.as_str()) {
                    self.clear_loading();
                    self.branches.set_items(branches);
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
        self.client = Some(client.clone());
        self.screen = Screen::Workspaces;
        self.status = Status::Loading("ワークスペースを取得中…".to_string());
        Command::LoadWorkspaces { client }
    }

    /// キー入力の処理。グローバルキー（Ctrl+C / ヘルプ / モーダル）を先に捌く。
    fn on_key(&mut self, key: KeyEvent) -> Command {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::Quit;
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
            KeyCode::Down | KeyCode::Char('j') => {
                self.workspaces.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.workspaces.select_prev();
                Command::None
            }
            KeyCode::Enter => self.enter_workspace(),
            _ => Command::None,
        }
    }

    /// ワークスペース決定時: 既定ワークスペースを保存し、リポジトリ取得を開始。
    fn enter_workspace(&mut self) -> Command {
        let Some(workspace) = self.workspaces.selected() else {
            return Command::None;
        };
        let slug = workspace.slug.clone();

        self.selected_workspace = Some(slug.clone());
        self.config.default_workspace = Some(slug.clone());
        if let Err(error) = self.config.save() {
            tracing::warn!(%error, "既定ワークスペースの保存に失敗しました");
        }

        self.repositories.set_items(Vec::new());
        self.screen = Screen::Repositories;
        self.status = Status::Loading(format!("{slug} のリポジトリを取得中…"));

        match &self.client {
            Some(client) => Command::LoadRepositories {
                client: client.clone(),
                workspace: slug,
            },
            None => {
                self.status = Status::Error("認証クライアントが未初期化です".to_string());
                Command::None
            }
        }
    }

    fn on_key_repositories(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
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
            KeyCode::Enter => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.open_pull_requests()
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
                self.open_branches()
            }
            KeyCode::Char('s') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            _ => Command::None,
        }
    }

    /// 選択リポジトリを確定する（`ws/repo` と既定ブランチを保持）。
    fn select_repo(&mut self, repo: &Repository) {
        self.selected_repo = Some(repo.full_name.clone());
        self.repo_main_branch = repo.main_branch_name().map(str::to_string);
    }

    /// Source ルートに使う既定ブランチ名（未取得なら `main` にフォールバック）。
    fn default_source_ref(&self) -> String {
        self.repo_main_branch
            .clone()
            .unwrap_or_else(|| "main".to_string())
    }

    /// PR 一覧画面へ遷移し、OPEN の一覧取得を開始する。
    fn open_pull_requests(&mut self) -> Command {
        self.screen = Screen::PullRequests;
        self.pr_state_filter = PrStateFilter::Open;
        self.pull_requests.set_items(Vec::new());
        self.current_pr = None;
        self.reload_pull_requests()
    }

    /// 現在のフィルタで PR 一覧を再取得する。
    fn reload_pull_requests(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!(
            "PR 一覧を取得中…（{}）",
            self.pr_state_filter.label()
        ));
        Command::LoadPullRequests {
            client,
            workspace,
            repo,
            filter: self.pr_state_filter,
        }
    }

    fn on_key_pull_requests(&mut self, key: KeyEvent) -> Command {
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
                self.pull_requests.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pull_requests.select_prev();
                Command::None
            }
            KeyCode::Char('o') => self.set_pr_filter(PrStateFilter::Open),
            KeyCode::Char('m') => self.set_pr_filter(PrStateFilter::Merged),
            KeyCode::Char('d') => self.set_pr_filter(PrStateFilter::Declined),
            KeyCode::Char('a') => self.set_pr_filter(PrStateFilter::All),
            KeyCode::Char('r') => self.reload_pull_requests(),
            KeyCode::Char('P') => self.open_pipelines(),
            KeyCode::Char('b') => self.open_branches(),
            KeyCode::Char('s') => {
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            KeyCode::Enter => self.open_pr_detail(),
            _ => Command::None,
        }
    }

    fn set_pr_filter(&mut self, filter: PrStateFilter) -> Command {
        self.pr_state_filter = filter;
        self.pull_requests.set_items(Vec::new());
        self.reload_pull_requests()
    }

    /// 選択中の PR の詳細画面へ遷移し、詳細/diffstat/コメントの取得を開始する。
    fn open_pr_detail(&mut self) -> Command {
        let Some(pr) = self.pull_requests.selected().cloned() else {
            return Command::None;
        };
        let id = pr.id;
        self.current_pr = Some(pr);
        self.diffstat.set_items(Vec::new());
        self.comments = Vec::new();
        self.diff = None;
        self.detail_scroll = 0;
        self.screen = Screen::PullRequestDetail;

        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(format!("PR #{id} を取得中…"));
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
                Command::None
            }
            KeyCode::PageUp => {
                self.detail_scroll = self.detail_scroll.saturating_sub(5);
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
            _ => Command::None,
        }
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
            _ => {}
        }

        let Some(diff) = self.diff.as_mut() else {
            return Command::None;
        };
        let page = diff.viewport.max(1);
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => diff.scroll_down(1),
            KeyCode::Up | KeyCode::Char('k') => diff.scroll_up(1),
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

    /// Branches 一覧画面へ遷移し、取得を開始する。
    fn open_branches(&mut self) -> Command {
        self.screen = Screen::Branches;
        self.branches.set_items(Vec::new());
        self.reload_branches()
    }

    /// ブランチ一覧を取得する。
    fn reload_branches(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading("ブランチ一覧を取得中…".to_string());
        Command::LoadBranches {
            client,
            workspace,
            repo,
        }
    }

    fn on_key_branches(&mut self, key: KeyEvent) -> Command {
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
                self.branches.select_next();
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.branches.select_prev();
                Command::None
            }
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

    /// 親ディレクトリへ戻る（ルートなら Repositories へ）。
    fn source_up(&mut self) -> Command {
        let parent = self.source.as_ref().and_then(|source| {
            parent_dir(&source.path).map(|parent| (source.reference.clone(), parent))
        });
        match parent {
            Some((reference, path)) => self.open_source(reference, path),
            None => {
                self.source = None;
                self.screen = Screen::Repositories;
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
        app.update(Msg::WorkspacesLoaded(vec![
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
        ]));
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
            workspace: "stale".to_string(),
            repos: vec![make_repo("x/y", None)],
        });
        assert!(app.repositories.items.is_empty());
    }

    #[test]
    fn selecting_repository_loads_pull_requests() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/widget", None)],
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
            repo: "widget".to_string(),
            filter: PrStateFilter::Open,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")],
        });
        assert_eq!(app.pull_requests.items.len(), 2);
        assert_eq!(app.pull_requests.state.selected(), Some(0));
    }

    #[test]
    fn pull_requests_loaded_ignored_for_stale_filter() {
        let mut app = review_app();
        app.pr_state_filter = PrStateFilter::Open;
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::Merged,
            prs: vec![make_pr(1, "MERGED")],
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
        // ビューポート未設定でも 1 行スクロールできる。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 1);
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
}

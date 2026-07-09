//! アプリ状態・画面遷移・`update()`。
//!
//! bubbletea の `Model`/`Msg`/`Cmd` に相当する構造。`update()` は状態を更新し、副作用を
//! [`Command`] として返す。実際の非同期実行（API 呼び出しの spawn）は `event` モジュールが行う。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::api::{
    ApiError, BitbucketClient, Comment, DiffStatEntry, MergeParams, MergeStrategy, PullRequest,
    Repository, User, Workspace,
};
use crate::auth;
use crate::config::Config;
use crate::tui::diff::{ParsedDiff, parse as parse_diff};
use crate::tui::onboarding::{Field, OnboardingState};

/// 画面種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Workspaces,
    Repositories,
    PullRequests,
    PullRequestDetail,
    Diff,
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
    pub pull_requests: SelectList<PullRequest>,
    pub pr_state_filter: PrStateFilter,
    pub current_pr: Option<PullRequest>,
    pub diffstat: SelectList<DiffStatEntry>,
    pub comments: Vec<Comment>,
    pub detail_scroll: u16,
    pub diff: Option<DiffState>,
    pub comment_editor: Option<CommentEditor>,
    pub merge_modal: Option<MergeModal>,
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
            onboarding.email = email.clone();
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
            pull_requests: SelectList::default(),
            pr_state_filter: PrStateFilter::Open,
            current_pr: None,
            diffstat: SelectList::default(),
            comments: Vec::new(),
            detail_scroll: 0,
            diff: None,
            comment_editor: None,
            merge_modal: None,
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
                self.status = Status::Error(error.to_string());
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

        match self.screen {
            Screen::Onboarding => self.on_key_onboarding(key),
            Screen::Workspaces => self.on_key_workspaces(key),
            Screen::Repositories => self.on_key_repositories(key),
            Screen::PullRequests => self.on_key_pull_requests(key),
            Screen::PullRequestDetail => self.on_key_pull_request_detail(key),
            Screen::Diff => self.on_key_diff(key),
        }
    }

    fn on_key_onboarding(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.onboarding.error = None;
                Command::None
            }
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Down | KeyCode::Up => {
                self.onboarding.toggle_field();
                Command::None
            }
            KeyCode::Backspace => {
                self.onboarding.backspace();
                Command::None
            }
            KeyCode::Enter => self.submit_onboarding(),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.onboarding.push_char(ch);
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
            email: self.onboarding.email.trim().to_string(),
            token: self.onboarding.token.clone(),
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
                if let Some(repo) = self.repositories.selected() {
                    self.selected_repo = Some(repo.full_name.clone());
                    return self.open_pull_requests();
                }
                Command::None
            }
            _ => Command::None,
        }
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
                self.screen = Screen::PullRequestDetail;
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
        assert_eq!(app.onboarding.email, "a@");
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.onboarding.field.0, Field::Token);
        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert_eq!(app.onboarding.token, "t");
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
        app.onboarding.email = "user@example.com".to_string();
        app.onboarding.token = "secret".to_string();
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
                name: "A".to_string(),
                uuid: None,
            },
            Workspace {
                slug: "b".to_string(),
                name: "B".to_string(),
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
            repos: vec![Repository {
                full_name: "x/y".to_string(),
                name: "y".to_string(),
                updated_on: None,
                is_private: false,
            }],
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
            repos: vec![Repository {
                full_name: "acme/widget".to_string(),
                name: "widget".to_string(),
                updated_on: None,
                is_private: true,
            }],
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
}

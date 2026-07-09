//! アプリ状態・画面遷移・`update()`。
//!
//! bubbletea の `Model`/`Msg`/`Cmd` に相当する構造。`update()` は状態を更新し、副作用を
//! [`Command`] として返す。実際の非同期実行（API 呼び出しの spawn）は `event` モジュールが行う。

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::widgets::ListState;

use crate::api::{ApiError, BitbucketClient, Repository, User, Workspace};
use crate::auth;
use crate::config::Config;
use crate::tui::onboarding::{Field, OnboardingState};

/// 画面種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Onboarding,
    Workspaces,
    Repositories,
    RepoSelected,
}

/// ステータス行の状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Idle,
    Loading(String),
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
}

/// `update()` が返す副作用の指示。実行は `event` モジュールが担う。
#[derive(Debug)]
pub enum Command {
    /// 何もしない。
    None,
    /// アプリ終了。
    Quit,
    /// email+token を検証する（`GET /2.0/user`）。
    ValidateAuth { email: String, token: String },
    /// ワークスペース一覧を取得する。
    LoadWorkspaces { client: BitbucketClient },
    /// 指定ワークスペースのリポジトリ一覧を取得する。
    LoadRepositories {
        client: BitbucketClient,
        workspace: String,
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
    pub onboarding: OnboardingState,
    pub workspaces: SelectList<Workspace>,
    pub repositories: SelectList<Repository>,
    pub selected_workspace: Option<String>,
    pub selected_repo: Option<String>,
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
        Self {
            screen: Screen::Onboarding,
            config,
            client,
            onboarding,
            workspaces: SelectList::default(),
            repositories: SelectList::default(),
            selected_workspace: None,
            selected_repo: None,
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

    /// キー入力の処理。グローバルキー（Ctrl+C / ヘルプ）を先に捌く。
    fn on_key(&mut self, key: KeyEvent) -> Command {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::Quit;
        }

        if self.show_help {
            // ヘルプ表示中は任意のキーで閉じる。
            self.show_help = false;
            return Command::None;
        }

        match self.screen {
            Screen::Onboarding => self.on_key_onboarding(key),
            Screen::Workspaces => self.on_key_workspaces(key),
            Screen::Repositories => self.on_key_repositories(key),
            Screen::RepoSelected => self.on_key_repo_selected(key),
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
                    self.screen = Screen::RepoSelected;
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    fn on_key_repo_selected(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::Repositories;
                Command::None
            }
            _ => Command::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEventKind;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn app() -> App {
        App::new(Config::default(), None)
    }

    #[test]
    fn ctrl_c_quits_from_any_screen() {
        let mut app = app();
        let event = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(app.update(Msg::Key(event)), Command::Quit));
    }

    #[test]
    fn onboarding_typing_and_field_switch() {
        let mut app = app();
        // email へ入力
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        app.update(Msg::Key(key(KeyCode::Char('@'))));
        assert_eq!(app.onboarding.email, "a@");
        // Enter で token フィールドへ
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.onboarding.field.0, Field::Token);
        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert_eq!(app.onboarding.token, "t");
    }

    #[test]
    fn onboarding_submit_requires_both_fields() {
        let mut app = app();
        app.onboarding.field.0 = Field::Token;
        // email 空のまま送信 → エラーになり検証コマンドは出ない
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
        // j で下へ
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.workspaces.state.selected(), Some(1));
        // 末尾で停止
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
    fn selecting_repository_transitions_to_repo_selected() {
        let mut app = app();
        app.selected_workspace = Some("ws".to_string());
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "ws".to_string(),
            repos: vec![Repository {
                full_name: "ws/repo".to_string(),
                name: "repo".to_string(),
                updated_on: None,
                is_private: true,
            }],
        });
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::RepoSelected);
        assert_eq!(app.selected_repo.as_deref(), Some("ws/repo"));
    }

    #[test]
    fn help_toggle_and_dismiss() {
        let mut app = app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(key(KeyCode::Char('?'))));
        assert!(app.show_help);
        // 任意キーで閉じる
        app.update(Msg::Key(KeyEvent {
            code: KeyCode::Char('x'),
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: ratatui::crossterm::event::KeyEventState::NONE,
        }));
        assert!(!app.show_help);
    }
}

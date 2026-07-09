//! イベントループと非同期コマンドの実行。
//!
//! 設計上は「入力イベント」と「API 結果」の 2 系統を `tokio::select!` で待ち受ける。
//! crossterm の入力読み取りはブロッキング API（`event::read`）を専用スレッドで回し、
//! tokio mpsc へ橋渡しする（`event-stream` / `futures` へ依存を増やさないため）。
//! API 呼び出しは `tokio::spawn` し、結果を [`Msg`] として返す。

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use tokio::sync::mpsc;

use crate::api::BitbucketClient;
use crate::tui::Tui;
use crate::tui::app::{App, Command, Msg};
use crate::tui::ui;

/// 入力・API チャネルの容量。
const CHANNEL_CAPACITY: usize = 64;

/// TUI のメインループ。`app` の状態が「終了」に達するまで描画と更新を繰り返す。
pub async fn run(tui: &mut Tui, mut app: App) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(CHANNEL_CAPACITY);
    let (api_tx, mut api_rx) = mpsc::channel::<Msg>(CHANNEL_CAPACITY);

    spawn_input_reader(input_tx);

    // 起動直後のコマンド（認証済みなら workspace 取得を開始）。
    if !dispatch(app.init_command(), &api_tx) {
        return Ok(());
    }

    loop {
        tui.terminal
            .draw(|frame| ui::render(frame, &mut app))
            .map_err(|error| anyhow::anyhow!("画面描画に失敗しました: {error}"))?;

        tokio::select! {
            maybe_event = input_rx.recv() => {
                // 入力スレッドが終了したら（stdin クローズ等）ループも終える。
                let Some(event) = maybe_event else { break };
                if let Event::Key(key) = event
                    && key.kind == KeyEventKind::Press
                    && !dispatch(app.update(Msg::Key(key)), &api_tx)
                {
                    break;
                }
                // それ以外（リサイズ等）は次のループ先頭で再描画される。
            }
            maybe_msg = api_rx.recv() => {
                if let Some(msg) = maybe_msg
                    && !dispatch(app.update(msg), &api_tx)
                {
                    break;
                }
            }
        }
    }

    Ok(())
}

/// [`Command`] を実行する。戻り値は「ループ継続なら true」。
fn dispatch(command: Command, api_tx: &mpsc::Sender<Msg>) -> bool {
    match command {
        Command::None => true,
        Command::Quit => false,
        Command::ValidateAuth { email, token } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match BitbucketClient::new(email.clone(), token.clone()) {
                    Ok(client) => match client.get_current_user().await {
                        Ok(user) => Msg::AuthValidated { email, token, user },
                        Err(error) => Msg::AuthFailed(error),
                    },
                    Err(error) => Msg::AuthFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadWorkspaces { client } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.list_workspaces().await {
                    Ok(workspaces) => Msg::WorkspacesLoaded(workspaces),
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadRepositories { client, workspace } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.list_repositories(&workspace).await {
                    Ok(repos) => Msg::RepositoriesLoaded { workspace, repos },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
    }
}

/// crossterm 入力をブロッキング読み取りし、tokio チャネルへ流す専用スレッド。
///
/// 受信側（`input_rx`）がドロップされると `blocking_send` が失敗し、スレッドは終了する。
fn spawn_input_reader(tx: mpsc::Sender<Event>) {
    std::thread::spawn(move || {
        // `read()` が Err（stdin クローズ等）を返したらループを抜ける。
        while let Ok(event) = event::read() {
            if tx.blocking_send(event).is_err() {
                break;
            }
        }
    });
}

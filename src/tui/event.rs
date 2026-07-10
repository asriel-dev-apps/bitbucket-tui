//! イベントループと非同期コマンドの実行。
//!
//! 設計上は「入力イベント」と「API 結果」の 2 系統を `tokio::select!` で待ち受ける。
//! crossterm の入力読み取りはブロッキング API（`event::read`）を専用スレッドで回し、
//! tokio mpsc へ橋渡しする（`event-stream` / `futures` へ依存を増やさないため）。
//! API 呼び出しは `tokio::spawn` し、結果を [`Msg`] として返す。

use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{self, Event, KeyEventKind};
use tokio::sync::mpsc;

use crate::api::BitbucketClient;
use crate::tui::Tui;
use crate::tui::app::{App, Command, Msg, PipelineAction};
use crate::tui::ui;

/// 入力・API チャネルの容量。
const CHANNEL_CAPACITY: usize = 64;

/// 進行中パイプラインの自動ポーリング間隔。
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// TUI のメインループ。`app` の状態が「終了」に達するまで描画と更新を繰り返す。
pub async fn run(tui: &mut Tui, mut app: App) -> Result<()> {
    let (input_tx, mut input_rx) = mpsc::channel::<Event>(CHANNEL_CAPACITY);
    let (api_tx, mut api_rx) = mpsc::channel::<Msg>(CHANNEL_CAPACITY);

    spawn_input_reader(input_tx);

    // 起動直後のコマンド（認証済みなら workspace 取得を開始）。
    if !dispatch(app.init_command(), &api_tx) {
        return Ok(());
    }

    // 自動ポーリング用のタイマ。tick を `Msg::Tick` として流し、update() 側が進行中
    // パイプラインの有無・自動更新の ON/OFF を見てリフレッシュ要否を判断する。
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 最初の tick は即時に完了するため、起動直後の無駄な発火を避けて捨てる。
    ticker.tick().await;

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
            _ = ticker.tick() => {
                if !dispatch(app.update(Msg::Tick), &api_tx) {
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
        Command::Batch(commands) => {
            let mut keep_running = true;
            for command in commands {
                // いずれかが Quit（false）ならループを終える。
                keep_running &= dispatch(command, api_tx);
            }
            keep_running
        }
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
        Command::LoadWorkspaces { client, page } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_workspaces_page(page).await {
                    Ok(result) => Msg::WorkspacesLoaded {
                        workspaces: result.values,
                        page_info: result.info,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadRepositories {
            client,
            workspace,
            sort,
            page,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_repositories_page(&workspace, sort, page).await {
                    Ok(result) => Msg::RepositoriesLoaded {
                        workspace,
                        sort,
                        repos: result.values,
                        page_info: result.info,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadPullRequests {
            client,
            workspace,
            repo,
            filter,
            sort,
            page,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client
                    .get_pull_requests_page(&workspace, &repo, filter.states(), sort, page)
                    .await
                {
                    Ok(result) => Msg::PullRequestsLoaded {
                        repo,
                        filter,
                        sort,
                        prs: result.values,
                        page_info: result.info,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadPrDetail {
            client,
            workspace,
            repo,
            id,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_pull_request(&workspace, &repo, id).await {
                    Ok(pr) => Msg::PrDetailLoaded {
                        id,
                        pr: Box::new(pr),
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadDiffStat {
            client,
            workspace,
            repo,
            id,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_pr_diffstat(&workspace, &repo, id).await {
                    Ok(entries) => Msg::DiffStatLoaded { id, entries },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadDiff {
            client,
            workspace,
            repo,
            id,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_pr_diff(&workspace, &repo, id).await {
                    Ok(text) => Msg::DiffLoaded { id, text },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadComments {
            client,
            workspace,
            repo,
            id,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.list_comments(&workspace, &repo, id).await {
                    Ok(comments) => Msg::CommentsLoaded { id, comments },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::Approve {
            client,
            workspace,
            repo,
            id,
            approve,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let result = if approve {
                    client.approve(&workspace, &repo, id).await
                } else {
                    client.unapprove(&workspace, &repo, id).await
                };
                let msg = match result {
                    Ok(()) => Msg::ReviewActionDone {
                        id,
                        message: if approve {
                            "承認しました".to_string()
                        } else {
                            "承認を取り消しました".to_string()
                        },
                    },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::RequestChanges {
            client,
            workspace,
            repo,
            id,
            request,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let result = if request {
                    client.request_changes(&workspace, &repo, id).await
                } else {
                    client.unrequest_changes(&workspace, &repo, id).await
                };
                let msg = match result {
                    Ok(()) => Msg::ReviewActionDone {
                        id,
                        message: if request {
                            "変更要求を出しました".to_string()
                        } else {
                            "変更要求を取り消しました".to_string()
                        },
                    },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::CreateComment {
            client,
            workspace,
            repo,
            id,
            raw,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.create_comment(&workspace, &repo, id, &raw).await {
                    Ok(_comment) => Msg::CommentPosted { id },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::Merge {
            client,
            workspace,
            repo,
            id,
            params,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client
                    .merge_pull_request(&workspace, &repo, id, &params)
                    .await
                {
                    Ok(()) => Msg::MergeDone { id },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadPipelines {
            client,
            workspace,
            repo,
            page,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_pipelines_page(&workspace, &repo, page).await {
                    Ok(result) => Msg::PipelinesLoaded {
                        repo,
                        pipelines: result.values,
                        page_info: result.info,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadPipeline {
            client,
            workspace,
            repo,
            uuid,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_pipeline(&workspace, &repo, &uuid).await {
                    Ok(pipeline) => Msg::PipelineLoaded {
                        uuid,
                        pipeline: Box::new(pipeline),
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadPipelineSteps {
            client,
            workspace,
            repo,
            uuid,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.list_pipeline_steps(&workspace, &repo, &uuid).await {
                    Ok(steps) => Msg::PipelineStepsLoaded { uuid, steps },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadStepLog {
            client,
            workspace,
            repo,
            pipeline_uuid,
            step_uuid,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                // 404（ログ未生成）は「ログなし」として扱い、それ以外はエラー表示。
                let msg = match client
                    .get_step_log(&workspace, &repo, &pipeline_uuid, &step_uuid)
                    .await
                {
                    Ok(text) => Msg::StepLogLoaded {
                        step_uuid,
                        text: Some(text),
                    },
                    Err(error) if error.is_not_found() => Msg::StepLogLoaded {
                        step_uuid,
                        text: None,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::StopPipeline {
            client,
            workspace,
            repo,
            uuid,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.stop_pipeline(&workspace, &repo, &uuid).await {
                    Ok(()) => Msg::PipelineActionDone {
                        action: PipelineAction::Stop,
                    },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::TriggerPipeline {
            client,
            workspace,
            repo,
            target,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.trigger_pipeline(&workspace, &repo, &target).await {
                    Ok(_pipeline) => Msg::PipelineActionDone {
                        action: PipelineAction::Rerun,
                    },
                    Err(error) => Msg::ActionFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadBranches {
            client,
            workspace,
            repo,
            page,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_branches_page(&workspace, &repo, page).await {
                    Ok(result) => Msg::BranchesLoaded {
                        repo,
                        branches: result.values,
                        page_info: result.info,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadCommits {
            client,
            workspace,
            repo,
            revision,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client
                    .list_commits(&workspace, &repo, revision.as_deref())
                    .await
                {
                    Ok(commits) => Msg::CommitsLoaded { revision, commits },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadCommitDetail {
            client,
            workspace,
            repo,
            hash,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_commit(&workspace, &repo, &hash).await {
                    Ok(commit) => Msg::CommitDetailLoaded {
                        hash,
                        commit: Box::new(commit),
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadCommitDiff {
            client,
            workspace,
            repo,
            spec,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.get_commit_diff(&workspace, &repo, &spec).await {
                    Ok(text) => Msg::CommitDiffLoaded { spec, text },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadSource {
            client,
            workspace,
            repo,
            reference,
            path,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client.list_src(&workspace, &repo, &reference, &path).await {
                    Ok(entries) => Msg::SourceLoaded {
                        reference,
                        path,
                        entries,
                    },
                    Err(error) => Msg::LoadFailed(error),
                };
                let _ = tx.send(msg).await;
            });
            true
        }
        Command::LoadFile {
            client,
            workspace,
            repo,
            reference,
            path,
        } => {
            let tx = api_tx.clone();
            tokio::spawn(async move {
                let msg = match client
                    .get_src_file(&workspace, &repo, &reference, &path)
                    .await
                {
                    Ok(text) => Msg::FileLoaded { path, text },
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

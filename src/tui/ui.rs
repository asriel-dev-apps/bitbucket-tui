//! 各画面の描画。
//!
//! レイアウトは「ヘッダ / 本文 / ステータス行 / キーヒント行」の 4 段構成。ヘルプ・merge 確認
//! モーダル・コメントエディタはオーバーレイ（ポップアップ）で表示する。TUI 実行中に
//! stdout/stderr へ出さないため、ここでの出力はすべて ratatui のバッファ経由。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::api::{
    Branch, Comment, Commit, DiffStatEntry, MergeStrategy, Pipeline, PipelineStatus, PipelineStep,
    PullRequest, SrcEntry,
};
use crate::tui::app::{
    App, CommentEditor, ConfirmModal, DiffState, MergeModal, Screen, SelectList, Status,
};
use crate::tui::diff::DiffLineKind;
use crate::tui::onboarding::Field;

/// API token 発行に関する常時ヒント。
const TOKEN_HINT: &str = "API token は Atlassian アカウント設定 > Security の「Create API token with scopes」で発行。必要スコープ: read:user:bitbucket, read:workspace:bitbucket, read:repository:bitbucket, read:pullrequest:bitbucket, write:pullrequest:bitbucket, read:pipeline:bitbucket, write:pipeline:bitbucket";

/// 画面全体を描画する。
pub fn render(frame: &mut Frame, app: &mut App) {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .split(frame.area());

    render_header(frame, chunks[0], app.screen);

    match app.screen {
        Screen::Onboarding => render_onboarding(frame, chunks[1], app),
        Screen::Workspaces => render_workspaces(frame, chunks[1], app),
        Screen::Repositories => render_repositories(frame, chunks[1], app),
        Screen::PullRequests => render_pull_requests(frame, chunks[1], app),
        Screen::PullRequestDetail => render_pull_request_detail(frame, chunks[1], app),
        Screen::Diff => render_diff(frame, chunks[1], app),
        Screen::Pipelines => render_pipelines(frame, chunks[1], app),
        Screen::PipelineDetail => render_pipeline_detail(frame, chunks[1], app),
        Screen::StepLog => render_step_log(frame, chunks[1], app),
        Screen::Branches => render_branches(frame, chunks[1], app),
        Screen::Commits => render_commits(frame, chunks[1], app),
        Screen::CommitDetail => render_commit_detail(frame, chunks[1], app),
        Screen::Source => render_source(frame, chunks[1], app),
        Screen::FileView => render_file_view(frame, chunks[1], app),
    }

    render_status(frame, chunks[2], &app.status);
    render_hints(frame, chunks[3], app.screen);

    // オーバーレイ（優先度: コメント/merge/確認モーダル → ヘルプ）。
    if let Some(editor) = &app.comment_editor {
        render_comment_editor(frame, editor);
    }
    if let Some(modal) = &app.merge_modal {
        render_merge_modal(frame, modal, app.current_pr.as_ref());
    }
    if let Some(modal) = &app.confirm_modal {
        render_confirm_modal(frame, modal);
    }
    if app.show_help {
        render_help(frame, app.screen);
    }
}

fn screen_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Onboarding => "認証情報の登録",
        Screen::Workspaces => "ワークスペース",
        Screen::Repositories => "リポジトリ",
        Screen::PullRequests => "プルリクエスト",
        Screen::PullRequestDetail => "PR 詳細",
        Screen::Diff => "差分",
        Screen::Pipelines => "パイプライン",
        Screen::PipelineDetail => "パイプライン詳細",
        Screen::StepLog => "ステップログ",
        Screen::Branches => "ブランチ",
        Screen::Commits => "コミット履歴",
        Screen::CommitDetail => "コミット詳細",
        Screen::Source => "ソース",
        Screen::FileView => "ファイル",
    }
}

fn render_header(frame: &mut Frame, area: Rect, screen: Screen) {
    let line = Line::from(vec![
        Span::styled(
            " bitbucket-tui ",
            Style::new().fg(Color::Black).bg(Color::Cyan).bold(),
        ),
        Span::raw(" "),
        Span::styled(
            screen_title(screen),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

fn render_onboarding(frame: &mut Frame, area: Rect, app: &App) {
    let active = app.onboarding.field.0;

    // email は実文字、token は文字数ぶんの `•` でマスク。カーソルは各フィールドが保持する。
    let email_chars: Vec<char> = app.onboarding.email.chars().to_vec();
    let token_chars: Vec<char> = vec!['•'; app.onboarding.token.len()];

    let email_spans = input_spans(
        &email_chars,
        app.onboarding.email.cursor(),
        active == Field::Email,
        "（メールアドレスを入力）",
    );
    let token_spans = input_spans(
        &token_chars,
        app.onboarding.token.cursor(),
        active == Field::Token,
        "（API token を入力・マスク表示）",
    );

    let mut lines = vec![
        field_line("Email", email_spans, active == Field::Email),
        field_line("Token", token_spans, active == Field::Token),
        Line::raw(""),
    ];

    if app.onboarding.validating {
        lines.push(Line::from(Span::styled(
            "検証中… (GET /2.0/user)",
            Style::new().fg(Color::Yellow),
        )));
    }
    if let Some(error) = &app.onboarding.error {
        lines.push(Line::from(Span::styled(
            format!("エラー: {error}"),
            Style::new().fg(Color::Red).bold(),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(TOKEN_HINT, Style::new().dim())));
    lines.push(Line::from(Span::styled(
        "username = Atlassian アカウントのメール / password = API token でログインします。",
        Style::new().dim(),
    )));

    let block = Block::default().borders(Borders::ALL).title(" ようこそ ");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn field_line<'a>(label: &'a str, value_spans: Vec<Span<'a>>, active: bool) -> Line<'a> {
    let marker = if active { "▶ " } else { "  " };
    let label_style = if active {
        Style::new().fg(Color::Cyan).bold()
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(marker, Style::new().fg(Color::Cyan)),
        Span::styled(format!("{label:<6}: "), label_style),
    ];
    spans.extend(value_spans);
    Line::from(spans)
}

/// 入力フィールドの表示スパンを作る。
///
/// `active` のときはカーソル位置を反転表示する（末尾では反転した空白を1つ置く）。空のときは
/// プレースホルダを淡色で表示する。`chars` は表示用の文字列（token はマスク済みの `•` 列）。
fn input_spans<'a>(
    chars: &[char],
    cursor: usize,
    active: bool,
    placeholder: &'a str,
) -> Vec<Span<'a>> {
    if chars.is_empty() {
        let mut spans = Vec::new();
        if active {
            spans.push(Span::styled(" ", Style::new().reversed()));
        }
        spans.push(Span::styled(placeholder, Style::new().dim()));
        return spans;
    }
    if !active {
        return vec![Span::raw(chars.iter().collect::<String>())];
    }
    let cursor = cursor.min(chars.len());
    let before: String = chars[..cursor].iter().collect();
    if cursor < chars.len() {
        let at = chars[cursor].to_string();
        let after: String = chars[cursor + 1..].iter().collect();
        vec![
            Span::raw(before),
            Span::styled(at, Style::new().reversed()),
            Span::raw(after),
        ]
    } else {
        vec![
            Span::raw(before),
            Span::styled(" ", Style::new().reversed()),
        ]
    }
}

fn render_workspaces(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.workspaces.items.is_empty() {
        render_placeholder(
            frame,
            area,
            &app.status,
            "参加しているワークスペースがありません",
        );
        return;
    }
    let items: Vec<ListItem> = app
        .workspaces
        .items
        .iter()
        .map(|workspace| {
            ListItem::new(Line::from(vec![
                Span::raw(workspace.display_name().to_string()),
                Span::styled(format!("  ({})", workspace.slug), Style::new().dim()),
            ]))
        })
        .collect();
    let title = format!(" ワークスペース ({}) ", app.workspaces.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.workspaces.state);
}

fn render_repositories(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.repositories.items.is_empty() {
        render_placeholder(frame, area, &app.status, "リポジトリがありません");
        return;
    }
    let items: Vec<ListItem> = app
        .repositories
        .items
        .iter()
        .map(|repo| {
            let visibility = if repo.is_private { "private" } else { "public" };
            let visibility_style = if repo.is_private {
                Style::new().fg(Color::Magenta)
            } else {
                Style::new().fg(Color::Green)
            };
            let updated = repo
                .updated_on
                .as_deref()
                .map(|value| value.chars().take(10).collect::<String>())
                .unwrap_or_default();
            ListItem::new(Line::from(vec![
                Span::raw(repo.name.clone()),
                Span::raw("  "),
                Span::styled(format!("[{visibility}]"), visibility_style),
                Span::styled(format!("  {updated}"), Style::new().dim()),
            ]))
        })
        .collect();
    let title = format!(" リポジトリ ({}) ", app.repositories.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.repositories.state);
}

fn render_pull_requests(frame: &mut Frame, area: Rect, app: &mut App) {
    let filter = app.pr_state_filter.label();
    if app.pull_requests.items.is_empty() {
        render_placeholder(
            frame,
            area,
            &app.status,
            &format!("{filter} の PR がありません"),
        );
        return;
    }
    let items: Vec<ListItem> = app
        .pull_requests
        .items
        .iter()
        .map(|pr| ListItem::new(pull_request_row(pr)))
        .collect();
    let title = format!(" PR [{}] ({}) ", filter, app.pull_requests.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.pull_requests.state);
}

fn pull_request_row(pr: &PullRequest) -> Line<'static> {
    let title: String = pr.title_str().chars().take(48).collect();
    let updated = pr
        .updated_on
        .as_deref()
        .map(|value| value.chars().take(10).collect::<String>())
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(format!("#{:<5}", pr.id), Style::new().fg(Color::Yellow)),
        Span::styled(
            format!("{:<9}", pr.state_str()),
            state_style(pr.state_str()),
        ),
        Span::raw(title),
        Span::styled(
            format!("  {}", pr.author_name()),
            Style::new().fg(Color::Blue),
        ),
        Span::styled(format!("  {updated}"), Style::new().dim()),
        Span::styled(
            format!("  ✔{}/{}", pr.approved_count(), pr.reviewer_count()),
            Style::new().fg(Color::Green),
        ),
    ])
}

fn state_style(state: &str) -> Style {
    let color = match state {
        "OPEN" => Color::Green,
        "MERGED" => Color::Magenta,
        "DECLINED" => Color::Red,
        "SUPERSEDED" => Color::DarkGray,
        _ => Color::Gray,
    };
    Style::new().fg(color)
}

fn render_pull_request_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);

    match app.current_pr.as_ref() {
        Some(pr) => render_pr_meta_body(frame, rows[0], pr, app.detail_scroll),
        None => render_placeholder(frame, rows[0], &app.status, "PR を選択してください"),
    }

    let bottom =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[1]);
    render_diffstat_list(frame, bottom[0], &mut app.diffstat);
    render_comments(frame, bottom[1], &app.comments);
}

fn render_pr_meta_body(frame: &mut Frame, area: Rect, pr: &PullRequest, scroll: u16) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("#{} ", pr.id),
                Style::new().fg(Color::Yellow).bold(),
            ),
            Span::styled(
                pr.title_str().to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{:<9}", pr.state_str()),
                state_style(pr.state_str()),
            ),
            Span::styled(
                format!("{} → {}", pr.source_branch(), pr.destination_branch()),
                Style::new().fg(Color::Cyan),
            ),
        ]),
        Line::from(vec![
            Span::styled("author: ", Style::new().dim()),
            Span::raw(pr.author_name().to_string()),
            Span::styled(
                format!(
                    "   ✔ {}/{}   コメント {}   タスク {}",
                    pr.approved_count(),
                    pr.reviewer_count(),
                    pr.comment_count.unwrap_or(0),
                    pr.task_count.unwrap_or(0),
                ),
                Style::new().dim(),
            ),
        ]),
        Line::raw(""),
    ];

    match pr.body() {
        Some(body) => {
            for raw in body.lines() {
                lines.push(Line::raw(raw.to_string()));
            }
        }
        None => lines.push(Line::from(Span::styled("（本文なし）", Style::new().dim()))),
    }

    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 概要 "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_diffstat_list(frame: &mut Frame, area: Rect, diffstat: &mut SelectList<DiffStatEntry>) {
    if diffstat.items.is_empty() {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" 変更ファイル ");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（差分情報なし）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = diffstat
        .items
        .iter()
        .map(|entry| {
            let status = entry.status_str();
            ListItem::new(Line::from(vec![
                Span::styled(format!("{status:<9}"), diffstat_status_style(status)),
                Span::raw(entry.path().to_string()),
                Span::styled(
                    format!(
                        "  +{} -{}",
                        entry.lines_added.unwrap_or(0),
                        entry.lines_removed.unwrap_or(0),
                    ),
                    Style::new().dim(),
                ),
            ]))
        })
        .collect();
    let title = format!(" 変更ファイル ({}) ", diffstat.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut diffstat.state);
}

fn diffstat_status_style(status: &str) -> Style {
    let color = match status {
        "added" => Color::Green,
        "removed" => Color::Red,
        "renamed" => Color::Yellow,
        "modified" => Color::Cyan,
        _ => Color::Gray,
    };
    Style::new().fg(color)
}

fn render_comments(frame: &mut Frame, area: Rect, comments: &[Comment]) {
    let title = format!(" コメント ({}) ", comments.len());
    let block = Block::default().borders(Borders::ALL).title(title);
    if comments.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（コメントなし）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return;
    }
    let mut lines = Vec::new();
    for comment in comments {
        let mut header = vec![Span::styled(
            comment.author_name().to_string(),
            Style::new().fg(Color::Cyan).bold(),
        )];
        if let Some(created) = comment.created_on.as_deref() {
            header.push(Span::styled(
                format!("  {}", created.chars().take(10).collect::<String>()),
                Style::new().dim(),
            ));
        }
        if let Some(anchor) = comment_inline_anchor(comment) {
            header.push(Span::styled(
                format!(" @ {anchor}"),
                Style::new().fg(Color::Yellow),
            ));
        }
        lines.push(Line::from(header));
        for raw in comment.raw().lines() {
            lines.push(Line::raw(format!("  {raw}")));
        }
        lines.push(Line::raw(""));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// inline コメントの表示アンカー（`path:line`）。line は新ファイル行(`to`)を優先。
fn comment_inline_anchor(comment: &Comment) -> Option<String> {
    let inline = comment.inline.as_ref()?;
    let path = inline.path.as_deref()?;
    match inline.to.or(inline.from) {
        Some(line) => Some(format!("{path}:{line}")),
        None => Some(path.to_string()),
    }
}

fn render_diff(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.diff.as_ref().is_none_or(|diff| diff.parsed.is_empty()) {
        render_placeholder(frame, area, &app.status, "差分がありません");
        return;
    }
    let Some(diff) = app.diff.as_mut() else {
        render_placeholder(frame, area, &app.status, "差分がありません");
        return;
    };
    // 枠線ぶんを差し引いたビューポート高さを保持（スクロール上限計算に使う）。
    let viewport = area.height.saturating_sub(2) as usize;
    diff.viewport = viewport;
    let max_scroll = diff.parsed.len().saturating_sub(viewport.max(1));
    if diff.scroll > max_scroll {
        diff.scroll = max_scroll;
    }

    render_diff_body(frame, area, diff);
}

fn render_diff_body(frame: &mut Frame, area: Rect, diff: &DiffState) {
    let lines: Vec<Line> = diff
        .parsed
        .lines
        .iter()
        .map(|line| Line::from(Span::styled(line.text.clone(), diff_line_style(line.kind))))
        .collect();
    let title = format!(" diff {} ({} 行) ", diff.title, diff.parsed.len());
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((diff.scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

fn diff_line_style(kind: DiffLineKind) -> Style {
    let base = Style::new().fg(kind.color());
    match kind {
        DiffLineKind::FileHeader => base.bold(),
        DiffLineKind::Meta => base.add_modifier(Modifier::DIM),
        _ => base,
    }
}

/// パイプライン/ステップ状態に対応する前景色。
fn pipeline_status_color(status: PipelineStatus) -> Color {
    match status {
        // 成功=緑 / 失敗・エラー=赤 / 進行中=黄 / 停止・中止=グレー / 保留=既定色。
        PipelineStatus::Successful => Color::Green,
        PipelineStatus::Failed => Color::Red,
        PipelineStatus::InProgress => Color::Yellow,
        PipelineStatus::Stopped => Color::DarkGray,
        PipelineStatus::Pending => Color::Reset,
        PipelineStatus::Unknown => Color::Gray,
    }
}

/// 状態バッジ（アイコン + `state`/`result` 名）の色付き Span。
fn pipeline_status_span(status: PipelineStatus, label: String) -> Span<'static> {
    Span::styled(
        format!("{} {label}", status.icon()),
        Style::new().fg(pipeline_status_color(status)),
    )
}

fn render_pipelines(frame: &mut Frame, area: Rect, app: &mut App) {
    let auto = if app.auto_refresh { "on" } else { "off" };
    if app.pipelines.items.is_empty() {
        render_placeholder(frame, area, &app.status, "パイプラインがありません");
        return;
    }
    let items: Vec<ListItem> = app
        .pipelines
        .items
        .iter()
        .map(|pipeline| ListItem::new(pipeline_row(pipeline)))
        .collect();
    let title = format!(
        " パイプライン ({}) [auto:{auto}] ",
        app.pipelines.items.len()
    );
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.pipelines.state);
}

fn pipeline_row(pipeline: &Pipeline) -> Line<'static> {
    let status = pipeline.status();
    let state_label = match pipeline.result_name() {
        Some(result) => format!("{}/{result}", pipeline.state_name()),
        None => pipeline.state_name().to_string(),
    };
    let created = pipeline
        .created_on
        .as_deref()
        .map(short_datetime)
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(
            format!("{:<7}", pipeline.build_label()),
            Style::new().fg(Color::Yellow),
        ),
        pipeline_status_span(status, format!("{state_label:<20}")),
        Span::styled(
            format!("  {}", pipeline.target_ref()),
            Style::new().fg(Color::Cyan),
        ),
        Span::styled(
            format!("  {}", pipeline.trigger_name()),
            Style::new().fg(Color::Blue),
        ),
        Span::styled(format!("  {created}"), Style::new().dim()),
        Span::styled(
            format!("  {}", pipeline.duration_label()),
            Style::new().dim(),
        ),
    ])
}

fn render_pipeline_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let rows =
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);

    match app.current_pipeline.as_ref() {
        Some(pipeline) => render_pipeline_meta(frame, rows[0], pipeline, app.auto_refresh),
        None => render_placeholder(
            frame,
            rows[0],
            &app.status,
            "パイプラインを選択してください",
        ),
    }
    render_steps_list(frame, rows[1], &mut app.pipeline_steps);
}

fn render_pipeline_meta(frame: &mut Frame, area: Rect, pipeline: &Pipeline, auto_refresh: bool) {
    let status = pipeline.status();
    let state_label = match pipeline.result_name() {
        Some(result) => format!("{} / {result}", pipeline.state_name()),
        None => pipeline.state_name().to_string(),
    };
    let auto = if auto_refresh { "on" } else { "off" };
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{} ", pipeline.build_label()),
                Style::new().fg(Color::Yellow).bold(),
            ),
            pipeline_status_span(status, state_label),
        ]),
        Line::from(vec![
            Span::styled("target: ", Style::new().dim()),
            Span::styled(
                pipeline.target_ref().to_string(),
                Style::new().fg(Color::Cyan),
            ),
            Span::styled("   trigger: ", Style::new().dim()),
            Span::raw(pipeline.trigger_name().to_string()),
        ]),
        Line::from(vec![
            Span::styled("creator: ", Style::new().dim()),
            Span::raw(pipeline.creator_name().to_string()),
        ]),
        Line::from(vec![
            Span::styled("created: ", Style::new().dim()),
            Span::raw(
                pipeline
                    .created_on
                    .as_deref()
                    .map(short_datetime)
                    .unwrap_or_default(),
            ),
            Span::styled("   completed: ", Style::new().dim()),
            Span::raw(
                pipeline
                    .completed_on
                    .as_deref()
                    .map(short_datetime)
                    .unwrap_or_default(),
            ),
            Span::styled("   所要: ", Style::new().dim()),
            Span::raw(pipeline.duration_label()),
        ]),
        Line::from(Span::styled(
            format!("自動更新: {auto}"),
            Style::new().dim(),
        )),
    ];
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" 概要 "))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_steps_list(frame: &mut Frame, area: Rect, steps: &mut SelectList<PipelineStep>) {
    if steps.items.is_empty() {
        let block = Block::default().borders(Borders::ALL).title(" ステップ ");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（ステップなし）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = steps
        .items
        .iter()
        .map(|step| {
            let status = step.status();
            let state_label = step
                .state
                .as_ref()
                .and_then(|state| state.name.as_deref())
                .unwrap_or("?");
            ListItem::new(Line::from(vec![
                pipeline_status_span(status, format!("{state_label:<12}")),
                Span::raw(step.name_str().to_string()),
                Span::styled(format!("  {}", step.duration_label()), Style::new().dim()),
            ]))
        })
        .collect();
    let title = format!(" ステップ ({}) ", steps.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut steps.state);
}

fn render_step_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(log) = app.step_log.as_mut() else {
        render_placeholder(frame, area, &app.status, "ログを取得しています…");
        return;
    };
    // 枠線ぶんを差し引いたビューポート高さを保持（スクロール上限計算に使う）。
    log.viewport = area.height.saturating_sub(2) as usize;
    log.clamp_scroll();

    let line_style = if log.missing {
        Style::new().dim()
    } else {
        Style::new()
    };
    let lines: Vec<Line> = log
        .lines
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), line_style)))
        .collect();
    let count_label = if log.missing {
        "ログなし".to_string()
    } else {
        format!("{} 行", log.lines.len())
    };
    let title = format!(" ログ {} ({count_label}) ", log.title);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((log.scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

// ---- リポジトリブラウズ（M3） ----

fn render_branches(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.branches.items.is_empty() {
        render_placeholder(frame, area, &app.status, "ブランチがありません");
        return;
    }
    let items: Vec<ListItem> = app
        .branches
        .items
        .iter()
        .map(|branch| ListItem::new(branch_row(branch)))
        .collect();
    let title = format!(" ブランチ ({}) ", app.branches.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.branches.state);
}

fn branch_row(branch: &Branch) -> Line<'static> {
    let name: String = branch.name_str().chars().take(30).collect();
    let summary: String = branch.target_summary().chars().take(50).collect();
    Line::from(vec![
        Span::styled(format!("{name:<30}"), Style::new().fg(Color::Green)),
        Span::styled(
            format!("  {}", branch.target_short_hash()),
            Style::new().fg(Color::Yellow),
        ),
        Span::styled(
            format!("  {}", short_datetime(branch.target_date())),
            Style::new().dim(),
        ),
        Span::raw(format!("  {summary}")),
    ])
}

fn render_commits(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.commits.items.is_empty() {
        render_placeholder(frame, area, &app.status, "コミットがありません");
        return;
    }
    let items: Vec<ListItem> = app
        .commits
        .items
        .iter()
        .map(|commit| ListItem::new(commit_row(commit)))
        .collect();
    let revision = app.commits_revision.as_deref().unwrap_or("既定ブランチ");
    let title = format!(" コミット [{revision}] ({}) ", app.commits.items.len());
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut app.commits.state);
}

fn commit_row(commit: &Commit) -> Line<'static> {
    let author: String = commit.author_name().chars().take(16).collect();
    let summary: String = commit.summary().chars().take(50).collect();
    Line::from(vec![
        Span::styled(
            format!("{:<8}", commit.short_hash()),
            Style::new().fg(Color::Yellow),
        ),
        Span::styled(
            format!("  {}", short_datetime(commit.date_str())),
            Style::new().dim(),
        ),
        Span::styled(format!("  {author:<16}"), Style::new().fg(Color::Blue)),
        Span::raw(format!("  {summary}")),
    ])
}

fn render_commit_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    match app.current_commit.as_ref() {
        Some(commit) => render_commit_meta_body(frame, area, commit, app.commit_scroll),
        None => render_placeholder(frame, area, &app.status, "コミットを選択してください"),
    }
}

fn render_commit_meta_body(frame: &mut Frame, area: Rect, commit: &Commit, scroll: u16) {
    let parents = commit.parent_short_hashes();
    let parents_label = if parents.is_empty() {
        "（なし）".to_string()
    } else {
        parents.join(", ")
    };
    let mut lines = vec![
        Line::from(vec![
            Span::styled("commit ", Style::new().dim()),
            Span::styled(
                commit.hash_str().to_string(),
                Style::new().fg(Color::Yellow).bold(),
            ),
        ]),
        Line::from(vec![
            Span::styled("author: ", Style::new().dim()),
            Span::raw(commit.author_name().to_string()),
            Span::styled("   date: ", Style::new().dim()),
            Span::raw(short_datetime(commit.date_str())),
        ]),
        Line::from(vec![
            Span::styled("parents: ", Style::new().dim()),
            Span::styled(parents_label, Style::new().fg(Color::Cyan)),
        ]),
        Line::raw(""),
    ];
    let message = commit.message_str();
    if message.trim().is_empty() {
        lines.push(Line::from(Span::styled(
            "（メッセージなし）",
            Style::new().dim(),
        )));
    } else {
        for raw in message.lines() {
            lines.push(Line::raw(raw.to_string()));
        }
    }
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" コミット "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_source(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(source) = app.source.as_mut() else {
        render_placeholder(frame, area, &app.status, "ソースがありません");
        return;
    };
    let title = format!(
        " src {} ({}) ",
        source.location(),
        source.entries.items.len()
    );
    if source.entries.items.is_empty() {
        let block = Block::default().borders(Borders::ALL).title(title);
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（空のディレクトリ）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return;
    }
    let items: Vec<ListItem> = source
        .entries
        .items
        .iter()
        .map(|entry| ListItem::new(src_entry_row(entry)))
        .collect();
    let list = list_widget(items, title);
    frame.render_stateful_widget(list, area, &mut source.entries.state);
}

fn src_entry_row(entry: &SrcEntry) -> Line<'static> {
    if entry.is_dir() {
        Line::from(Span::styled(
            format!("{}/", entry.name()),
            Style::new().fg(Color::Cyan).bold(),
        ))
    } else {
        let size = entry.size.map(human_size).unwrap_or_default();
        Line::from(vec![
            Span::raw(entry.name().to_string()),
            Span::styled(format!("  {size}"), Style::new().dim()),
        ])
    }
}

/// バイト数を `1.2KB` / `3.4MB` 形式に整形する。
fn human_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;
    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1}KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes}B")
    }
}

fn render_file_view(frame: &mut Frame, area: Rect, app: &mut App) {
    let Some(view) = app.file_view.as_mut() else {
        render_placeholder(frame, area, &app.status, "ファイルを取得しています…");
        return;
    };
    // 枠線ぶんを差し引いたビューポート高さを保持（スクロール上限計算に使う）。
    view.viewport = area.height.saturating_sub(2) as usize;
    view.clamp_scroll();

    let line_style = if view.missing {
        Style::new().dim()
    } else {
        Style::new()
    };
    let lines: Vec<Line> = view
        .lines
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), line_style)))
        .collect();
    let count_label = if view.missing {
        "バイナリ".to_string()
    } else {
        format!("{} 行", view.lines.len())
    };
    let title = format!(" file {} ({count_label}) ", view.title);
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((view.scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

/// ISO8601 文字列を `YYYY-MM-DD HH:MM` へ短縮する（`T` を空白に）。
fn short_datetime(value: &str) -> String {
    let truncated: String = value.chars().take(16).collect();
    truncated.replacen('T', " ", 1)
}

fn render_confirm_modal(frame: &mut Frame, modal: &ConfirmModal) {
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            modal.action.description(),
            Style::new().fg(Color::Red).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("対象: ", Style::new().dim()),
            Span::styled(modal.build_label.clone(), Style::new().fg(Color::Yellow)),
        ]),
        Line::raw(""),
    ];
    if modal.submitting {
        lines.push(Line::from(Span::styled(
            "実行中…",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Enter: 実行   Esc: 取消",
        Style::new().dim(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(format!(" {} ", modal.action.title()))
        .border_style(Style::new().fg(Color::Red))
        .style(Style::new().bg(Color::Black));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_placeholder(frame: &mut Frame, area: Rect, status: &Status, empty_text: &str) {
    let text = if matches!(status, Status::Loading(_)) {
        "読み込み中…"
    } else {
        empty_text
    };
    let block = Block::default().borders(Borders::ALL);
    let paragraph = Paragraph::new(Line::from(Span::styled(text, Style::new().dim())))
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

fn list_widget<'a>(items: Vec<ListItem<'a>>, title: String) -> List<'a> {
    List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("▶ ")
}

fn render_status(frame: &mut Frame, area: Rect, status: &Status) {
    let line = match status {
        Status::Idle => Line::raw(""),
        Status::Loading(message) => Line::from(Span::styled(
            format!(" ⏳ {message}"),
            Style::new().fg(Color::Yellow),
        )),
        Status::Success(message) => Line::from(Span::styled(
            format!(" ✔ {message}"),
            Style::new().fg(Color::Green).bold(),
        )),
        Status::Error(message) => Line::from(Span::styled(
            format!(" ✖ {message}"),
            Style::new().fg(Color::Red).bold(),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

fn render_hints(frame: &mut Frame, area: Rect, screen: Screen) {
    let hint = match screen {
        Screen::Onboarding => "Tab/↑↓: フィールド切替   Enter: 次へ/検証   Ctrl+C: 終了",
        Screen::Workspaces => "↑↓ / j k: 移動   Enter: 開く   ?: ヘルプ   q: 終了",
        Screen::Repositories => {
            "↑↓/jk: 移動  Enter: PR  p: パイプライン  b: ブランチ  s: ソース  Esc: 戻る  q: 終了"
        }
        Screen::PullRequests => {
            "↑↓/jk: 移動  Enter: 詳細  o/m/d/a: 状態  r: 再読込  P: パイプライン  b/s: ブラウズ  Esc: 戻る"
        }
        Screen::PullRequestDetail => {
            "d: Diff  c: コメント  a: 承認  x: 変更要求  M: マージ  ↑↓: ファイル  Esc: 戻る"
        }
        Screen::Diff => "↑↓/jk PgUp/PgDn g/G: スクロール  n/N: ファイル  Esc: 戻る  q: 終了",
        Screen::Pipelines => {
            "↑↓/jk: 移動  Enter: 詳細  r: 再読込  a: 自動更新  S: 停止  R: 再実行  Esc: 戻る"
        }
        Screen::PipelineDetail => {
            "↑↓/jk: ステップ  Enter: ログ  r: 再読込  a: 自動更新  S: 停止  R: 再実行  Esc: 戻る"
        }
        Screen::StepLog => "↑↓/jk PgUp/PgDn g/G: スクロール  r: 再取得  Esc: 戻る  q: 終了",
        Screen::Branches => {
            "↑↓/jk: 移動  Enter: コミット履歴  s: ソース  r: 再読込  Esc: 戻る  q: 終了"
        }
        Screen::Commits => "↑↓/jk: 移動  Enter: 詳細  r: 再読込  Esc: 戻る  q: 終了",
        Screen::CommitDetail => "d: Diff  ↑↓/jk PgUp/PgDn: スクロール  Esc: 戻る  q: 終了",
        Screen::Source => "↑↓/jk: 移動  Enter: 開く  Backspace/Esc: 親へ  r: 再読込  q: 終了",
        Screen::FileView => "↑↓/jk PgUp/PgDn g/G: スクロール  Esc: 戻る  q: 終了",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().dim()))),
        area,
    );
}

fn render_comment_editor(frame: &mut Frame, editor: &CommentEditor) {
    let area = centered_rect(70, 50, frame.area());
    frame.render_widget(Clear, area);

    let mut lines: Vec<Line> = if editor.text.is_empty() {
        vec![Line::from(Span::styled(
            "（コメントを入力）",
            Style::new().dim(),
        ))]
    } else {
        editor
            .text
            .split('\n')
            .map(|raw| Line::raw(raw.to_string()))
            .collect()
    };
    lines.push(Line::raw(""));
    if editor.submitting {
        lines.push(Line::from(Span::styled(
            "送信中…",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        "改行: Enter    送信: Ctrl+S    取消: Esc",
        Style::new().dim(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" コメントを書く ")
        .style(Style::new().bg(Color::Black));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_merge_modal(frame: &mut Frame, modal: &MergeModal, pr: Option<&PullRequest>) {
    let area = centered_rect(60, 55, frame.area());
    frame.render_widget(Clear, area);

    let title = pr
        .map(|pr| format!(" マージ確認: #{} ", pr.id))
        .unwrap_or_else(|| " マージ確認 ".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            "破壊的操作: この PR をマージします。",
            Style::new().fg(Color::Red).bold(),
        )),
        Line::raw(""),
        Line::from(Span::styled("マージ戦略:", Style::new().bold())),
    ];
    for (index, strategy) in MergeStrategy::ALL.iter().enumerate() {
        let selected = index == modal.strategy % MergeStrategy::ALL.len();
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::new().fg(Color::Cyan).bold()
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::new().fg(Color::Cyan)),
            Span::styled(strategy.label(), style),
        ]));
    }
    lines.push(Line::raw(""));
    let checkbox = if modal.close_source_branch {
        "[x]"
    } else {
        "[ ]"
    };
    lines.push(Line::from(format!(
        "{checkbox} ソースブランチを削除 (Space で切替)"
    )));
    lines.push(Line::raw(""));
    if modal.submitting {
        lines.push(Line::from(Span::styled(
            "マージ中…",
            Style::new().fg(Color::Yellow),
        )));
    }
    lines.push(Line::from(Span::styled(
        "←/→/Tab: 戦略   Space: ブランチ削除   Enter: 実行   Esc: 取消",
        Style::new().dim(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(title)
        .border_style(Style::new().fg(Color::Red))
        .style(Style::new().bg(Color::Black));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(frame: &mut Frame, screen: Screen) {
    let area = centered_rect(64, 70, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "キーバインド（共通）",
            Style::new().fg(Color::Cyan).bold(),
        )),
        Line::raw(""),
        Line::raw("↑ / k, ↓ / j   上下へ移動"),
        Line::raw("Enter          決定 / 開く"),
        Line::raw("Esc            戻る"),
        Line::raw("?              このヘルプ"),
        Line::raw("q              終了"),
        Line::raw("Ctrl+C         強制終了"),
    ];

    let screen_keys: &[&str] = match screen {
        Screen::PullRequests => &[
            "o / m / d / a  状態フィルタ (OPEN/MERGED/DECLINED/ALL)",
            "r              再読込",
            "Enter          PR 詳細を開く",
            "P              パイプライン一覧を開く",
            "b / s          ブランチ一覧 / ソースを開く",
        ],
        Screen::PullRequestDetail => &[
            "d              Diff を開く",
            "c              コメント投稿（Ctrl+S 送信 / Esc 取消）",
            "a              approve / unapprove トグル",
            "x              request-changes / 取消 トグル",
            "M              マージ（確認モーダル）",
            "↑↓             変更ファイル選択  PgUp/PgDn: 本文スクロール",
        ],
        Screen::Diff => &[
            "↑↓ / j k       1 行スクロール",
            "PgUp/PgDn      1 画面スクロール",
            "g / G          先頭 / 末尾",
            "n / N          次 / 前のファイル境界",
        ],
        Screen::Repositories => &[
            "Enter          プルリクエスト一覧を開く",
            "p              パイプライン一覧を開く",
            "b              ブランチ一覧を開く",
            "s              ソース（既定ブランチのルート）を開く",
        ],
        Screen::Pipelines => &[
            "Enter          パイプライン詳細を開く",
            "r              一覧を再読込",
            "a              自動更新の ON/OFF",
            "S              停止（進行中のみ・確認モーダル）",
            "R              再実行（確認モーダル）",
        ],
        Screen::PipelineDetail => &[
            "↑↓ / j k       ステップ選択",
            "Enter          ステップのログを開く",
            "r              詳細を再読込",
            "a              自動更新の ON/OFF",
            "S / R          停止 / 再実行（確認モーダル）",
        ],
        Screen::StepLog => &[
            "↑↓ / j k       1 行スクロール",
            "PgUp/PgDn      1 画面スクロール",
            "g / G          先頭 / 末尾",
            "r              ログ再取得（擬似 tail）",
        ],
        Screen::Branches => &[
            "Enter          そのブランチのコミット履歴",
            "s              そのブランチのソースルート",
            "r              一覧を再読込",
        ],
        Screen::Commits => &[
            "Enter          コミット詳細を開く",
            "r              履歴を再読込",
        ],
        Screen::CommitDetail => &[
            "d              このコミットの Diff を開く",
            "↑↓ / PgUp/PgDn メッセージをスクロール",
        ],
        Screen::Source => &[
            "Enter          ディレクトリを開く / ファイルを表示",
            "Backspace/Esc  親ディレクトリへ（ルートで repo へ戻る）",
            "r              再読込",
        ],
        Screen::FileView => &[
            "↑↓ / j k       1 行スクロール",
            "PgUp/PgDn      1 画面スクロール",
            "g / G          先頭 / 末尾",
        ],
        _ => &[],
    };
    if !screen_keys.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("{} 画面", screen_title(screen)),
            Style::new().fg(Color::Cyan).bold(),
        )));
        lines.push(Line::raw(""));
        for key in screen_keys {
            lines.push(Line::raw((*key).to_string()));
        }
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "任意のキーで閉じる",
        Style::new().dim(),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ヘルプ ")
        .style(Style::new().bg(Color::Black));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// 中央に指定パーセントの矩形を作る（ポップアップ用）。
fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipeline_status_colors_map_to_spec() {
        // 成功=緑 / 失敗・エラー=赤 / 進行中=黄 / 停止・中止=グレー / 保留=既定色。
        assert_eq!(
            pipeline_status_color(PipelineStatus::Successful),
            Color::Green
        );
        assert_eq!(pipeline_status_color(PipelineStatus::Failed), Color::Red);
        assert_eq!(
            pipeline_status_color(PipelineStatus::InProgress),
            Color::Yellow
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::Stopped),
            Color::DarkGray
        );
        assert_eq!(pipeline_status_color(PipelineStatus::Pending), Color::Reset);
    }

    #[test]
    fn short_datetime_formats_iso8601() {
        assert_eq!(short_datetime("2026-07-10T12:34:56Z"), "2026-07-10 12:34");
        assert_eq!(short_datetime("2026-07-10"), "2026-07-10");
    }
}

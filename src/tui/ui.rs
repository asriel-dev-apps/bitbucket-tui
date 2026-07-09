//! 各画面の描画。
//!
//! レイアウトは「ヘッダ / 本文 / ステータス行 / キーヒント行」の 4 段構成。ヘルプは
//! オーバーレイ（ポップアップ）で表示する。TUI 実行中に stdout/stderr へ出さないため、
//! ここでの出力はすべて ratatui のバッファ経由。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};

use crate::tui::app::{App, Screen, Status};
use crate::tui::onboarding::Field;

/// API token 発行に関する常時ヒント。
const TOKEN_HINT: &str = "API token は Atlassian アカウント設定 > Security で発行。必要スコープ: account, repository, pullrequest, pullrequest:write, pipeline";

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
        Screen::RepoSelected => render_repo_selected(frame, chunks[1], app),
    }

    render_status(frame, chunks[2], &app.status);
    render_hints(frame, chunks[3], app.screen);

    if app.show_help {
        render_help(frame);
    }
}

fn screen_title(screen: Screen) -> &'static str {
    match screen {
        Screen::Onboarding => "認証情報の登録",
        Screen::Workspaces => "ワークスペース",
        Screen::Repositories => "リポジトリ",
        Screen::RepoSelected => "選択済みリポジトリ",
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
    let masked_token: String = "•".repeat(app.onboarding.token.chars().count());

    let email_value = if app.onboarding.email.is_empty() {
        Span::styled("（メールアドレスを入力）", Style::new().dim())
    } else {
        Span::raw(app.onboarding.email.clone())
    };
    let token_value = if masked_token.is_empty() {
        Span::styled("（API token を入力・マスク表示）", Style::new().dim())
    } else {
        Span::raw(masked_token)
    };

    let mut lines = vec![
        field_line("Email", email_value, active == Field::Email),
        field_line("Token", token_value, active == Field::Token),
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

fn field_line<'a>(label: &'a str, value: Span<'a>, active: bool) -> Line<'a> {
    let marker = if active { "▶ " } else { "  " };
    let label_style = if active {
        Style::new().fg(Color::Cyan).bold()
    } else {
        Style::new()
    };
    Line::from(vec![
        Span::styled(marker, Style::new().fg(Color::Cyan)),
        Span::styled(format!("{label:<6}: "), label_style),
        value,
    ])
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
                Span::raw(workspace.name.clone()),
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

fn render_repo_selected(frame: &mut Frame, area: Rect, app: &App) {
    let full_name = app.selected_repo.as_deref().unwrap_or("(未選択)");
    let lines = vec![
        Line::raw(""),
        Line::from(Span::styled(
            full_name.to_string(),
            Style::new().fg(Color::Cyan).bold(),
        )),
        Line::raw(""),
        Line::from(Span::styled(
            "M1: PR一覧をここに実装",
            Style::new().add_modifier(Modifier::DIM),
        )),
    ];
    let block = Block::default().borders(Borders::ALL).title(" 選択済み ");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
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
        Screen::Repositories => "↑↓ / j k: 移動   Enter: 選択   Esc: 戻る   ?: ヘルプ   q: 終了",
        Screen::RepoSelected => "Esc: 戻る   ?: ヘルプ   q: 終了",
    };
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hint, Style::new().dim()))),
        area,
    );
}

fn render_help(frame: &mut Frame) {
    let area = centered_rect(60, 60, frame.area());
    frame.render_widget(Clear, area);

    let lines = vec![
        Line::from(Span::styled(
            "キーバインド",
            Style::new().fg(Color::Cyan).bold(),
        )),
        Line::raw(""),
        Line::raw("↑ / k          上へ移動"),
        Line::raw("↓ / j          下へ移動"),
        Line::raw("Enter          決定 / 開く"),
        Line::raw("Esc            戻る"),
        Line::raw("Tab            (認証画面) フィールド切替"),
        Line::raw("?              このヘルプ"),
        Line::raw("q              終了"),
        Line::raw("Ctrl+C         強制終了"),
        Line::raw(""),
        Line::from(Span::styled("任意のキーで閉じる", Style::new().dim())),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" ヘルプ ")
        .style(Style::new().bg(Color::Black));
    frame.render_widget(Paragraph::new(lines).block(block), area);
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

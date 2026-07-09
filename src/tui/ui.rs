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

use crate::api::{Comment, DiffStatEntry, MergeStrategy, PullRequest};
use crate::tui::app::{App, CommentEditor, DiffState, MergeModal, Screen, SelectList, Status};
use crate::tui::diff::DiffLineKind;
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
        Screen::PullRequests => render_pull_requests(frame, chunks[1], app),
        Screen::PullRequestDetail => render_pull_request_detail(frame, chunks[1], app),
        Screen::Diff => render_diff(frame, chunks[1], app),
    }

    render_status(frame, chunks[2], &app.status);
    render_hints(frame, chunks[3], app.screen);

    // オーバーレイ（優先度: コメント/merge モーダル → ヘルプ）。
    if let Some(editor) = &app.comment_editor {
        render_comment_editor(frame, editor);
    }
    if let Some(modal) = &app.merge_modal {
        render_merge_modal(frame, modal, app.current_pr.as_ref());
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
        Screen::Repositories => "↑↓ / j k: 移動   Enter: 選択   Esc: 戻る   ?: ヘルプ   q: 終了",
        Screen::PullRequests => {
            "↑↓/jk: 移動  Enter: 詳細  o/m/d/a: 状態  r: 再読込  Esc: 戻る  ?: ヘルプ"
        }
        Screen::PullRequestDetail => {
            "d: Diff  c: コメント  a: 承認  x: 変更要求  M: マージ  ↑↓: ファイル  Esc: 戻る"
        }
        Screen::Diff => "↑↓/jk PgUp/PgDn g/G: スクロール  n/N: ファイル  Esc: 戻る  q: 終了",
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

//! 各画面の描画。
//!
//! レイアウトは「ヘッダ / 本文 / ステータス行 / キーヒント行」の 4 段構成。ヘルプ・merge 確認
//! モーダル・コメントエディタはオーバーレイ（ポップアップ）で表示する。TUI 実行中に
//! stdout/stderr へ出さないため、ここでの出力はすべて ratatui のバッファ経由。
//!
//! 色は一切ハードコードしない。すべて [`Theme`]（意味役割ベースの配色）経由で決める
//! （`&App`/`&mut App` を受け取る関数は `app.theme` を、純粋関数は `theme: &Theme` 引数を使う）。

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap,
};

use crate::api::{
    Branch, Comment, Commit, DiffStatEntry, MergeStrategy, PageInfo, Pipeline, PipelineStatus,
    PipelineStep, PullRequest, SrcEntry,
};
use crate::tui::app::{
    App, CommentEditor, ConfirmModal, DiffFocus, DiffState, JumpPaletteState, MergeModal, Screen,
    SelectList, Status,
};
use crate::tui::diff::DiffLineKind;
use crate::tui::onboarding::Field;
use crate::tui::theme::Theme;

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

    render_header(frame, chunks[0], app);

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

    render_status(frame, chunks[2], &app.status, &app.theme);
    render_hints(frame, chunks[3], app.screen, &app.theme);

    // オーバーレイ（優先度: コメント/merge/確認モーダル → ヘルプ）。
    if let Some(editor) = &app.comment_editor {
        render_comment_editor(frame, editor, &app.theme);
    }
    if let Some(modal) = &app.merge_modal {
        render_merge_modal(frame, modal, app.current_pr.as_ref(), &app.theme);
    }
    if let Some(modal) = &app.confirm_modal {
        render_confirm_modal(frame, modal, &app.theme);
    }
    if app.show_help {
        render_help(frame, app.screen, &app.theme);
    }
    // ジャンプパレットは最前面（他のどのオーバーレイより優先して開ける想定のため）。
    let theme = app.theme;
    if let Some(palette) = app.jump_palette.as_mut() {
        render_jump_palette(frame, palette, &theme);
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

/// 角丸枠 + 左右パディングを持つ Block を作る（テーマの `title_style` 込み）。
///
/// `border_color` は呼び出し側が意味役割（`theme.border` / `theme.border_focus` /
/// `theme.danger` 等）から選ぶ。タイトルは常に `theme.accent` で着色する。
fn rounded_block<'a>(theme: &Theme, border_color: Color) -> Block<'a> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border_color))
        .title_style(Style::new().fg(theme.accent))
        .padding(Padding::horizontal(1))
}

/// 通常ペイン用の Block。複数ペイン画面ではキー操作が向く側（インタラクティブな一覧）を
/// `focused = true`、付随する静的な表示ペイン（本文・コメント等）を `false` にする。
/// 単一ペイン画面（一覧のみ・スクロール本文のみ 等）は常に `true` でよい。
fn themed_block<'a>(theme: &Theme, focused: bool) -> Block<'a> {
    rounded_block(
        theme,
        if focused {
            theme.border_focus
        } else {
            theme.border
        },
    )
}

fn render_header(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
    let mut spans = vec![
        Span::styled(
            " bitbucket-tui ",
            Style::new().fg(theme.bg).bg(theme.accent).bold(),
        ),
        Span::raw(" "),
        Span::styled(
            screen_title(app.screen),
            Style::new().add_modifier(Modifier::BOLD),
        ),
    ];
    // インクリメンタル検索中、またはフィルタが残っている間はヘッダに検索文字列を表示する
    // （`/` で開始・編集中はカーソルを付ける・Enter 確定後もフィルタ自体は表示し続ける）。
    if let Some(filter) = current_screen_filter(app)
        && (app.search_editing || !filter.is_empty())
    {
        let cursor = if app.search_editing { "▏" } else { "" };
        spans.push(Span::raw("   "));
        spans.push(Span::styled(
            format!("検索: {filter}{cursor}"),
            Style::new().fg(theme.warning),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// 現在の画面がインクリメンタル検索に対応するリストを持つ場合、そのフィルタ文字列。
fn current_screen_filter(app: &App) -> Option<&str> {
    match app.screen {
        Screen::Workspaces => Some(app.workspaces.filter.as_str()),
        Screen::Repositories => Some(app.repositories.filter.as_str()),
        Screen::PullRequests => Some(app.pull_requests.filter.as_str()),
        _ => None,
    }
}

fn render_onboarding(frame: &mut Frame, area: Rect, app: &App) {
    let theme = &app.theme;
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
        field_line("Email", email_spans, active == Field::Email, theme),
        field_line("Token", token_spans, active == Field::Token, theme),
        Line::raw(""),
    ];

    if app.onboarding.validating {
        lines.push(Line::from(Span::styled(
            "検証中… (GET /2.0/user)",
            Style::new().fg(theme.warning),
        )));
    }
    if let Some(error) = &app.onboarding.error {
        lines.push(Line::from(Span::styled(
            format!("エラー: {error}"),
            Style::new().fg(theme.danger).bold(),
        )));
    }

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(TOKEN_HINT, Style::new().dim())));
    lines.push(Line::from(Span::styled(
        "username = Atlassian アカウントのメール / password = API token でログインします。",
        Style::new().dim(),
    )));

    let block = themed_block(theme, true).title(" ようこそ ");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn field_line<'a>(
    label: &'a str,
    value_spans: Vec<Span<'a>>,
    active: bool,
    theme: &Theme,
) -> Line<'a> {
    let marker = if active { "▶ " } else { "  " };
    let label_style = if active {
        Style::new().fg(theme.accent).bold()
    } else {
        Style::new()
    };
    let mut spans = vec![
        Span::styled(marker, Style::new().fg(theme.accent)),
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

/// フィルタ適用中の一覧が空になったときの案内文（データ自体は無いのか、フィルタで
/// 絞られただけなのかを区別する）。
fn list_empty_text<T>(list: &SelectList<T>, no_data_text: &str) -> String {
    if list.items.is_empty() {
        no_data_text.to_string()
    } else {
        "検索条件に一致するものがありません".to_string()
    }
}

/// `(N)` または（フィルタ適用中は）`(絞込件数/全件数)` の件数表示。
fn count_label<T>(list: &SelectList<T>) -> String {
    if list.filter.is_empty() {
        format!("{}", list.items.len())
    } else {
        format!("{}/{}", list.matches.len(), list.items.len())
    }
}

/// 一覧本体とページャ行（下端 1 行）に分割する。4 画面（Workspaces/Repositories/
/// PullRequests/Branches）共通のレイアウト。
fn split_list_and_pager(area: Rect) -> (Rect, Rect) {
    let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
    (rows[0], rows[1])
}

fn render_workspaces(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.workspaces.matches.is_empty() {
        let text = list_empty_text(&app.workspaces, "参加しているワークスペースがありません");
        render_placeholder(frame, list_area, &app.status, &text, &theme);
    } else {
        let items: Vec<ListItem> = app
            .workspaces
            .visible()
            .map(|workspace| {
                ListItem::new(Line::from(vec![
                    Span::raw(workspace.display_name().to_string()),
                    Span::styled(format!("  ({})", workspace.slug), Style::new().dim()),
                ]))
            })
            .collect();
        let title = format!(" ワークスペース ({}) ", count_label(&app.workspaces));
        let list = list_widget(&theme, items, title);
        frame.render_stateful_widget(list, list_area, &mut app.workspaces.state);
    }

    render_pager(frame, pager_area, app.workspaces_page_info, &theme);
}

fn render_repositories(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.repositories.matches.is_empty() {
        let text = list_empty_text(&app.repositories, "リポジトリがありません");
        render_placeholder(frame, list_area, &app.status, &text, &theme);
    } else {
        let items: Vec<ListItem> = app
            .repositories
            .visible()
            .map(|repo| {
                let visibility = if repo.is_private { "private" } else { "public" };
                let visibility_style = if repo.is_private {
                    Style::new().fg(theme.accent)
                } else {
                    Style::new().fg(theme.success)
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
        let title = format!(
            " リポジトリ ({}) [{}] ",
            count_label(&app.repositories),
            app.repositories_sort.label()
        );
        let list = list_widget(&theme, items, title);
        frame.render_stateful_widget(list, list_area, &mut app.repositories.state);
    }

    render_pager(frame, pager_area, app.repositories_page_info, &theme);
}

fn render_pull_requests(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let filter = app.pr_state_filter.label();
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.pull_requests.matches.is_empty() {
        let no_data_text = format!("{filter} の PR がありません");
        let text = list_empty_text(&app.pull_requests, &no_data_text);
        render_placeholder(frame, list_area, &app.status, &text, &theme);
    } else {
        let items: Vec<ListItem> = app
            .pull_requests
            .visible()
            .map(|pr| ListItem::new(pull_request_row(pr, &theme)))
            .collect();
        let title = format!(
            " PR [{}] ({}) [{}] ",
            filter,
            count_label(&app.pull_requests),
            app.pull_requests_sort.label()
        );
        let list = list_widget(&theme, items, title);
        frame.render_stateful_widget(list, list_area, &mut app.pull_requests.state);
    }

    render_pager(frame, pager_area, app.pull_requests_page_info, &theme);
}

/// ページャ行を描画する（`‹ 1 2 [3] 4 … N ›` 形式）。[`pager_line`] を参照。
fn render_pager(frame: &mut Frame, area: Rect, info: PageInfo, theme: &Theme) {
    let paragraph = Paragraph::new(pager_line(info, theme)).alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// ページャに表示するページ番号を、`None`（省略記号 `…`）を挟みつつ列挙する。
///
/// 先頭・末尾・現在ページの前後 1 件だけを候補にし、間が空けば `None` を 1 つ挿む。
/// `total`（総ページ数）ぶん全件を走査しないため、総ページ数が大きくても O(1) で済む。
/// `current` が `[1, total]` の範囲外でも（`total.max(1)` へクランプして）破綻しない。
fn pager_page_labels(current: u32, total: u32) -> Vec<Option<u32>> {
    let total = total.max(1);
    let current = current.clamp(1, total);

    let mut candidates = vec![
        1,
        total,
        current.saturating_sub(1).max(1),
        current,
        (current + 1).min(total),
    ];
    candidates.retain(|&page| (1..=total).contains(&page));
    candidates.sort_unstable();
    candidates.dedup();

    let mut labels = Vec::with_capacity(candidates.len() * 2);
    let mut prev: Option<u32> = None;
    for page in candidates {
        if let Some(prev_page) = prev
            && page - prev_page > 1
        {
            labels.push(None);
        }
        labels.push(Some(page));
        prev = Some(page);
    }
    labels
}

/// ページャの 1 行を組み立てる（`‹ 1 2 [3] 4 … N ›` 形式）。
///
/// `total_pages` が判明していれば省略記号込みのページ番号列（[`pager_page_labels`]、現在ページ
/// 前後 1 件 + 先頭/末尾、間は `…` で省略）を、不明なら `page N` のみを表示する（総ページ数
/// 不明時のフォールバック）。現在ページは `theme.accent` の太字で強調する。矢印 `‹`/`›` は
/// `page > 1` / `has_next` で活性・非活性を切り替える（非活性は `theme.muted` で淡色）。
/// 1 ページのみ・0 ページ（空一覧）でも破綻しない。
fn pager_line(info: PageInfo, theme: &Theme) -> Line<'static> {
    let prev_style = if info.page > 1 {
        Style::new().fg(theme.fg)
    } else {
        Style::new().fg(theme.muted)
    };
    let next_style = if info.has_next {
        Style::new().fg(theme.fg)
    } else {
        Style::new().fg(theme.muted)
    };

    let mut spans = vec![Span::styled("‹", prev_style), Span::raw(" ")];

    match info.total_pages {
        Some(total) => {
            for label in pager_page_labels(info.page, total) {
                match label {
                    None => spans.push(Span::styled("… ", Style::new().fg(theme.muted))),
                    Some(page) if page == info.page => spans.push(Span::styled(
                        format!("{page} "),
                        Style::new().fg(theme.accent).bold(),
                    )),
                    Some(page) => spans.push(Span::raw(format!("{page} "))),
                }
            }
        }
        None => {
            spans.push(Span::styled(
                format!("page {} ", info.page),
                Style::new().fg(theme.accent).bold(),
            ));
        }
    }

    spans.push(Span::styled("›", next_style));
    Line::from(spans)
}

fn pull_request_row(pr: &PullRequest, theme: &Theme) -> Line<'static> {
    let title: String = pr.title_str().chars().take(48).collect();
    let updated = pr
        .updated_on
        .as_deref()
        .map(|value| value.chars().take(10).collect::<String>())
        .unwrap_or_default();
    Line::from(vec![
        Span::styled(format!("#{:<5}", pr.id), Style::new().fg(theme.warning)),
        Span::styled(
            format!("{:<9}", pr.state_str()),
            state_style(pr.state_str(), theme),
        ),
        Span::raw(title),
        Span::styled(
            format!("  {}", pr.author_name()),
            Style::new().fg(theme.info),
        ),
        Span::styled(format!("  {updated}"), Style::new().dim()),
        Span::styled(
            format!("  ✔{}/{}", pr.approved_count(), pr.reviewer_count()),
            Style::new().fg(theme.success),
        ),
    ])
}

/// PR の `state` に対応する前景色。OPEN=成功 / MERGED=強調 / DECLINED=危険 /
/// SUPERSEDED・不明=補助色。
fn state_style(state: &str, theme: &Theme) -> Style {
    let color = match state {
        "OPEN" => theme.success,
        "MERGED" => theme.accent,
        "DECLINED" => theme.danger,
        "SUPERSEDED" => theme.muted,
        _ => theme.muted,
    };
    Style::new().fg(color)
}

fn render_pull_request_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let rows =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);

    // 枠線ぶんを差し引いたビューポート高さを保持し、リサイズ等で末尾を超えていれば
    // 再クランプする（`DiffState`/`LogView` と同じパターン）。
    app.detail_viewport = rows[0].height.saturating_sub(2) as usize;
    app.clamp_detail_scroll();

    match app.current_pr.as_ref() {
        Some(pr) => render_pr_meta_body(frame, rows[0], pr, app.detail_scroll, &theme),
        None => render_placeholder(frame, rows[0], &app.status, "PR を選択してください", &theme),
    }

    let bottom =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[1]);
    render_diffstat_list(frame, bottom[0], &mut app.diffstat, &theme);
    render_comments(frame, bottom[1], &app.comments, &theme);
}

fn render_pr_meta_body(
    frame: &mut Frame,
    area: Rect,
    pr: &PullRequest,
    scroll: u16,
    theme: &Theme,
) {
    let mut lines = vec![
        Line::from(vec![
            Span::styled(
                format!("#{} ", pr.id),
                Style::new().fg(theme.warning).bold(),
            ),
            Span::styled(
                pr.title_str().to_string(),
                Style::new().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(
                format!("{:<9}", pr.state_str()),
                state_style(pr.state_str(), theme),
            ),
            Span::styled(
                format!("{} → {}", pr.source_branch(), pr.destination_branch()),
                Style::new().fg(theme.info),
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
    ];

    // 承認/変更要求パネル（`app::participant_panel_line_count` と行数を対応させること。
    // どちらも無ければ 0 行のまま追加しない）。
    lines.extend(participant_panel_lines(pr, theme));
    lines.push(Line::raw(""));

    match pr.body() {
        Some(body) => lines.extend(render_markdown_lines(body, theme)),
        None => lines.push(Line::from(Span::styled("（本文なし）", Style::new().dim()))),
    }

    // 複数ペイン画面: 本文は静的な表示ペインなので非フォーカス（下の変更ファイル一覧が
    // インタラクティブな主ペイン）。
    let paragraph = Paragraph::new(lines)
        .block(themed_block(theme, false).title(" 概要 "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

/// 承認/変更要求パネル（承認者・変更要求者の表示名を 1 行ずつ）。
///
/// どちらも参加者がいなければ空（`app::participant_panel_line_count` の行数計算と
/// 一致させること）。
fn participant_panel_lines(pr: &PullRequest, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let approved = pr.approved_names();
    if !approved.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("承認: ", Style::new().fg(theme.success)),
            Span::raw(approved.join(", ")),
        ]));
    }
    let changes_requested = pr.changes_requested_names();
    if !changes_requested.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("変更要求: ", Style::new().fg(theme.danger)),
            Span::raw(changes_requested.join(", ")),
        ]));
    }
    lines
}

/// PR 本文の簡易 Markdown 整形（フルパーサ不要・行頭記号ベース）。
///
/// - 見出し（`#`〜`######` + 半角スペース）: `theme.accent` 太字。
/// - 箇条書き（`-`/`*` + 半角スペース、インデント可）: 記号のみ `theme.accent`。
/// - コードフェンス（\`\`\`）で囲まれた行・インライン `` `code` ``: `theme.muted`。
/// - 画像記法 `![alt](url)`: TUI では表示できないため `[画像: alt]（o でブラウザ表示）`
///   という代替テキストに置換する（画像本体は非対応）。
///
/// 1 入力行 = 1 出力行を維持する（`App::detail_body_line_count` の行数計算と対応させるため、
/// 行の増減を伴う変換はしない）。
fn render_markdown_lines(body: &str, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut in_code_block = false;
    for raw in body.lines() {
        let trimmed_start = raw.trim_start();
        if trimmed_start.starts_with("```") {
            in_code_block = !in_code_block;
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::new().fg(theme.muted),
            )));
            continue;
        }
        if in_code_block {
            lines.push(Line::from(Span::styled(
                raw.to_string(),
                Style::new().fg(theme.muted),
            )));
            continue;
        }

        let replaced = replace_image_syntax(raw);

        if is_heading_line(&replaced) {
            lines.push(Line::from(Span::styled(
                replaced,
                Style::new().fg(theme.accent).bold(),
            )));
            continue;
        }

        if let Some((prefix, rest)) = split_bullet_prefix(&replaced) {
            let mut spans = vec![Span::styled(prefix, Style::new().fg(theme.accent))];
            spans.extend(inline_code_spans(&rest, theme));
            lines.push(Line::from(spans));
            continue;
        }

        lines.push(Line::from(inline_code_spans(&replaced, theme)));
    }
    lines
}

/// 見出し行か（行頭の空白を除き `#`〜`######` の後に半角スペースまたは行末が続く）。
fn is_heading_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if hashes == 0 || hashes > 6 {
        return false;
    }
    matches!(trimmed.as_bytes().get(hashes), None | Some(b' '))
}

/// 箇条書き行を `(先頭の空白+記号+空白, 残りの本文)` に分解する（`-`/`*` のみ対応）。
fn split_bullet_prefix(line: &str) -> Option<(String, String)> {
    let indent_len = line.len() - line.trim_start().len();
    let indent = &line[..indent_len];
    let rest = &line[indent_len..];
    if let Some(after) = rest.strip_prefix("- ") {
        Some((format!("{indent}- "), after.to_string()))
    } else {
        rest.strip_prefix("* ")
            .map(|after| (format!("{indent}* "), after.to_string()))
    }
}

/// インライン `` `code` `` を分割して色分けする（バッククォートの対応が崩れていても
/// パニックせず、単純な交互トグルとしてフェイルソフトに扱う）。
fn inline_code_spans(line: &str, theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut in_code = false;
    for (index, part) in line.split('`').enumerate() {
        if index > 0 {
            in_code = !in_code;
        }
        if part.is_empty() {
            continue;
        }
        if in_code {
            spans.push(Span::styled(part.to_string(), Style::new().fg(theme.muted)));
        } else {
            spans.push(Span::raw(part.to_string()));
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(String::new()));
    }
    spans
}

/// 画像記法 `![alt](url)` を TUI 向けの代替テキストへ置換する。
///
/// TUI では画像を表示できないため、`[画像: alt]（o でブラウザ表示）` という代替テキストに
/// 差し替える（画像本体の表示は非対応）。厳密な Markdown 解釈は行わず、`![` `]` `(` `)` の
/// 並びのみを見る簡易版（記法が崩れている場合は元のテキストをそのまま残す）。
fn replace_image_syntax(line: &str) -> String {
    let mut result = String::new();
    let mut rest = line;
    while let Some(start) = rest.find("![") {
        result.push_str(&rest[..start]);
        let after_bang = &rest[start + 2..];
        let Some(close_bracket) = after_bang.find(']') else {
            // 閉じ `]` が無ければ画像記法とみなさず、残りをそのまま出力して終了。
            result.push_str(&rest[start..]);
            return result;
        };
        let alt = &after_bang[..close_bracket];
        let after_alt = &after_bang[close_bracket + 1..];
        match after_alt
            .strip_prefix('(')
            .and_then(|paren_rest| paren_rest.find(')').map(|end| (paren_rest, end)))
        {
            Some((paren_rest, close_paren)) => {
                result.push_str(&format!("[画像: {alt}]（o でブラウザ表示）"));
                rest = &paren_rest[close_paren + 1..];
            }
            None => {
                // `(url)` が続かない場合は画像記法とみなさず、`![` をそのまま出力して続行。
                result.push_str("![");
                rest = &rest[start + 2..];
            }
        }
    }
    result.push_str(rest);
    result
}

fn render_diffstat_list(
    frame: &mut Frame,
    area: Rect,
    diffstat: &mut SelectList<DiffStatEntry>,
    theme: &Theme,
) {
    if diffstat.items.is_empty() {
        let block = themed_block(theme, true).title(" 変更ファイル ");
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
                Span::styled(format!("{status:<9}"), diffstat_status_style(status, theme)),
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
    let list = list_widget(theme, items, title);
    frame.render_stateful_widget(list, area, &mut diffstat.state);
}

/// diffstat の `status` に対応する前景色。
fn diffstat_status_style(status: &str, theme: &Theme) -> Style {
    let color = match status {
        "added" => theme.success,
        "removed" => theme.danger,
        "renamed" => theme.warning,
        "modified" => theme.info,
        _ => theme.muted,
    };
    Style::new().fg(color)
}

fn render_comments(frame: &mut Frame, area: Rect, comments: &[Comment], theme: &Theme) {
    let title = format!(" コメント ({}) ", comments.len());
    // 静的な表示ペイン（スクロール等の操作を持たない）なので非フォーカス。
    let block = themed_block(theme, false).title(title);
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
        // 返信（`parent` あり）は 1 段インデントして表示する（スレッドの厳密な木構造化は
        // せず、一覧の並び順のまま返信であることが分かる程度の簡易表示）。
        let is_reply = comment.parent.is_some();
        let indent = if is_reply { "  " } else { "" };
        let mut header = vec![Span::raw(indent)];
        if is_reply {
            header.push(Span::styled("↳ ", Style::new().fg(theme.muted)));
        }
        header.push(Span::styled(
            comment.author_name().to_string(),
            Style::new().fg(theme.accent).bold(),
        ));
        if let Some(created) = comment.created_on.as_deref() {
            header.push(Span::styled(
                format!("  {}", created.chars().take(10).collect::<String>()),
                Style::new().dim(),
            ));
        }
        if let Some(anchor) = comment_inline_anchor(comment) {
            header.push(Span::styled(
                format!(" @ {anchor}"),
                Style::new().fg(theme.warning),
            ));
        }
        lines.push(Line::from(header));
        for raw in comment.raw().lines() {
            lines.push(Line::raw(format!("  {indent}{raw}")));
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

/// サイドバー（ファイル一覧）の幅比率。右の本文が主ペインなので控えめに 30%。
const DIFF_SIDEBAR_PERCENT: u16 = 30;

fn render_diff(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    if app.diff.as_ref().is_none_or(|diff| diff.parsed.is_empty()) {
        render_placeholder(frame, area, &app.status, "差分がありません", &theme);
        return;
    }
    let Some(diff) = app.diff.as_mut() else {
        render_placeholder(frame, area, &app.status, "差分がありません", &theme);
        return;
    };

    // 左: ファイル一覧サイドバー / 右: 差分本文（Phase1 のキャッシュ＋viewport スライス描画を
    // 維持するため、本文の再構築ロジックは `render_diff_body` に閉じたまま変更しない）。
    let cols = Layout::horizontal([
        Constraint::Percentage(DIFF_SIDEBAR_PERCENT),
        Constraint::Percentage(100 - DIFF_SIDEBAR_PERCENT),
    ])
    .split(area);

    // 枠線ぶんを差し引いたビューポート高さを保持（スクロール上限計算に使う）。
    let viewport = cols[1].height.saturating_sub(2) as usize;
    diff.viewport = viewport;
    let max_scroll = diff.parsed.len().saturating_sub(viewport.max(1));
    if diff.scroll > max_scroll {
        diff.scroll = max_scroll;
    }

    render_diff_sidebar(frame, cols[0], diff, &theme);
    render_diff_body(frame, cols[1], diff, &theme);
}

/// ファイル一覧サイドバー。選択中ファイル（`file_index`）をハイライトし、フォーカス中は
/// 枠線を `theme.border_focus` にする。
fn render_diff_sidebar(frame: &mut Frame, area: Rect, diff: &DiffState, theme: &Theme) {
    let focused = diff.focus == DiffFocus::Files;
    let title = format!(" ファイル ({}) ", diff.parsed.files.len());
    let block = themed_block(theme, focused).title(title);

    if diff.parsed.files.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（ファイル境界なし）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return;
    }

    // 枠線 + 左右パディングぶん（`rounded_block` が `Padding::horizontal(1)` を持つ）を
    // 差し引いた表示可能文字数。
    let name_width = area.width.saturating_sub(4);
    let items: Vec<ListItem> = diff
        .parsed
        .files
        .iter()
        .map(|file| {
            ListItem::new(Line::from(Span::raw(truncate_file_name(
                &file.name, name_width,
            ))))
        })
        .collect();

    let list = List::new(items)
        .block(block)
        .highlight_style(
            Style::new()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .bold(),
        )
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always);

    let selected = diff.file_index.min(diff.parsed.files.len() - 1);
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// ファイル名を表示幅に収める。長い場合は先頭を省略し末尾（ファイル名側）を優先して残す。
fn truncate_file_name(name: &str, max_width: u16) -> String {
    let budget = (max_width as usize).max(1);
    let chars: Vec<char> = name.chars().collect();
    if chars.len() <= budget {
        return name.to_string();
    }
    // 省略記号 1 文字ぶんを差し引いた残りを末尾から残す。
    let tail_len = budget.saturating_sub(1);
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("…{tail}")
}

/// 差分の可視範囲 `[start, end)` を計算する。
///
/// `scroll`/`viewport`/総行数のどんな組み合わせでも `start <= end <= len` を保証する
/// （`viewport` が総行数を超える／`scroll` が末尾を超える場合を含む）。
fn diff_visible_range(scroll: usize, viewport: usize, len: usize) -> (usize, usize) {
    let start = scroll.min(len);
    let end = start.saturating_add(viewport).min(len);
    (start, end)
}

fn render_diff_body(frame: &mut Frame, area: Rect, diff: &mut DiffState, theme: &Theme) {
    // 着色済み行は diff ロード時（新しい `DiffState`）ごと・テーマ切替ごとに一度だけ構築し、
    // 以降は使い回す。毎フレーム全行を `Span::styled` で作り直すと diff が大きいほど描画
    // コストが線形に増える（テーマ切替時のキャッシュ無効化は `App::cycle_theme` が行う）。
    let total = diff.parsed.len();
    let title = format!(" diff {} ({total} 行) ", diff.title);
    let lines = diff.rendered_lines.get_or_insert_with(|| {
        diff.parsed
            .lines
            .iter()
            .map(|line| {
                Line::from(Span::styled(
                    line.text.clone(),
                    diff_line_style(theme, line.kind),
                ))
            })
            .collect()
    });

    // per-frame のコストを O(viewport) に抑えるため、可視範囲だけを切り出して渡す
    // （`.scroll()` はここでは使わない。全行を Paragraph に渡すのを避けるのが目的）。
    let (start, end) = diff_visible_range(diff.scroll, diff.viewport, lines.len());
    let visible: Vec<Line> = lines[start..end].to_vec();

    let focused = diff.focus == DiffFocus::Body;
    let paragraph = Paragraph::new(visible).block(themed_block(theme, focused).title(title));
    frame.render_widget(paragraph, area);
}

/// diff の行種別に対応する前景色（テーマの意味役割へマッピング）。
///
/// `+`=成功色 / `-`=危険色 / `@@`=補足色 / ファイルヘッダ=警告色 / メタ=補助色 /
/// 文脈行=通常前景色。
fn diff_line_color(theme: &Theme, kind: DiffLineKind) -> Color {
    match kind {
        DiffLineKind::FileHeader => theme.warning,
        DiffLineKind::Hunk => theme.info,
        DiffLineKind::Added => theme.success,
        DiffLineKind::Removed => theme.danger,
        DiffLineKind::Meta => theme.muted,
        DiffLineKind::Context => theme.fg,
    }
}

fn diff_line_style(theme: &Theme, kind: DiffLineKind) -> Style {
    let base = Style::new().fg(diff_line_color(theme, kind));
    match kind {
        DiffLineKind::FileHeader => base.bold(),
        DiffLineKind::Meta => base.add_modifier(Modifier::DIM),
        _ => base,
    }
}

/// パイプライン/ステップ状態に対応する前景色。
fn pipeline_status_color(status: PipelineStatus, theme: &Theme) -> Color {
    match status {
        // 成功=成功色 / 失敗・エラー=危険色 / 進行中=警告色 / 停止・中止=補助色 / 保留=通常前景色。
        PipelineStatus::Successful => theme.success,
        PipelineStatus::Failed => theme.danger,
        PipelineStatus::InProgress => theme.warning,
        PipelineStatus::Stopped => theme.muted,
        PipelineStatus::Pending => theme.fg,
        PipelineStatus::Unknown => theme.muted,
    }
}

/// 状態バッジ（アイコン + `state`/`result` 名）の色付き Span。
fn pipeline_status_span(status: PipelineStatus, label: String, theme: &Theme) -> Span<'static> {
    Span::styled(
        format!("{} {label}", status.icon()),
        Style::new().fg(pipeline_status_color(status, theme)),
    )
}

fn render_pipelines(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let auto = if app.auto_refresh { "on" } else { "off" };
    if app.pipelines.items.is_empty() {
        render_placeholder(frame, area, &app.status, "パイプラインがありません", &theme);
        return;
    }
    let items: Vec<ListItem> = app
        .pipelines
        .items
        .iter()
        .map(|pipeline| ListItem::new(pipeline_row(pipeline, &theme)))
        .collect();
    let title = format!(
        " パイプライン ({}) [auto:{auto}] ",
        app.pipelines.items.len()
    );
    let list = list_widget(&theme, items, title);
    frame.render_stateful_widget(list, area, &mut app.pipelines.state);
}

fn pipeline_row(pipeline: &Pipeline, theme: &Theme) -> Line<'static> {
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
            Style::new().fg(theme.warning),
        ),
        pipeline_status_span(status, format!("{state_label:<20}"), theme),
        Span::styled(
            format!("  {}", pipeline.target_ref()),
            Style::new().fg(theme.info),
        ),
        Span::styled(
            format!("  {}", pipeline.trigger_name()),
            Style::new().fg(theme.info),
        ),
        Span::styled(format!("  {created}"), Style::new().dim()),
        Span::styled(
            format!("  {}", pipeline.duration_label()),
            Style::new().dim(),
        ),
    ])
}

fn render_pipeline_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let rows =
        Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)]).split(area);

    match app.current_pipeline.as_ref() {
        Some(pipeline) => render_pipeline_meta(frame, rows[0], pipeline, app.auto_refresh, &theme),
        None => render_placeholder(
            frame,
            rows[0],
            &app.status,
            "パイプラインを選択してください",
            &theme,
        ),
    }
    render_steps_list(frame, rows[1], &mut app.pipeline_steps, &theme);
}

fn render_pipeline_meta(
    frame: &mut Frame,
    area: Rect,
    pipeline: &Pipeline,
    auto_refresh: bool,
    theme: &Theme,
) {
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
                Style::new().fg(theme.warning).bold(),
            ),
            pipeline_status_span(status, state_label, theme),
        ]),
        Line::from(vec![
            Span::styled("target: ", Style::new().dim()),
            Span::styled(
                pipeline.target_ref().to_string(),
                Style::new().fg(theme.info),
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
    // 複数ペイン画面: 概要は静的な表示ペインなので非フォーカス（下のステップ一覧が主ペイン）。
    let paragraph = Paragraph::new(lines)
        .block(themed_block(theme, false).title(" 概要 "))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_steps_list(
    frame: &mut Frame,
    area: Rect,
    steps: &mut SelectList<PipelineStep>,
    theme: &Theme,
) {
    if steps.items.is_empty() {
        let block = themed_block(theme, true).title(" ステップ ");
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
                pipeline_status_span(status, format!("{state_label:<12}"), theme),
                Span::raw(step.name_str().to_string()),
                Span::styled(format!("  {}", step.duration_label()), Style::new().dim()),
            ]))
        })
        .collect();
    let title = format!(" ステップ ({}) ", steps.items.len());
    let list = list_widget(theme, items, title);
    frame.render_stateful_widget(list, area, &mut steps.state);
}

fn render_step_log(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let Some(log) = app.step_log.as_mut() else {
        render_placeholder(frame, area, &app.status, "ログを取得しています…", &theme);
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
        .block(themed_block(&theme, true).title(title))
        .scroll((log.scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

// ---- リポジトリブラウズ（M3） ----

fn render_branches(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.branches.items.is_empty() {
        render_placeholder(
            frame,
            list_area,
            &app.status,
            "ブランチがありません",
            &theme,
        );
    } else {
        let items: Vec<ListItem> = app
            .branches
            .items
            .iter()
            .map(|branch| ListItem::new(branch_row(branch, &theme)))
            .collect();
        let title = format!(" ブランチ ({}) ", app.branches.items.len());
        let list = list_widget(&theme, items, title);
        frame.render_stateful_widget(list, list_area, &mut app.branches.state);
    }

    render_pager(frame, pager_area, app.branches_page_info, &theme);
}

fn branch_row(branch: &Branch, theme: &Theme) -> Line<'static> {
    let name: String = branch.name_str().chars().take(30).collect();
    let summary: String = branch.target_summary().chars().take(50).collect();
    Line::from(vec![
        Span::styled(format!("{name:<30}"), Style::new().fg(theme.success)),
        Span::styled(
            format!("  {}", branch.target_short_hash()),
            Style::new().fg(theme.warning),
        ),
        Span::styled(
            format!("  {}", short_datetime(branch.target_date())),
            Style::new().dim(),
        ),
        Span::raw(format!("  {summary}")),
    ])
}

fn render_commits(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    if app.commits.items.is_empty() {
        render_placeholder(frame, area, &app.status, "コミットがありません", &theme);
        return;
    }
    let items: Vec<ListItem> = app
        .commits
        .items
        .iter()
        .map(|commit| ListItem::new(commit_row(commit, &theme)))
        .collect();
    let revision = app.commits_revision.as_deref().unwrap_or("既定ブランチ");
    let title = format!(" コミット [{revision}] ({}) ", app.commits.items.len());
    let list = list_widget(&theme, items, title);
    frame.render_stateful_widget(list, area, &mut app.commits.state);
}

fn commit_row(commit: &Commit, theme: &Theme) -> Line<'static> {
    let author: String = commit.author_name().chars().take(16).collect();
    let summary: String = commit.summary().chars().take(50).collect();
    Line::from(vec![
        Span::styled(
            format!("{:<8}", commit.short_hash()),
            Style::new().fg(theme.warning),
        ),
        Span::styled(
            format!("  {}", short_datetime(commit.date_str())),
            Style::new().dim(),
        ),
        Span::styled(format!("  {author:<16}"), Style::new().fg(theme.info)),
        Span::raw(format!("  {summary}")),
    ])
}

fn render_commit_detail(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    match app.current_commit.as_ref() {
        Some(commit) => render_commit_meta_body(frame, area, commit, app.commit_scroll, &theme),
        None => render_placeholder(
            frame,
            area,
            &app.status,
            "コミットを選択してください",
            &theme,
        ),
    }
}

fn render_commit_meta_body(
    frame: &mut Frame,
    area: Rect,
    commit: &Commit,
    scroll: u16,
    theme: &Theme,
) {
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
                Style::new().fg(theme.warning).bold(),
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
            Span::styled(parents_label, Style::new().fg(theme.info)),
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
        .block(themed_block(theme, true).title(" コミット "))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    frame.render_widget(paragraph, area);
}

fn render_source(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let Some(source) = app.source.as_mut() else {
        render_placeholder(frame, area, &app.status, "ソースがありません", &theme);
        return;
    };
    let title = format!(
        " src {} ({}) ",
        source.location(),
        source.entries.items.len()
    );
    if source.entries.items.is_empty() {
        let block = themed_block(&theme, true).title(title);
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
        .map(|entry| ListItem::new(src_entry_row(entry, &theme)))
        .collect();
    let list = list_widget(&theme, items, title);
    frame.render_stateful_widget(list, area, &mut source.entries.state);
}

fn src_entry_row(entry: &SrcEntry, theme: &Theme) -> Line<'static> {
    if entry.is_dir() {
        Line::from(Span::styled(
            format!("{}/", entry.name()),
            Style::new().fg(theme.info).bold(),
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
    let theme = app.theme;
    let Some(view) = app.file_view.as_mut() else {
        render_placeholder(
            frame,
            area,
            &app.status,
            "ファイルを取得しています…",
            &theme,
        );
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
        .block(themed_block(&theme, true).title(title))
        .scroll((view.scroll.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);
}

/// ISO8601 文字列を `YYYY-MM-DD HH:MM` へ短縮する（`T` を空白に）。
fn short_datetime(value: &str) -> String {
    let truncated: String = value.chars().take(16).collect();
    truncated.replacen('T', " ", 1)
}

fn render_confirm_modal(frame: &mut Frame, modal: &ConfirmModal, theme: &Theme) {
    let area = centered_rect(60, 40, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            modal.action.description(),
            Style::new().fg(theme.danger).bold(),
        )),
        Line::raw(""),
        Line::from(vec![
            Span::styled("対象: ", Style::new().dim()),
            Span::styled(modal.build_label.clone(), Style::new().fg(theme.warning)),
        ]),
        Line::raw(""),
    ];
    if modal.submitting {
        lines.push(Line::from(Span::styled(
            "実行中…",
            Style::new().fg(theme.warning),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Enter: 実行   Esc: 取消",
        Style::new().dim(),
    )));

    // 破壊的操作の確認モーダルなので枠線は危険色。
    let block = rounded_block(theme, theme.danger)
        .title(format!(" {} ", modal.action.title()))
        .style(Style::new().bg(theme.bg));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_placeholder(
    frame: &mut Frame,
    area: Rect,
    status: &Status,
    empty_text: &str,
    theme: &Theme,
) {
    let text = if matches!(status, Status::Loading(_)) {
        "読み込み中…"
    } else {
        empty_text
    };
    let block = themed_block(theme, false);
    let paragraph = Paragraph::new(Line::from(Span::styled(text, Style::new().dim())))
        .block(block)
        .alignment(Alignment::Center);
    frame.render_widget(paragraph, area);
}

/// インタラクティブな一覧（矢印キーで選択が動く画面の主ペイン）用の `List`。
///
/// 常にフォーカス色の枠線を使う（一覧が表示される画面では、それが常に主たる操作対象のため）。
fn list_widget<'a>(theme: &Theme, items: Vec<ListItem<'a>>, title: String) -> List<'a> {
    List::new(items)
        .block(themed_block(theme, true).title(title))
        .highlight_style(
            Style::new()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .bold(),
        )
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always)
}

fn render_status(frame: &mut Frame, area: Rect, status: &Status, theme: &Theme) {
    let line = match status {
        Status::Idle => Line::raw(""),
        Status::Loading(message) => Line::from(Span::styled(
            format!(" ⏳ {message}"),
            Style::new().fg(theme.warning),
        )),
        Status::Success(message) => Line::from(Span::styled(
            format!(" ✔ {message}"),
            Style::new().fg(theme.success).bold(),
        )),
        Status::Error(message) => Line::from(Span::styled(
            format!(" ✖ {message}"),
            Style::new().fg(theme.danger).bold(),
        )),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// 画面ごとのキーヒント（`(key, 説明)` の並び）。フッターとヘルプ双方の元データにはしない
/// （ヘルプは操作範囲が広く独立して管理する方が読みやすいため）。フッターは要点のみ。
///
/// `Ctrl+K`（ジャンプパレット）/ `?`（ヘルプ）/ `q`（終了）は `on_key` の優先度チェーン上
/// Onboarding を除く全画面で有効なため、末尾に共通で付与する（画面ごとの重複記述を避ける）。
/// Onboarding だけは対象外: `Ctrl+K` は emacs 風の「行末まで削除」に使用中で、`?` もヘルプでは
/// なく通常の入力文字として扱われる（[`crate::tui::app::App::on_key_onboarding`] 参照）。
fn hint_entries(screen: Screen) -> Vec<(&'static str, &'static str)> {
    let mut entries: Vec<(&'static str, &'static str)> = match screen {
        Screen::Onboarding => vec![
            ("Tab/↑↓", "フィールド切替"),
            ("Enter", "次へ/検証"),
            ("Esc", "エラー消去"),
            ("Ctrl+C", "終了"),
        ],
        Screen::Workspaces => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "開く"),
            ("/", "検索"),
            ("[/]", "前/次ページ"),
            ("g", "ページ番号ジャンプ"),
        ],
        Screen::Repositories => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "PR"),
            ("p", "パイプライン"),
            ("b", "ブランチ"),
            ("s", "ソース"),
            ("/", "検索"),
            ("S", "並び替え"),
            ("[/]", "前/次ページ"),
            ("g", "ページ番号ジャンプ"),
            ("Esc", "戻る"),
        ],
        Screen::PullRequests => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "詳細"),
            ("o", "Open"),
            ("m", "Merged"),
            ("d", "Declined"),
            ("a", "All"),
            ("r", "再読込"),
            ("P", "パイプライン"),
            ("b", "ブランチ"),
            ("s", "ソース"),
            ("/", "検索"),
            ("S", "並び替え"),
            ("[/]", "前/次ページ"),
            ("g", "ページ番号ジャンプ"),
            ("Esc", "戻る"),
        ],
        Screen::PullRequestDetail => vec![
            ("d", "Diff"),
            ("c", "コメント"),
            ("a", "承認"),
            ("x", "変更要求"),
            ("M", "マージ"),
            ("o", "ブラウザで開く"),
            ("↑↓/jk", "ファイル"),
            ("Shift+J/K", "本文10行"),
            ("Esc", "戻る"),
        ],
        Screen::Diff => vec![
            ("Tab", "一覧/本文"),
            ("↑↓/jk", "選択/スクロール"),
            ("Shift+J/K", "10行"),
            ("n/N", "ファイル境界"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("Esc", "戻る"),
        ],
        Screen::Pipelines => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "詳細"),
            ("r", "再読込"),
            ("a", "自動更新"),
            ("S", "停止"),
            ("R", "再実行"),
            ("Esc", "戻る"),
        ],
        Screen::PipelineDetail => vec![
            ("↑↓/jk", "ステップ"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "ログ"),
            ("r", "再読込"),
            ("a", "自動更新"),
            ("S", "停止"),
            ("R", "再実行"),
            ("Esc", "戻る"),
        ],
        Screen::StepLog => vec![
            ("↑↓/jk", "スクロール"),
            ("Shift+J/K", "10行"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("r", "再取得"),
            ("Esc", "戻る"),
        ],
        Screen::Branches => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "コミット履歴"),
            ("s", "ソース"),
            ("r", "再読込"),
            ("[/]", "前/次ページ"),
            ("g", "ページ番号ジャンプ"),
            ("Esc", "戻る"),
        ],
        Screen::Commits => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "詳細"),
            ("r", "再読込"),
            ("Esc", "戻る"),
        ],
        Screen::CommitDetail => vec![
            ("d", "Diff"),
            ("↑↓/jk/PgUp/PgDn", "スクロール"),
            ("Shift+J/K", "10行"),
            ("Esc", "戻る"),
        ],
        Screen::Source => vec![
            ("↑↓/jk", "移動"),
            ("Shift+J/K", "10件移動"),
            ("Enter", "開く"),
            ("Backspace/Esc", "親へ"),
            ("r", "再読込"),
        ],
        Screen::FileView => vec![
            ("↑↓/jk", "スクロール"),
            ("Shift+J/K", "10行"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("Esc", "戻る"),
        ],
    };

    if screen != Screen::Onboarding {
        entries.push(("Ctrl+K", "ジャンプ"));
        entries.push(("?", "ヘルプ"));
        entries.push(("q", "終了"));
    }

    entries
}

fn render_hints(frame: &mut Frame, area: Rect, screen: Screen, theme: &Theme) {
    let mut spans = Vec::new();
    for (index, (key, description)) in hint_entries(screen).iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(Span::styled(*key, Style::new().fg(theme.accent)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*description, Style::new().fg(theme.muted)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_comment_editor(frame: &mut Frame, editor: &CommentEditor, theme: &Theme) {
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
            Style::new().fg(theme.warning),
        )));
    }
    lines.push(Line::from(Span::styled(
        "改行: Enter    送信: Ctrl+S    取消: Esc",
        Style::new().dim(),
    )));

    // 非破壊的な入力オーバーレイなのでフォーカス色の枠線。
    let block = rounded_block(theme, theme.border_focus)
        .title(" コメントを書く ")
        .style(Style::new().bg(theme.bg));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn render_merge_modal(
    frame: &mut Frame,
    modal: &MergeModal,
    pr: Option<&PullRequest>,
    theme: &Theme,
) {
    let area = centered_rect(60, 55, frame.area());
    frame.render_widget(Clear, area);

    let title = pr
        .map(|pr| format!(" マージ確認: #{} ", pr.id))
        .unwrap_or_else(|| " マージ確認 ".to_string());

    let mut lines = vec![
        Line::from(Span::styled(
            "破壊的操作: この PR をマージします。",
            Style::new().fg(theme.danger).bold(),
        )),
        Line::raw(""),
        Line::from(Span::styled("マージ戦略:", Style::new().bold())),
    ];
    for (index, strategy) in MergeStrategy::ALL.iter().enumerate() {
        let selected = index == modal.strategy % MergeStrategy::ALL.len();
        let marker = if selected { "▶ " } else { "  " };
        let style = if selected {
            Style::new().fg(theme.accent).bold()
        } else {
            Style::new()
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::new().fg(theme.accent)),
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
            Style::new().fg(theme.warning),
        )));
    }
    lines.push(Line::from(Span::styled(
        "←/→/Tab: 戦略   Space: ブランチ削除   Enter: 実行   Esc: 取消",
        Style::new().dim(),
    )));

    // 破壊的操作の確認モーダルなので枠線は危険色。
    let block = rounded_block(theme, theme.danger)
        .title(title)
        .style(Style::new().bg(theme.bg));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_help(frame: &mut Frame, screen: Screen, theme: &Theme) {
    let area = centered_rect(64, 70, frame.area());
    frame.render_widget(Clear, area);

    let mut lines = vec![
        Line::from(Span::styled(
            "キーバインド（共通）",
            Style::new().fg(theme.accent).bold(),
        )),
        Line::raw(""),
        Line::raw("↑ / k, ↓ / j   上下へ移動"),
        Line::raw("Enter          決定 / 開く"),
        Line::raw("Esc            戻る（Onboarding はエラー消去 / Workspaces は非対応）"),
        Line::raw("?              このヘルプ（Onboarding では非対応）"),
        Line::raw("q              終了（Onboarding では非対応。Ctrl+C は常に有効）"),
        Line::raw("Ctrl+T         テーマ切替（常に有効）"),
        Line::raw("Ctrl+K         ジャンプパレット（保持済みデータへ一気に移動。Onboarding 以外の"),
        Line::raw("               全画面で有効。検索編集中や各種モーダル表示中は無効）"),
        Line::raw("Ctrl+C         強制終了（常に有効）"),
        Line::raw("/              検索（Workspaces/Repositories/PullRequests。文字入力で絞込み、"),
        Line::raw("               Enter で確定、Esc で解除）"),
    ];

    let screen_keys: &[&str] = match screen {
        Screen::Onboarding => &[
            "Tab / ↑↓       Email ⇔ Token フィールド切替",
            "Enter          Email 入力後は Token へ / Token 入力後は認証検証を開始",
            "←→ / Home/End  カーソル移動",
            "Backspace/Delete カーソルの前 / 後ろを 1 文字削除",
            "Ctrl+A / E     行頭 / 行末へ移動",
            "Ctrl+B / F     カーソルを1つ左 / 右へ移動",
            "Ctrl+U         カーソルから行頭まで削除",
            "Ctrl+K         カーソルから行末まで削除（ジャンプパレットではない）",
            "Ctrl+W         直前の単語を削除",
            "Ctrl+D / H     Delete / Backspace と同じ",
        ],
        Screen::PullRequests => &[
            "o              状態フィルタ: Open",
            "m              状態フィルタ: Merged",
            "d              状態フィルタ: Declined",
            "a              状態フィルタ: All",
            "r              再読込（現在ページ）",
            "Enter          PR 詳細を開く",
            "P              パイプライン一覧を開く",
            "b              ブランチ一覧を開く",
            "s              ソースを開く",
            "S              並び替え（更新が新しい順→古い順→作成が新しい順→古い順の巡回。",
            "               タイトルに現在のソートを表示）",
            "Shift+J / K    10 件下 / 上へ移動",
            "[ / ]          前 / 次ページ（1 ページ 40 件）",
            "g              ページ番号ジャンプ（数字入力 + Enter, Esc で取消）",
        ],
        Screen::PullRequestDetail => &[
            "d              Diff を開く",
            "c              コメント投稿（Enter 改行 / Ctrl+S 送信 / Esc 取消）",
            "a              approve / unapprove トグル",
            "x              request-changes / 取消 トグル",
            "M              マージ（確認モーダル: ←→/Tab 戦略切替, Space ブランチ削除切替,",
            "               Enter 実行, Esc 取消）",
            "o              ブラウザで開く（`open` コマンドで既定ブラウザに開く）",
            "↑↓ / j k       変更ファイル選択",
            "PgUp/PgDn      本文スクロール（±5 行）",
            "Shift+J / K    本文スクロール（±10 行）",
        ],
        Screen::Diff => &[
            "Tab            ファイル一覧 / 本文フォーカス切替",
            "↑↓ / j k       (一覧) ファイル選択  /  (本文) 1 行スクロール",
            "Shift+J / K    本文 10 行スクロール",
            "PgUp/PgDn / f/b 1 画面スクロール（本文）",
            "g / Home, G / End 先頭 / 末尾（本文）",
            "n / N          次 / 前のファイル境界（フォーカス問わず）",
        ],
        Screen::Repositories => &[
            "Enter          プルリクエスト一覧を開く",
            "p              パイプライン一覧を開く",
            "b              ブランチ一覧を開く",
            "s              ソース（既定ブランチのルート）を開く",
            "S              並び替え（更新が新しい順→古い順→作成が新しい順→古い順の巡回。",
            "               タイトルに現在のソートを表示）",
            "Shift+J / K    10 件下 / 上へ移動",
            "[ / ]          前 / 次ページ（1 ページ 40 件）",
            "g              ページ番号ジャンプ（数字入力 + Enter, Esc で取消）",
        ],
        Screen::Pipelines => &[
            "Enter          パイプライン詳細を開く",
            "r              一覧を再読込",
            "a              自動更新の ON/OFF",
            "S              停止（進行中のみ・確認モーダル: Enter 実行 / Esc 取消）",
            "R              再実行（確認モーダル: Enter 実行 / Esc 取消）",
            "Shift+J / K    10 件下 / 上へ移動",
        ],
        Screen::PipelineDetail => &[
            "↑↓ / j k       ステップ選択",
            "Shift+J / K    10 件下 / 上へ移動",
            "Enter          ステップのログを開く",
            "r              詳細を再読込",
            "a              自動更新の ON/OFF",
            "S / R          停止 / 再実行（確認モーダル: Enter 実行 / Esc 取消）",
        ],
        Screen::StepLog => &[
            "↑↓ / j k       1 行スクロール",
            "Shift+J / K    10 行スクロール",
            "PgUp/PgDn / f/b 1 画面スクロール",
            "g / Home, G / End 先頭 / 末尾",
            "r              ログ再取得（擬似 tail）",
        ],
        Screen::Branches => &[
            "Enter          そのブランチのコミット履歴",
            "s              そのブランチのソースルート",
            "r              一覧を再読込",
            "Shift+J / K    10 件下 / 上へ移動",
            "[ / ]          前 / 次ページ（1 ページ 40 件）",
            "g              ページ番号ジャンプ（数字入力 + Enter, Esc で取消）",
            "Esc            戻る（Repositories/PullRequests のうち入って来た画面へ）",
        ],
        Screen::Commits => &[
            "Enter          コミット詳細を開く",
            "r              履歴を再読込",
            "Shift+J / K    10 件下 / 上へ移動",
        ],
        Screen::CommitDetail => &[
            "d              このコミットの Diff を開く",
            "↑↓ / j k       1 行スクロール（PgUp/PgDn で ±5 行）",
            "Shift+J / K    10 行スクロール",
        ],
        Screen::Source => &[
            "Enter          ディレクトリを開く / ファイルを表示",
            "Backspace/Esc  親ディレクトリへ（ルートで、Repositories/PullRequests のうち",
            "               入って来た画面へ戻る）",
            "r              再読込",
            "Shift+J / K    10 件下 / 上へ移動",
        ],
        Screen::FileView => &[
            "↑↓ / j k       1 行スクロール",
            "Shift+J / K    10 行スクロール",
            "PgUp/PgDn / f/b 1 画面スクロール",
            "g / Home, G / End 先頭 / 末尾",
        ],
        Screen::Workspaces => &[
            "Shift+J / K    10 件下 / 上へ移動",
            "[ / ]          前 / 次ページ（1 ページ 40 件）",
            "g              ページ番号ジャンプ（数字入力 + Enter, Esc で取消）",
        ],
    };
    if !screen_keys.is_empty() {
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            format!("{} 画面", screen_title(screen)),
            Style::new().fg(theme.accent).bold(),
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

    let block = rounded_block(theme, theme.border_focus)
        .title(" ヘルプ ")
        .style(Style::new().bg(theme.bg));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false }),
        area,
    );
}

/// ジャンプパレット（`Ctrl+K`）を描画する。入力行 + ヒント行 + 候補一覧の 3 段構成。
fn render_jump_palette(frame: &mut Frame, palette: &mut JumpPaletteState, theme: &Theme) {
    let area = centered_rect(70, 60, frame.area());
    frame.render_widget(Clear, area);

    let block = rounded_block(theme, theme.border_focus)
        .title(" ジャンプ (Ctrl+K) ")
        .style(Style::new().bg(theme.bg));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
    ])
    .split(inner);

    let input_line = Line::from(vec![
        Span::styled("> ", Style::new().fg(theme.accent).bold()),
        Span::raw(palette.entries.filter.clone()),
        Span::styled("▏", Style::new().fg(theme.accent)),
    ]);
    frame.render_widget(Paragraph::new(input_line), rows[0]);

    let hint_line = Line::from(Span::styled(
        "↑↓: 選択   Enter: 移動   Esc: 閉じる",
        Style::new().dim(),
    ));
    frame.render_widget(Paragraph::new(hint_line), rows[1]);

    if palette.entries.matches.is_empty() {
        let paragraph = Paragraph::new(Line::from(Span::styled(
            "一致するものがありません",
            Style::new().dim(),
        )));
        frame.render_widget(paragraph, rows[2]);
        return;
    }

    let items: Vec<ListItem> = palette
        .entries
        .visible()
        .map(|entry| ListItem::new(entry.label.clone()))
        .collect();
    let list = List::new(items)
        .highlight_style(
            Style::new()
                .bg(theme.selection_bg)
                .fg(theme.selection_fg)
                .bold(),
        )
        .highlight_symbol("▌ ")
        .highlight_spacing(HighlightSpacing::Always);
    frame.render_stateful_widget(list, rows[2], &mut palette.entries.state);
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
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    use crate::tui::diff::parse as parse_diff;
    use crate::tui::theme::ThemeName;

    /// `line_count` 行のダミー diff（すべて文脈行）から `DiffState` を作る。
    fn make_diff_state(line_count: usize) -> DiffState {
        let mut text = String::new();
        for index in 0..line_count {
            text.push_str(&format!(" context line {index}\n"));
        }
        DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 0,
            title: "#1".to_string(),
            rendered_lines: None,
            file_index: 0,
            focus: DiffFocus::Body,
        }
    }

    /// ファイル境界を複数持つ diff（サイドバー・フォーカス関連のテスト用）。
    /// `file_count` 個のファイル、各ファイル `lines_per_file` 行のコンテキスト行を持つ。
    fn make_multi_file_diff_state(file_count: usize, lines_per_file: usize) -> DiffState {
        let mut text = String::new();
        for file_index in 0..file_count {
            text.push_str(&format!(
                "diff --git a/file{file_index}.txt b/file{file_index}.txt\n\
--- a/file{file_index}.txt\n\
+++ b/file{file_index}.txt\n\
@@ -1,{lines_per_file} +1,{lines_per_file} @@\n"
            ));
            for line_index in 0..lines_per_file {
                text.push_str(&format!(" file{file_index} line {line_index}\n"));
            }
        }
        DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 0,
            title: "#1".to_string(),
            rendered_lines: None,
            file_index: 0,
            focus: DiffFocus::Body,
        }
    }

    fn buffer_text(buffer: &Buffer) -> String {
        buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn diff_visible_range_slices_from_scroll_offset() {
        assert_eq!(diff_visible_range(5, 10, 20), (5, 15));
    }

    #[test]
    fn diff_visible_range_covers_full_when_viewport_exceeds_len() {
        assert_eq!(diff_visible_range(0, 100, 10), (0, 10));
    }

    #[test]
    fn diff_visible_range_clamps_at_tail() {
        assert_eq!(diff_visible_range(15, 10, 20), (15, 20));
    }

    #[test]
    fn diff_visible_range_handles_zero_viewport() {
        assert_eq!(diff_visible_range(3, 0, 20), (3, 3));
    }

    #[test]
    fn diff_visible_range_handles_empty_diff() {
        assert_eq!(diff_visible_range(0, 10, 0), (0, 0));
    }

    #[test]
    fn diff_visible_range_never_panics_when_scroll_exceeds_len() {
        // 通常は呼び出し側（`render_diff`）が事前にクランプするが、万一 scroll が総行数を
        // 超えて渡されても範囲が破綻しないことを確認する。
        assert_eq!(diff_visible_range(50, 10, 20), (20, 20));
    }

    #[test]
    fn render_diff_body_caches_lines_and_reuses_allocation_across_frames() {
        let mut diff = make_diff_state(300);
        diff.viewport = 15;
        diff.scroll = 0;
        let theme = Theme::default();

        let backend = TestBackend::new(40, 17);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("first draw succeeds");

        let cached = diff
            .rendered_lines
            .as_ref()
            .expect("cache built on first render");
        assert_eq!(cached.len(), diff.parsed.len());
        let first_ptr = cached.as_ptr();

        // scroll を変えて再描画してもキャッシュは再構築されず、同じアロケーションを使い回す。
        diff.scroll = 200;
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("second draw succeeds");
        let second_ptr = diff
            .rendered_lines
            .as_ref()
            .expect("cache still present")
            .as_ptr();
        assert_eq!(
            first_ptr, second_ptr,
            "diff の着色済み行キャッシュは一度だけ構築され、以後は再利用されるべき"
        );
    }

    #[test]
    fn render_diff_body_only_draws_viewport_worth_of_lines() {
        let mut diff = make_diff_state(50);
        diff.viewport = 5;
        diff.scroll = 3;
        let theme = Theme::default();

        // 幅十分・高さ = 可視 5 行 + 上下ボーダー。
        let backend = TestBackend::new(30, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        // scroll=3, viewport=5 → 表示されるのは行 3..8。
        assert!(content.contains("context line 3"));
        assert!(content.contains("context line 7"));
        // 範囲外（viewport を超える行）は表示されない。
        assert!(!content.contains("context line 8"));
        assert!(!content.contains("context line 2"));
    }

    #[test]
    fn render_diff_body_handles_viewport_larger_than_diff() {
        let mut diff = make_diff_state(3);
        diff.viewport = 100;
        diff.scroll = 0;
        let theme = Theme::default();

        let backend = TestBackend::new(30, 102);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("context line 0"));
        assert!(content.contains("context line 2"));
    }

    #[test]
    fn render_diff_body_rebuilds_cache_with_new_theme_colors_after_invalidation() {
        // `App::cycle_theme` は `rendered_lines = None` にしてから再描画する契約になっている。
        // ここでは「キャッシュが None のときは新しい theme 引数で作り直される」ことだけを保証する
        // （色そのものの比較は diff_line_color のテストで行う）。
        let mut diff = make_diff_state(5);
        diff.viewport = 5;
        let catppuccin = Theme::default();

        let backend = TestBackend::new(30, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &catppuccin);
            })
            .expect("first draw succeeds");
        assert!(diff.rendered_lines.is_some());

        // テーマ切替相当（`App::cycle_theme` がやること）。
        diff.rendered_lines = None;
        let nord = ThemeName::Nord.theme();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &nord);
            })
            .expect("second draw succeeds");
        assert!(
            diff.rendered_lines.is_some(),
            "無効化後はキャッシュが再構築されるべき"
        );
    }

    /// `diff` を持つ最小構成の `App`（Diff 画面の 2 ペイン描画テスト用）。
    fn app_with_diff(diff: DiffState) -> App {
        let mut app = App::new(crate::config::Config::default(), None);
        app.screen = Screen::Diff;
        app.diff = Some(diff);
        app
    }

    #[test]
    fn truncate_file_name_keeps_short_names_untouched() {
        assert_eq!(truncate_file_name("src/main.rs", 20), "src/main.rs");
    }

    #[test]
    fn truncate_file_name_prefers_tail_when_too_long() {
        let truncated = truncate_file_name("very/long/nested/path/to/file.rs", 10);
        assert_eq!(truncated.chars().count(), 10);
        assert!(truncated.starts_with('…'));
        assert!(truncated.ends_with("file.rs"));
    }

    #[test]
    fn truncate_file_name_never_panics_on_zero_width() {
        assert_eq!(truncate_file_name("file.rs", 0), "…");
    }

    #[test]
    fn replace_image_syntax_replaces_full_line_image() {
        let replaced = replace_image_syntax("![Screenshot](https://example.com/img.png)");
        assert_eq!(replaced, "[画像: Screenshot]（o でブラウザ表示）");
    }

    #[test]
    fn replace_image_syntax_replaces_inline_image_and_keeps_surrounding_text() {
        let replaced = replace_image_syntax("見て: ![図](https://example.com/a.png) です");
        assert_eq!(replaced, "見て: [画像: 図]（o でブラウザ表示） です");
    }

    #[test]
    fn replace_image_syntax_leaves_plain_text_untouched() {
        let replaced = replace_image_syntax("普通のテキストです");
        assert_eq!(replaced, "普通のテキストです");
    }

    #[test]
    fn replace_image_syntax_tolerates_malformed_syntax() {
        // `(url)` が続かない壊れた記法はそのまま残す（フェイルソフト）。
        let replaced = replace_image_syntax("![alt without parens");
        assert_eq!(replaced, "![alt without parens");
    }

    #[test]
    fn replace_image_syntax_handles_multiple_images_in_one_line() {
        let replaced = replace_image_syntax("![a](u1) と ![b](u2)");
        assert_eq!(
            replaced,
            "[画像: a]（o でブラウザ表示） と [画像: b]（o でブラウザ表示）"
        );
    }

    #[test]
    fn is_heading_line_detects_hash_headings() {
        assert!(is_heading_line("# Title"));
        assert!(is_heading_line("## Subtitle"));
        assert!(is_heading_line("###### Deep"));
        assert!(is_heading_line("#"));
        assert!(!is_heading_line("#no-space"));
        assert!(!is_heading_line("normal text"));
        assert!(!is_heading_line("####### too many"));
    }

    #[test]
    fn split_bullet_prefix_splits_dash_and_star_bullets() {
        let (prefix, rest) = split_bullet_prefix("- item one").expect("bullet");
        assert_eq!(prefix, "- ");
        assert_eq!(rest, "item one");

        let (prefix, rest) = split_bullet_prefix("* item two").expect("bullet");
        assert_eq!(prefix, "* ");
        assert_eq!(rest, "item two");
    }

    #[test]
    fn split_bullet_prefix_preserves_leading_indent() {
        let (prefix, rest) = split_bullet_prefix("  - nested").expect("bullet");
        assert_eq!(prefix, "  - ");
        assert_eq!(rest, "nested");
    }

    #[test]
    fn split_bullet_prefix_returns_none_for_non_bullet_lines() {
        assert!(split_bullet_prefix("plain text").is_none());
        assert!(split_bullet_prefix("-no space after dash").is_none());
    }

    #[test]
    fn inline_code_spans_highlights_backtick_segments() {
        let theme = Theme::default();
        let spans = inline_code_spans("use `foo` here", &theme);
        assert_eq!(
            spans,
            vec![
                Span::raw("use ".to_string()),
                Span::styled("foo".to_string(), Style::new().fg(theme.muted)),
                Span::raw(" here".to_string()),
            ]
        );
    }

    #[test]
    fn inline_code_spans_without_backticks_is_plain_raw() {
        let theme = Theme::default();
        let spans = inline_code_spans("plain text", &theme);
        assert_eq!(spans, vec![Span::raw("plain text".to_string())]);
    }

    #[test]
    fn render_markdown_lines_styles_heading_bullet_and_image() {
        let theme = Theme::default();
        let body = "# Heading\n- item `code`\n![alt](https://example.com/x.png)\nplain";
        let lines = render_markdown_lines(body, &theme);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[0],
            Line::from(Span::styled(
                "# Heading".to_string(),
                Style::new().fg(theme.accent).bold()
            ))
        );
        assert_eq!(
            lines[1],
            Line::from(vec![
                Span::styled("- ".to_string(), Style::new().fg(theme.accent)),
                Span::raw("item ".to_string()),
                Span::styled("code".to_string(), Style::new().fg(theme.muted)),
            ])
        );
        assert_eq!(
            lines[2],
            Line::from(vec![Span::raw(
                "[画像: alt]（o でブラウザ表示）".to_string()
            )])
        );
        assert_eq!(lines[3], Line::from(vec![Span::raw("plain".to_string())]));
    }

    #[test]
    fn render_markdown_lines_dims_code_fence_block() {
        let theme = Theme::default();
        let body = "before\n```\nlet x = 1;\n```\nafter";
        let lines = render_markdown_lines(body, &theme);
        assert_eq!(lines.len(), 5);
        assert_eq!(
            lines[1],
            Line::from(Span::styled(
                "```".to_string(),
                Style::new().fg(theme.muted)
            ))
        );
        assert_eq!(
            lines[2],
            Line::from(Span::styled(
                "let x = 1;".to_string(),
                Style::new().fg(theme.muted)
            ))
        );
        assert_eq!(
            lines[3],
            Line::from(Span::styled(
                "```".to_string(),
                Style::new().fg(theme.muted)
            ))
        );
    }

    #[test]
    fn render_markdown_lines_preserves_one_to_one_line_mapping() {
        let theme = Theme::default();
        let body = "line1\nline2\nline3";
        assert_eq!(render_markdown_lines(body, &theme).len(), 3);
    }

    fn make_pr_with_participants(json: &str) -> PullRequest {
        serde_json::from_str(json).expect("valid pr json")
    }

    #[test]
    fn participant_panel_lines_shows_approved_and_changes_requested() {
        let theme = Theme::default();
        let pr = make_pr_with_participants(
            r#"{ "id": 1, "participants": [
                { "user": { "display_name": "Bob" }, "approved": true, "state": "approved" },
                { "user": { "display_name": "Carol" }, "approved": false, "state": "changes_requested" }
            ] }"#,
        );
        let lines = participant_panel_lines(&pr, &theme);
        assert_eq!(
            lines,
            vec![
                Line::from(vec![
                    Span::styled("承認: ".to_string(), Style::new().fg(theme.success)),
                    Span::raw("Bob".to_string()),
                ]),
                Line::from(vec![
                    Span::styled("変更要求: ".to_string(), Style::new().fg(theme.danger)),
                    Span::raw("Carol".to_string()),
                ]),
            ]
        );
    }

    #[test]
    fn participant_panel_lines_empty_when_no_participants() {
        let theme = Theme::default();
        let pr = make_pr_with_participants(r#"{ "id": 2, "participants": [] }"#);
        assert!(participant_panel_lines(&pr, &theme).is_empty());
    }

    #[test]
    fn render_comments_indents_reply_with_parent() {
        let theme = Theme::default();
        let root: Comment = serde_json::from_str(
            r#"{ "id": 1, "content": { "raw": "root comment" },
                "user": { "display_name": "Alice" } }"#,
        )
        .expect("valid comment json");
        let reply: Comment = serde_json::from_str(
            r#"{ "id": 2, "content": { "raw": "reply comment" },
                "user": { "display_name": "Bob" }, "parent": { "id": 1 } }"#,
        )
        .expect("valid comment json");
        let comments = vec![root, reply];

        let backend = TestBackend::new(50, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                render_comments(frame, frame.area(), &comments, &theme);
            })
            .expect("draw succeeds");
        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("root comment"));
        assert!(text.contains("↳"));
        assert!(text.contains("reply comment"));
    }

    #[test]
    fn render_diff_renders_two_panes_with_file_list_and_body() {
        let mut app = app_with_diff(make_multi_file_diff_state(2, 3));

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        // サイドバーにファイル名が並ぶ。
        assert!(content.contains("file0.txt"));
        assert!(content.contains("file1.txt"));
        // 本文には選択中（先頭）ファイルの内容が見える。
        assert!(content.contains("file0 line 0"));
    }

    #[test]
    fn render_diff_keeps_body_cache_across_frames_with_sidebar_present() {
        // Phase1 の性能最適化（`rendered_lines` を 1 度だけ構築しキャッシュを使い回す）が
        // サイドバー追加後も壊れていないことを確認する。
        let mut app = app_with_diff(make_multi_file_diff_state(3, 100));

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("first draw succeeds");
        let first_ptr = app
            .diff
            .as_ref()
            .expect("diff present")
            .rendered_lines
            .as_ref()
            .expect("cache built on first render")
            .as_ptr();

        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("second draw succeeds");
        let second_ptr = app
            .diff
            .as_ref()
            .expect("diff present")
            .rendered_lines
            .as_ref()
            .expect("cache still present")
            .as_ptr();

        assert_eq!(
            first_ptr, second_ptr,
            "サイドバー描画を挟んでも本文の着色済み行キャッシュは再構築されないべき"
        );
    }

    #[test]
    fn render_diff_sidebar_selection_highlights_and_focus_moves_border_color() {
        let mut diff = make_multi_file_diff_state(2, 3);
        diff.file_index = 1;
        let theme = Theme::default();
        let sidebar_area = Rect::new(0, 0, 20, 12);

        // フォーカスがファイル一覧のとき: 左ペイン枠線がフォーカス色。
        diff.focus = DiffFocus::Files;
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render_diff_sidebar(frame, sidebar_area, &diff, &theme))
            .expect("draw succeeds");
        let top_left = terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .expect("cell exists");
        assert_eq!(top_left.fg, theme.border_focus);
        let content = buffer_text(terminal.backend().buffer());
        // 選択中（file_index = 1）のファイル行にハイライト記号が付く。
        assert!(content.contains("▌"));

        // フォーカスが本文のとき: 左ペイン枠線は非フォーカス色。
        diff.focus = DiffFocus::Body;
        terminal
            .draw(|frame| render_diff_sidebar(frame, sidebar_area, &diff, &theme))
            .expect("draw succeeds");
        let top_left = terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .expect("cell exists");
        assert_eq!(top_left.fg, theme.border);
    }

    #[test]
    fn pipeline_status_colors_map_to_theme_roles() {
        let theme = Theme::default();
        // 成功=成功色 / 失敗・エラー=危険色 / 進行中=警告色 / 停止・中止=補助色 / 保留=通常前景色。
        assert_eq!(
            pipeline_status_color(PipelineStatus::Successful, &theme),
            theme.success
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::Failed, &theme),
            theme.danger
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::InProgress, &theme),
            theme.warning
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::Stopped, &theme),
            theme.muted
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::Pending, &theme),
            theme.fg
        );
        assert_eq!(
            pipeline_status_color(PipelineStatus::Unknown, &theme),
            theme.muted
        );
    }

    #[test]
    fn diff_line_colors_map_to_theme_roles() {
        let theme = Theme::default();
        assert_eq!(diff_line_color(&theme, DiffLineKind::Added), theme.success);
        assert_eq!(diff_line_color(&theme, DiffLineKind::Removed), theme.danger);
        assert_eq!(diff_line_color(&theme, DiffLineKind::Hunk), theme.info);
        assert_eq!(
            diff_line_color(&theme, DiffLineKind::FileHeader),
            theme.warning
        );
        assert_eq!(diff_line_color(&theme, DiffLineKind::Meta), theme.muted);
        assert_eq!(diff_line_color(&theme, DiffLineKind::Context), theme.fg);
    }

    #[test]
    fn state_style_maps_pr_states_to_theme_roles() {
        let theme = Theme::default();
        assert_eq!(state_style("OPEN", &theme).fg, Some(theme.success));
        assert_eq!(state_style("MERGED", &theme).fg, Some(theme.accent));
        assert_eq!(state_style("DECLINED", &theme).fg, Some(theme.danger));
        assert_eq!(state_style("SUPERSEDED", &theme).fg, Some(theme.muted));
        assert_eq!(state_style("UNKNOWN", &theme).fg, Some(theme.muted));
    }

    #[test]
    fn diffstat_status_style_maps_to_theme_roles() {
        let theme = Theme::default();
        assert_eq!(
            diffstat_status_style("added", &theme).fg,
            Some(theme.success)
        );
        assert_eq!(
            diffstat_status_style("removed", &theme).fg,
            Some(theme.danger)
        );
        assert_eq!(
            diffstat_status_style("renamed", &theme).fg,
            Some(theme.warning)
        );
        assert_eq!(
            diffstat_status_style("modified", &theme).fg,
            Some(theme.info)
        );
        assert_eq!(diffstat_status_style("other", &theme).fg, Some(theme.muted));
    }

    /// スパンから連結テキストを取り出す（ページャ行のテキスト内容を検証するためのヘルパ）。
    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn pager_page_labels_small_total_shows_every_page_without_ellipsis() {
        assert_eq!(
            pager_page_labels(3, 5),
            vec![Some(1), Some(2), Some(3), Some(4), Some(5)]
        );
    }

    #[test]
    fn pager_page_labels_large_total_elides_far_pages() {
        assert_eq!(
            pager_page_labels(5, 20),
            vec![Some(1), None, Some(4), Some(5), Some(6), None, Some(20)]
        );
    }

    #[test]
    fn pager_page_labels_current_near_start_has_single_trailing_ellipsis() {
        assert_eq!(
            pager_page_labels(1, 20),
            vec![Some(1), Some(2), None, Some(20)]
        );
    }

    #[test]
    fn pager_page_labels_current_near_end_has_single_leading_ellipsis() {
        assert_eq!(
            pager_page_labels(20, 20),
            vec![Some(1), None, Some(19), Some(20)]
        );
    }

    #[test]
    fn pager_page_labels_clamps_zero_total_to_single_page() {
        assert_eq!(pager_page_labels(1, 0), vec![Some(1)]);
    }

    #[test]
    fn pager_page_labels_clamps_out_of_range_current() {
        // current が total を超えていても panic せず、末尾へクランプする。
        assert_eq!(
            pager_page_labels(99, 5),
            vec![Some(1), None, Some(4), Some(5)]
        );
    }

    #[test]
    fn pager_line_single_page_shows_only_page_one_with_inactive_arrows() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 1,
            total_pages: Some(1),
            has_next: false,
        };
        let line = pager_line(info, &theme);
        assert_eq!(line_text(&line), "‹ 1 ›");
        // 矢印は両方非活性（前ページ無し・次ページ無し）。
        assert_eq!(
            line.spans.first().expect("prev arrow").style.fg,
            Some(theme.muted)
        );
        assert_eq!(
            line.spans.last().expect("next arrow").style.fg,
            Some(theme.muted)
        );
    }

    #[test]
    fn pager_line_empty_list_with_zero_total_pages_does_not_panic() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 1,
            total_pages: Some(0),
            has_next: false,
        };
        // 0 ページ（空一覧）でも panic せず、単一の空ページとして "1" を表示する。
        let line = pager_line(info, &theme);
        assert_eq!(line_text(&line), "‹ 1 ›");
    }

    #[test]
    fn pager_line_highlights_current_page_with_accent() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 3,
            total_pages: Some(5),
            has_next: true,
        };
        let line = pager_line(info, &theme);
        let current = line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "3 ")
            .expect("current page span present");
        assert_eq!(current.style.fg, Some(theme.accent));
    }

    #[test]
    fn pager_line_elides_far_pages_with_ellipsis() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 5,
            total_pages: Some(20),
            has_next: true,
        };
        let text = line_text(&pager_line(info, &theme));
        // 先頭(1) … 隣接(4 5 6) … 末尾(20) の形。
        assert!(text.contains("1 "));
        assert!(text.contains("4 "));
        assert!(text.contains("5 "));
        assert!(text.contains("6 "));
        assert!(text.contains("20 "));
        assert!(text.contains('…'));
    }

    #[test]
    fn pager_line_falls_back_to_page_label_when_total_unknown() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 4,
            total_pages: None,
            has_next: true,
        };
        let line = pager_line(info, &theme);
        assert_eq!(line_text(&line), "‹ page 4 ›");
        // 次ページありなので `›` は活性（非 muted）。
        assert_ne!(
            line.spans.last().expect("next arrow").style.fg,
            Some(theme.muted)
        );
    }

    #[test]
    fn pager_line_arrows_active_when_prev_and_next_available() {
        let theme = Theme::default();
        let info = PageInfo {
            page: 2,
            total_pages: Some(3),
            has_next: true,
        };
        let line = pager_line(info, &theme);
        assert_ne!(
            line.spans.first().expect("prev arrow").style.fg,
            Some(theme.muted)
        );
        assert_ne!(
            line.spans.last().expect("next arrow").style.fg,
            Some(theme.muted)
        );
    }

    #[test]
    fn render_repositories_with_pager_does_not_panic_and_shows_page_label() {
        let mut app = App::new(crate::config::Config::default(), None);
        app.repositories.set_items(vec![]);
        app.repositories_page_info = PageInfo {
            page: 2,
            total_pages: None,
            has_next: false,
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_repositories(frame, area, &mut app);
            })
            .expect("draw succeeds even with empty list");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("page 2"));
    }

    #[test]
    fn render_branches_with_pager_does_not_panic_and_shows_page_label() {
        let mut app = App::new(crate::config::Config::default(), None);
        app.branches.set_items(vec![]);
        app.branches_page_info = PageInfo {
            page: 3,
            total_pages: None,
            has_next: true,
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_branches(frame, area, &mut app);
            })
            .expect("draw succeeds even with empty list");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("page 3"));
    }

    #[test]
    fn short_datetime_formats_iso8601() {
        assert_eq!(short_datetime("2026-07-10T12:34:56Z"), "2026-07-10 12:34");
        assert_eq!(short_datetime("2026-07-10"), "2026-07-10");
    }

    #[test]
    fn hint_entries_are_non_empty_for_every_screen() {
        for screen in [
            Screen::Onboarding,
            Screen::Workspaces,
            Screen::Repositories,
            Screen::PullRequests,
            Screen::PullRequestDetail,
            Screen::Diff,
            Screen::Pipelines,
            Screen::PipelineDetail,
            Screen::StepLog,
            Screen::Branches,
            Screen::Commits,
            Screen::CommitDetail,
            Screen::Source,
            Screen::FileView,
        ] {
            assert!(
                !hint_entries(screen).is_empty(),
                "{screen:?} のヒントが空です"
            );
        }
    }

    /// `Ctrl+K`（ジャンプパレット）/ `?`（ヘルプ）/ `q`（終了）は `on_key` の優先度チェーン上
    /// Onboarding を除く全画面で有効なため、フッターにも必ず出す（実装との食い違い防止）。
    #[test]
    fn hint_entries_include_global_keys_for_every_screen_except_onboarding() {
        for screen in [
            Screen::Workspaces,
            Screen::Repositories,
            Screen::PullRequests,
            Screen::PullRequestDetail,
            Screen::Diff,
            Screen::Pipelines,
            Screen::PipelineDetail,
            Screen::StepLog,
            Screen::Branches,
            Screen::Commits,
            Screen::CommitDetail,
            Screen::Source,
            Screen::FileView,
        ] {
            let keys: Vec<&str> = hint_entries(screen).iter().map(|(key, _)| *key).collect();
            for expected in ["Ctrl+K", "?", "q"] {
                assert!(
                    keys.contains(&expected),
                    "{screen:?} のフッターに {expected} がありません: {keys:?}"
                );
            }
        }
    }

    /// Onboarding は `Ctrl+K`（emacs 風の行末まで削除）と衝突するため、ジャンプパレットの
    /// フッター表示は出さない（実装 [`crate::tui::app::App::on_key_onboarding`] の通り）。
    #[test]
    fn hint_entries_exclude_jump_palette_key_on_onboarding() {
        let keys: Vec<&str> = hint_entries(Screen::Onboarding)
            .iter()
            .map(|(key, _)| *key)
            .collect();
        assert!(!keys.contains(&"Ctrl+K"));
        assert!(!keys.contains(&"?"));
        assert!(!keys.contains(&"q"));
    }

    /// `/`（検索）は Workspaces/Repositories/PullRequests のみ、`S`（並び替え）は
    /// Repositories/PullRequests のみで有効（[`crate::tui::app::App::current_filter_text`] /
    /// [`crate::tui::app::App::cycle_repositories_sort`] / 同 `cycle_pull_requests_sort`）。
    #[test]
    fn hint_entries_search_and_sort_are_scoped_to_list_screens() {
        let search_screens = [
            Screen::Workspaces,
            Screen::Repositories,
            Screen::PullRequests,
        ];
        for screen in search_screens {
            let keys: Vec<&str> = hint_entries(screen).iter().map(|(key, _)| *key).collect();
            assert!(keys.contains(&"/"), "{screen:?} に検索キーがありません");
        }

        let sort_screens = [Screen::Repositories, Screen::PullRequests];
        for screen in sort_screens {
            let keys: Vec<&str> = hint_entries(screen).iter().map(|(key, _)| *key).collect();
            assert!(keys.contains(&"S"), "{screen:?} に並び替えキーがありません");
        }

        // Pipelines/PipelineDetail の `S` は「停止」であり「並び替え」ではないため、
        // hint の説明文が sort 由来ではないことを確認する（食い違い防止）。
        for screen in [Screen::Pipelines, Screen::PipelineDetail] {
            let entries = hint_entries(screen);
            let sort_labeled = entries
                .iter()
                .any(|(key, desc)| *key == "S" && *desc == "並び替え");
            assert!(
                !sort_labeled,
                "{screen:?} の S が誤って並び替えと表示されています"
            );
        }
    }
}

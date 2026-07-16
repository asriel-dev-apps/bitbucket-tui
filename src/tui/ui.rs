//! 各画面の描画。
//!
//! レイアウトは「ヘッダ / 本文 / ステータス行 / キーヒント行」の 4 段構成。ヘルプ・merge 確認
//! モーダル・コメントエディタはオーバーレイ（ポップアップ）で表示する。TUI 実行中に
//! stdout/stderr へ出さないため、ここでの出力はすべて ratatui のバッファ経由。
//!
//! 色は一切ハードコードしない。すべて [`Theme`]（意味役割ベースの配色）経由で決める
//! （`&App`/`&mut App` を受け取る関数は `app.theme` を、純粋関数は `theme: &Theme` 引数を使う）。

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, HighlightSpacing, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui_image::{CropOptions, Resize, StatefulImage};
use unicode_width::UnicodeWidthStr;

use crate::api::{
    Branch, Comment, CommentSide, Commit, DiffStatEntry, MergeStrategy, PageInfo, Pipeline,
    PipelineStatus, PipelineStep, PullRequest, SrcEntry,
};
use crate::tui::app::{
    App, CommentAction, CommentActionHit, CommentEditor, CommentRow, CommentRowKind, ConfirmModal,
    DeleteCommentModal, DetailFocus, DiffFocus, DiffState, DiffViewMode, DisplayRow, HintLayout,
    ImageHit, JumpPaletteState, LinkPalette, ListKind, ListLayout, MergeModal, ModalKind,
    ModalLayout, PageJumpModal, PaneKind, Screen, SelectList, Status, comment_action_labels,
    format_when, now_unix,
};
use crate::tui::diff::{DiffLineKind, FileStatus, ParsedDiff, SidebarRow};
use crate::tui::onboarding::Field;
use crate::tui::richdoc::{self, DocBlock, RichDocument};
use crate::tui::theme::Theme;

/// API token 発行に関する常時ヒント。
const TOKEN_HINT: &str = "API token は Atlassian アカウント設定 > Security の「Create API token with scopes」で発行。必要スコープ: read:user:bitbucket, read:workspace:bitbucket, read:repository:bitbucket, read:pullrequest:bitbucket, write:pullrequest:bitbucket, read:pipeline:bitbucket, write:pipeline:bitbucket";

/// 画面全体を描画する。
pub fn render(frame: &mut Frame, app: &mut App) {
    app.layout = Default::default();
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
        Screen::ImageView => render_image_view(frame, chunks[1], app),
    }
    if app.layout.panes.is_empty() {
        let kind = if app.screen == Screen::ImageView {
            PaneKind::ImageView
        } else {
            PaneKind::Static
        };
        app.layout.panes.push((kind, chunks[1]));
    }

    render_status(frame, chunks[2], &app.status, &app.theme);
    render_hints(frame, chunks[3], app);

    // オーバーレイ（優先度: コメント/merge/確認モーダル → ヘルプ）。
    if let Some(editor) = &app.comment_editor {
        let area = centered_rect(70, 50, frame.area());
        render_comment_editor(frame, area, editor, &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::CommentEditor,
            area,
        });
    }
    if let Some(modal) = &app.delete_comment_modal {
        let area = centered_rect(60, 30, frame.area());
        render_delete_comment_modal(frame, area, modal, &app.theme);
        // モーダルとして登録し、モーダル外のクリック（アクションリンク等）を遮断する。
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::DeleteCommentConfirm,
            area,
        });
    }
    if let Some(modal) = &app.merge_modal {
        let area = centered_rect(60, 55, frame.area());
        render_merge_modal(frame, area, modal, app.current_pr.as_ref(), &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::MergeConfirm,
            area,
        });
    }
    if let Some(modal) = &app.confirm_modal {
        let area = centered_rect(60, 40, frame.area());
        render_confirm_modal(frame, area, modal, &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::PipelineConfirm,
            area,
        });
    }
    if let Some(modal) = &app.page_jump {
        let area = centered_rect(46, 22, frame.area());
        render_page_jump(frame, area, modal, &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::PageJump,
            area,
        });
    }
    if let Some(palette) = app.link_palette.as_mut() {
        let area = centered_rect(74, 64, frame.area());
        render_link_palette(frame, area, palette, &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::LinkPalette,
            area,
        });
        app.layout.lists.push(ListLayout {
            kind: ListKind::LinkPalette,
            area: list_inner(area),
            first_visible: palette.links.state.offset(),
        });
    }
    if app.show_help {
        let area = centered_rect(64, 70, frame.area());
        render_help(frame, area, app.screen, &app.theme);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::Help,
            area,
        });
    }
    // ジャンプパレットは最前面（他のどのオーバーレイより優先して開ける想定のため）。
    let theme = app.theme;
    if let Some(palette) = app.jump_palette.as_mut() {
        let area = centered_rect(70, 60, frame.area());
        render_jump_palette(frame, area, palette, &theme);
        let inner = list_inner(area);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(inner);
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::JumpPalette,
            area,
        });
        app.layout.lists.push(ListLayout {
            kind: ListKind::JumpPalette,
            area: rows[2],
            first_visible: palette.entries.state.offset(),
        });
    }
}

fn list_inner(area: Rect) -> Rect {
    Rect::new(
        area.x.saturating_add(2),
        area.y.saturating_add(1),
        area.width.saturating_sub(4),
        area.height.saturating_sub(2),
    )
}

fn record_list(app: &mut App, kind: ListKind, area: Rect, first_visible: usize) {
    app.layout.lists.push(ListLayout {
        kind,
        area: list_inner(area),
        first_visible,
    });
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
        Screen::ImageView => "画像",
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

/// 通常ペイン用の Block。複数ペイン画面ではキー操作が向いているペイン
/// （PR 詳細は `App::detail_focus` のペイン）を `focused = true` にする。
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

/// 一覧本体とページャ行（下端 1 行）に分割する。5 画面（Workspaces/Repositories/
/// PullRequests/Pipelines/Branches）共通のレイアウト。
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
    record_list(
        app,
        ListKind::Workspaces,
        list_area,
        app.workspaces.state.offset(),
    );
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
    record_list(
        app,
        ListKind::Repositories,
        list_area,
        app.repositories.state.offset(),
    );
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
    record_list(
        app,
        ListKind::PullRequests,
        list_area,
        app.pull_requests.state.offset(),
    );
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

    // 枠線ぶんを差し引いたビューポート高さを保持する。概要の clamp は rich document の
    // 仮想高さを組み立てた後に行う。
    app.detail_viewport = rows[0].height.saturating_sub(2) as usize;

    if app.current_pr.is_some() {
        render_pr_meta_body(frame, rows[0], app, &theme);
    } else {
        render_placeholder(frame, rows[0], &app.status, "PR を選択してください", &theme);
    }

    let bottom =
        Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)]).split(rows[1]);
    app.layout.panes.extend([
        (PaneKind::Overview, rows[0]),
        (PaneKind::ChangedFiles, bottom[0]),
        (PaneKind::Comments, bottom[1]),
    ]);
    render_diffstat_list(
        frame,
        bottom[0],
        &mut app.diffstat,
        app.detail_focus == DetailFocus::Files,
        &theme,
    );
    app.comments_viewport = bottom[1].height.saturating_sub(2) as usize;
    app.clamp_comments_scroll();
    app.comments_rendered_lines = Some(render_comments(
        frame,
        bottom[1],
        &app.comments,
        app.comments_scroll,
        app.detail_focus == DetailFocus::Comments,
        &theme,
    ));
    record_list(
        app,
        ListKind::ChangedFiles,
        bottom[0],
        app.diffstat.state.offset(),
    );
}

/// PR 詳細の概要ペインを、折り返し済みテキストとインライン画像の仮想文書として描画する。
fn render_pr_meta_body(frame: &mut Frame, area: Rect, app: &mut App, theme: &Theme) {
    let focused = app.detail_focus == DetailFocus::Overview;
    let block = themed_block(theme, focused).title(" 概要 ");
    let inner = block.inner(area);
    app.layout.overview_content = Some(inner);
    frame.render_widget(block, area);

    let (leading_lines, body) = {
        let Some(pr) = app.current_pr.as_ref() else {
            return;
        };
        let mut leading = pr_meta_lines(pr, theme);
        let body = match pr.body() {
            Some(body) => body.to_string(),
            None => {
                leading.push(Line::from(Span::styled("（本文なし）", Style::new().dim())));
                String::new()
            }
        };
        (leading, body)
    };

    let document = richdoc::build_document(leading_lines, &body, inner.width, |alt, url| {
        app.overview_image_presentation(alt, url, inner.width)
    });
    app.detail_body_rendered_lines = Some(document.height);
    app.detail_scroll =
        richdoc::clamp_scroll(app.detail_scroll, document.height, app.detail_viewport);
    app.overview_link_positions = document.links.clone();

    render_rich_document(frame, inner, app.detail_scroll, &document, app, theme);
}

/// 現行のタイトル/ブランチ/author/承認パネルをリッチドキュメントの先頭 Text block にする。
fn pr_meta_lines(pr: &PullRequest, theme: &Theme) -> Vec<Line<'static>> {
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
    lines
}

fn render_rich_document(
    frame: &mut Frame,
    area: Rect,
    scroll: u16,
    document: &RichDocument,
    app: &mut App,
    theme: &Theme,
) {
    let viewport_start = usize::from(scroll);
    let viewport_end = viewport_start.saturating_add(usize::from(area.height));
    let mut document_y = 0usize;

    for (index, block) in document.blocks.iter().enumerate() {
        let block_height = document.block_height(index);
        let block_end = document_y.saturating_add(block_height);
        if block_end <= viewport_start {
            document_y = block_end;
            continue;
        }
        if document_y >= viewport_end {
            break;
        }

        let visible_start = document_y.max(viewport_start);
        let visible_end = block_end.min(viewport_end);
        let screen_y = area
            .y
            .saturating_add((visible_start - viewport_start) as u16);
        let visible_height = (visible_end - visible_start) as u16;
        match block {
            DocBlock::Text(lines) => {
                let start = visible_start - document_y;
                let end = start
                    .saturating_add(usize::from(visible_height))
                    .min(lines.len());
                if start < end {
                    let visible = lines[start..end].to_vec();
                    frame.render_widget(
                        Paragraph::new(visible),
                        Rect::new(area.x, screen_y, area.width, visible_height),
                    );
                }
            }
            DocBlock::Image { url, .. } => {
                let Some(presentation) = document.image_presentation(index) else {
                    document_y = block_end;
                    continue;
                };
                match presentation.size {
                    Some(size) => {
                        let image_area =
                            Rect::new(area.x, screen_y, size.width.min(area.width), visible_height);
                        app.layout.overview_images.push(ImageHit {
                            area: image_area,
                            url: url.clone(),
                        });
                        if let Some(protocol) = app.overview_image_protocol_mut(url) {
                            let top_clipped = visible_start > document_y;
                            let resize = if top_clipped {
                                Resize::Crop(Some(CropOptions {
                                    clip_top: true,
                                    clip_left: false,
                                }))
                            } else {
                                Resize::Crop(None)
                            };
                            frame.render_stateful_widget(
                                StatefulImage::default().resize(resize),
                                image_area,
                                protocol,
                            );
                        } else {
                            frame.render_widget(
                                Paragraph::new(Line::from(Span::styled(
                                    presentation.placeholder.clone(),
                                    Style::new().fg(theme.muted),
                                ))),
                                Rect::new(area.x, screen_y, area.width, 1),
                            );
                        }
                    }
                    None => {
                        app.layout.overview_images.push(ImageHit {
                            area: Rect::new(area.x, screen_y, area.width, visible_height.min(1)),
                            url: url.clone(),
                        });
                        frame.render_widget(
                            Paragraph::new(Line::from(Span::styled(
                                presentation.placeholder.clone(),
                                Style::new().fg(theme.muted),
                            ))),
                            Rect::new(area.x, screen_y, area.width, visible_height.min(1)),
                        );
                    }
                }
            }
        }
        document_y = block_end;
    }
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

/// PR 本文・コメント本文の Markdown を [`tui_markdown`] で描画する。
///
/// 画像記法 `![alt](url)` は事前に（コードフェンス内を除いて）`[画像: alt]（i で表示 / o で
/// ブラウザ）` という代替テキストへ置換してから `tui_markdown::from_str` へ渡す（本文中に画像を
/// インライン描画することはできないため。`i` で開く ImageView（`Screen::ImageView`）が実体の
/// 表示を担う。画像表示機能が無効な端末では `i` を押しても Status に案内が出るのみで、この
/// プレースホルダ自体は表記を変えない）。
///
/// `tui_markdown` は入力行数と出力行数が一致しない（見出し前後の空行挿入・ソフト改行の結合等）
/// ため、呼び出し元は返り値の `len()` を実際の描画行数として扱うこと
/// （PR 本文側は [`App::detail_body_rendered_lines`] への書き戻しに使う）。
fn render_markdown_lines(body: &str) -> Vec<Line<'static>> {
    let with_placeholders = replace_image_syntax_outside_code_fences(body);
    convert_markdown_text(&with_placeholders)
}

/// 画像記法をコードフェンス（\`\`\`で囲まれた範囲）の外側だけ [`replace_image_syntax`] で
/// 置換する（コードブロック内の `![...]()` はそのまま保持する）。
fn replace_image_syntax_outside_code_fences(body: &str) -> String {
    let mut in_code_block = false;
    body.lines()
        .map(|line| {
            if line.trim_start().starts_with("```") {
                in_code_block = !in_code_block;
                line.to_string()
            } else if in_code_block {
                line.to_string()
            } else {
                replace_image_syntax(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `tui_markdown::from_str` の出力（`ratatui-core` 版の `Text`）を、本クレートが使う
/// `ratatui`（0.29 系、`ratatui-core` 分離前の独自型）の `Vec<Line<'static>>` へ変換する。
///
/// 両クレートの `Text`/`Line`/`Span`/`Style` はフィールド構成が同一だが型としては別物なので
/// 直接代入できない。`ratatui-core` は本クレートの直接依存ではなく `tui-markdown` 経由の
/// 推移的依存のため型を名指しできず、`Color`/`Modifier` は `Display`/`Binary`（`fmt` 経由の
/// 往復変換）で型名を経由せずに変換する。
fn convert_markdown_text(body: &str) -> Vec<Line<'static>> {
    crate::tui::richdoc::markdown_to_lines(body)
}

/// 画像記法 `![alt](url)` を TUI 向けの代替テキストへ置換する。
///
/// 本文中にインライン描画はしないため、`[画像: alt]（i で表示 / o でブラウザ）` という代替
/// テキストに差し替える（`i` で ImageView を開いて実体を表示できる。詳細は
/// [`crate::tui::imageview`]）。厳密な Markdown 解釈は行わず、`![` `]` `(` `)` の並びのみを見る
/// 簡易版（記法が崩れている場合は元のテキストをそのまま残す）。
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
                result.push_str(&format!("[画像: {alt}]（i で表示 / o でブラウザ）"));
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
    focused: bool,
    theme: &Theme,
) {
    if diffstat.items.is_empty() {
        let block = themed_block(theme, focused).title(" 変更ファイル ");
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
    let list = list_widget_with_focus(theme, items, title, focused);
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

fn render_comments(
    frame: &mut Frame,
    area: Rect,
    comments: &[Comment],
    scroll: u16,
    focused: bool,
    theme: &Theme,
) -> usize {
    let title = format!(" コメント ({}) ", comments.len());
    let block = themed_block(theme, focused).title(title);
    if comments.is_empty() {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "（コメントなし）",
                Style::new().dim(),
            )))
            .block(block),
            area,
        );
        return 1;
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
                format!("  {}", format_when(created, now_unix())),
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
        let body_indent = format!("  {indent}");
        for body_line in render_markdown_lines(comment.raw()) {
            let mut spans = vec![Span::raw(body_indent.clone())];
            spans.extend(body_line.spans);
            lines.push(Line::from(spans).style(body_line.style));
        }
        lines.push(Line::raw(""));
    }
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    // `line_count` は Block の上下枠 2 行を加算して返すため差し引く（概要ペイン側と同じ）。
    let rendered_line_count = paragraph
        .line_count(area.width.saturating_sub(2))
        .saturating_sub(2);
    frame.render_widget(paragraph, area);
    rendered_line_count
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
    let theme = app.theme;
    if app.diff.as_ref().is_none_or(|diff| diff.parsed.is_empty()) {
        render_placeholder(frame, area, &app.status, "差分がありません", &theme);
        return;
    }
    let sidebar_visible = app.diff_sidebar_visible;
    let sidebar_width = app.diff_sidebar_render_width(area.width);
    let Some(diff) = app.diff.as_mut() else {
        render_placeholder(frame, area, &app.status, "差分がありません", &theme);
        return;
    };

    // 左: ファイル一覧サイドバー（`t` で表示/非表示切替、境界のマウスドラッグで幅調整。
    // `App::diff_sidebar_visible`/`diff_sidebar_width`） / 右: 差分本文（Phase1 のキャッシュ＋
    // viewport スライス描画を維持するため、本文の再構築ロジックは `render_diff_body` に
    // 閉じたまま変更しない）。サイドバー非表示中は本文が全幅になる。
    let (sidebar_area, body_area) = if sidebar_visible {
        let cols =
            Layout::horizontal([Constraint::Length(sidebar_width), Constraint::Min(0)]).split(area);
        (Some(cols[0]), cols[1])
    } else {
        (None, area)
    };

    // 枠線ぶんを差し引いたビューポート高さを保持（スクロール上限計算に使う）。
    let viewport = body_area.height.saturating_sub(2) as usize;
    diff.viewport = viewport;
    // スクロール上限はコメント行ぶんの視覚行も勘定した [`DiffState::max_scroll`] に集約する
    // （unified/split・コメントの有無を含めて状態層と同じ計算を使い、二重定義を避ける）。
    let max_scroll = diff.max_scroll();
    if diff.scroll > max_scroll {
        diff.scroll = max_scroll;
    }

    if let Some(sidebar_area) = sidebar_area {
        render_diff_sidebar(frame, sidebar_area, diff, &theme);
    }
    let action_hits = match diff.view_mode {
        DiffViewMode::Unified => render_diff_body(frame, body_area, diff, &theme),
        DiffViewMode::Split => render_diff_body_split(frame, body_area, diff, &theme),
    };
    app.layout.comment_actions = action_hits;

    let Some(sidebar_area) = sidebar_area else {
        app.layout.panes.push((PaneKind::DiffBody, body_area));
        return;
    };
    app.layout.panes.extend([
        (PaneKind::DiffFiles, sidebar_area),
        (PaneKind::DiffBody, body_area),
    ]);
    let visible_height = usize::from(sidebar_area.height.saturating_sub(2)).max(1);
    let first_visible = diff
        .selected_sidebar_row()
        .saturating_add(1)
        .saturating_sub(visible_height);
    app.layout.lists.push(ListLayout {
        kind: ListKind::DiffFiles,
        area: list_inner(sidebar_area),
        first_visible,
    });
}

/// ファイル一覧サイドバー。フォルダ階層のツリー（各ファイルに `+追加 -削除` とコメント数
/// バッジ）を描画し、選択中ファイルの表示行をハイライトする。フォーカス中は枠線を
/// `theme.border_focus` にする。`sidebar_rows` が未構築ならフルパスのフラット一覧へ退避する。
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

    let items: Vec<ListItem> = if diff.sidebar_rows.is_empty() {
        // ツリー未構築時はフルパスのフラット一覧へフォールバック（末尾優先で省略）。
        // 枠線 + 左右パディングぶんを差し引いた表示可能文字数。
        let name_width = area.width.saturating_sub(4);
        diff.parsed
            .files
            .iter()
            .map(|file| {
                ListItem::new(Line::from(Span::raw(truncate_file_name(
                    &file.name, name_width,
                ))))
            })
            .collect()
    } else {
        // 枠線 2 + 左右パディング 2 + 選択記号ぶん 2（`highlight_spacing=Always`）を差し引いた
        // 実表示幅。名前を中略して統計・コメントバッジが右端で切れないようにする。
        let content_width = area.width.saturating_sub(6) as usize;
        diff.sidebar_rows
            .iter()
            .map(|row| sidebar_row_item(row, diff, theme, content_width))
            .collect()
    };
    let item_count = items.len();

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

    let selected = diff
        .selected_sidebar_row()
        .min(item_count.saturating_sub(1));
    let mut state = ListState::default().with_selected(Some(selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// サイドバーのツリー 1 行を [`ListItem`] に整形する。フォルダは淡色 + `name/`、ファイルは
/// `basename  +A -R  🗨N`（値が 0 の要素は省略）。深さぶんインデントし、`content_width` に
/// 収まるよう名前を中略して統計・バッジが右端で切れないようにする。
fn sidebar_row_item(
    row: &SidebarRow,
    diff: &DiffState,
    theme: &Theme,
    content_width: usize,
) -> ListItem<'static> {
    match row {
        SidebarRow::Folder { depth, name } => {
            let indent = "  ".repeat(*depth);
            // ファイル行の状態マーカー（2 桁）と桁を揃えるため、先頭に空マーカー分を空ける。
            // 末尾 `/` の 1 桁ぶんも残して名前を中略する。
            let budget = content_width.saturating_sub(2 + indent.len() + 1).max(1);
            let shown = truncate_middle(name, budget);
            ListItem::new(Line::from(Span::styled(
                format!("  {indent}{shown}/"),
                Style::new().fg(theme.muted).bold(),
            )))
        }
        SidebarRow::File {
            depth,
            file_index,
            name,
        } => {
            let indent = "  ".repeat(*depth);
            // 先頭の状態マーカー（M/A/D/R）。追加=success・削除=danger・リネーム=info・変更=warning。
            let status = diff.parsed.files.get(*file_index).map(|file| file.status);
            let marker = status.map_or_else(
                || Span::raw("  ".to_string()),
                |status| {
                    let color = match status {
                        FileStatus::Added => theme.success,
                        FileStatus::Deleted => theme.danger,
                        FileStatus::Renamed => theme.info,
                        FileStatus::Modified => theme.warning,
                    };
                    Span::styled(format!("{} ", status.marker()), Style::new().fg(color))
                },
            );
            // 統計 + バッジ（右側）を先に組み、その表示幅ぶんを名前から確保する。
            let mut suffix: Vec<Span<'static>> = Vec::new();
            if let Some(file) = diff.parsed.files.get(*file_index) {
                if file.added > 0 {
                    suffix.push(Span::styled(
                        format!("  +{}", file.added),
                        Style::new().fg(theme.success),
                    ));
                }
                if file.removed > 0 {
                    suffix.push(Span::styled(
                        format!(" -{}", file.removed),
                        Style::new().fg(theme.danger),
                    ));
                }
            }
            let count = diff
                .comment_layout
                .file_comment_counts
                .get(*file_index)
                .copied()
                .unwrap_or(0);
            if count > 0 {
                suffix.push(Span::styled(
                    format!("  🗨{count}"),
                    Style::new().fg(theme.info),
                ));
            }
            let suffix_width: usize = suffix
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            let marker_width = UnicodeWidthStr::width(marker.content.as_ref());
            let budget = content_width
                .saturating_sub(indent.len() + marker_width + suffix_width)
                .max(1);
            let shown = truncate_middle(name, budget);
            let mut spans = vec![
                marker,
                Span::styled(format!("{indent}{shown}"), Style::new().fg(theme.fg)),
            ];
            spans.extend(suffix);
            ListItem::new(Line::from(spans))
        }
    }
}

/// 表示幅 `budget`（列数）に収まるよう文字列を中略する。頭と尾を残し、中央に `…` を置く
/// （ファイル名の拡張子側と識別子側の両方を残すため）。既に収まるならそのまま返す。
fn truncate_middle(text: &str, budget: usize) -> String {
    if UnicodeWidthStr::width(text) <= budget {
        return text.to_string();
    }
    if budget <= 1 {
        return "…".to_string();
    }
    // `…` の 1 桁を除いた残りを頭と尾に割り振る（列幅は近似として文字数で数える）。
    let keep = budget - 1;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= keep {
        // 文字数では収まる（全角で列幅超過のケース）。頭尾が重ならないよう clip に委ねる。
        return text.to_string();
    }
    let head_len = keep.div_ceil(2);
    let tail_len = keep - head_len;
    let head: String = chars.iter().take(head_len).collect();
    let tail: String = chars[chars.len() - tail_len..].iter().collect();
    format!("{head}…{tail}")
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

fn render_diff_body(
    frame: &mut Frame,
    area: Rect,
    diff: &mut DiffState,
    theme: &Theme,
) -> Vec<CommentActionHit> {
    // 着色済み diff 行は diff ロード時・テーマ切替時に一度だけ構築して使い回す（`parsed.lines`
    // と同じ並び・同じ長さ）。コメント行は `display_rows` として畳み込み、`scroll`/`cursor` は
    // その表示行の添字を指す。per-frame コストは O(viewport)。
    let total = diff.parsed.len();
    let position = diff_cursor_position_label(diff);
    let title = format!(" diff {} ({total} 行){position} ", diff.title);
    if diff.rendered_lines.is_none() {
        diff.rendered_lines = Some(
            diff.parsed
                .lines
                .iter()
                .map(|line| {
                    Line::from(Span::styled(
                        line.text.clone(),
                        diff_line_style(theme, line.kind),
                    ))
                })
                .collect(),
        );
    }
    let focused = diff.focus == DiffFocus::Body;
    let viewport = diff.viewport.max(1);
    let inner_width = area.width.saturating_sub(2) as usize;
    let focused_comment = diff.cursor_comment().map(|(_, comment_id)| comment_id);
    let focused_thread = diff.cursor_comment().map(|(thread_root, _)| thread_root);
    let now = now_unix();
    let mut visible: Vec<Line> = Vec::with_capacity(viewport);
    let mut hits: Vec<CommentActionHit> = Vec::new();
    let Some(lines) = diff.rendered_lines.as_ref() else {
        return hits;
    };

    if diff.display_rows.is_empty() {
        // フォールバック（コメント無し・未構築）: diff 行と 1:1。
        let (start, end) = diff_visible_range(diff.scroll, viewport, lines.len());
        for (offset, line) in lines[start..end].iter().enumerate() {
            let i = start + offset;
            visible.push(if i == diff.cursor {
                highlighted_diff_line(line, theme)
            } else {
                line.clone()
            });
        }
    } else {
        for offset in 0..viewport {
            let index = diff.scroll + offset;
            let Some(row) = diff.display_rows.get(index) else {
                break;
            };
            match row {
                DisplayRow::Diff(i) => {
                    let line = lines.get(*i).cloned().unwrap_or_default();
                    visible.push(if index == diff.cursor {
                        highlighted_diff_line(&line, theme)
                    } else {
                        line
                    });
                }
                DisplayRow::Comment(comment_row) => {
                    visible.push(comment_box_line(
                        comment_row,
                        inner_width,
                        theme,
                        focused_comment,
                        focused_thread,
                        now,
                    ));
                    // アクション行はクリック用ヒットボックスも収集する（内容開始列 =
                    // 枠線 1 + パディング 1 + ボックス左枠 "│ " 2 = area.x + 4）。
                    collect_action_hits(
                        &mut hits,
                        comment_row,
                        theme,
                        inner_width,
                        area.x.saturating_add(4),
                        area.y.saturating_add(1).saturating_add(offset as u16),
                    );
                }
            }
        }
    }

    let paragraph = Paragraph::new(visible).block(themed_block(theme, focused).title(title));
    frame.render_widget(paragraph, area);
    hits
}

/// コメントボックスの 1 行を整形する。枠（`┌─┐│└─┘`）で囲む。カーソルが乗っているスレッドは
/// 枠色をアクセントにし、選択中コメント（ヘッダ/本文）は背景でハイライトする。返信は 1 段インデント。
fn comment_box_line(
    row: &CommentRow,
    inner_width: usize,
    theme: &Theme,
    focused_comment: Option<u64>,
    focused_thread: Option<u64>,
    now: i64,
) -> Line<'static> {
    let width = inner_width.max(4);
    // 選択中コメントは背景ハイライト、選択中スレッドは枠全体をアクセント色にする。
    let hot = row.comment_id.is_some() && row.comment_id == focused_comment;
    let thread_focused = focused_thread == Some(row.thread_root);
    let border = Style::new().fg(if thread_focused {
        theme.accent
    } else {
        theme.info
    });
    match &row.kind {
        CommentRowKind::Top => {
            let mid = "─".repeat(width.saturating_sub(2));
            Line::from(Span::styled(format!("┌{mid}┐"), border))
        }
        CommentRowKind::Bottom => {
            let mid = "─".repeat(width.saturating_sub(2));
            Line::from(Span::styled(format!("└{mid}┘"), border))
        }
        CommentRowKind::Header {
            reply,
            author,
            when,
            resolved,
        } => {
            let budget = width.saturating_sub(4);
            let marker = if *reply { "↳ " } else { "🗨 " };
            // 枠がずれないよう、固定部（marker/when/解決）を差し引いて著者名をクリップする。
            // `when` は生の created_on。相対時刻はこの場（毎フレーム）で整形する。
            let when_part = if when.is_empty() {
                String::new()
            } else {
                format!("  {}", format_when(when, now))
            };
            let resolved_part = if *resolved { "  ✓解決" } else { "" };
            let fixed = UnicodeWidthStr::width(marker)
                + UnicodeWidthStr::width(when_part.as_str())
                + UnicodeWidthStr::width(resolved_part);
            let author_shown = clip_to_width(author, budget.saturating_sub(fixed).max(1));
            let mut content: Vec<Span<'static>> = vec![
                Span::styled(marker.to_string(), Style::new().fg(theme.info)),
                Span::styled(author_shown, Style::new().fg(theme.accent).bold()),
            ];
            if !when_part.is_empty() {
                content.push(Span::styled(when_part, Style::new().dim()));
            }
            if !resolved_part.is_empty() {
                content.push(Span::styled(
                    resolved_part.to_string(),
                    Style::new().fg(theme.success),
                ));
            }
            comment_box_content_line(content, budget, border, hot, theme)
        }
        CommentRowKind::Actions {
            reply,
            root,
            mine,
            resolved,
        } => {
            let budget = width.saturating_sub(4);
            let (line, _) = comment_actions_line(*reply, *root, *mine, *resolved, theme);
            let content_width: usize = line
                .spans
                .iter()
                .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
                .sum();
            let mut content = line.spans;
            // 幅超過時はリンクを丸ごと落とさず clip（稀: 極端に狭いペイン）。
            if content_width > budget {
                let flat: String = content.iter().map(|span| span.content.as_ref()).collect();
                content = vec![Span::styled(
                    clip_to_width(&flat, budget),
                    Style::new().fg(theme.info),
                )];
            }
            comment_box_content_line(content, budget, border, hot, theme)
        }
        CommentRowKind::Collapsed {
            author,
            count,
            resolved,
        } => {
            // 折りたたみ 1 行表示（Enter/クリックで展開）。枠は付けない。
            let text = if *resolved {
                format!("▸ ✓ {author} resolved this thread ({count})")
            } else {
                format!("▸ 🗨 {author} · thread ({count})")
            };
            let shown = clip_to_width(&text, width.max(1));
            let style = if *resolved {
                Style::new().fg(theme.success)
            } else {
                Style::new().fg(theme.muted)
            };
            let line = Line::from(Span::styled(shown, style));
            if hot {
                line.style(Style::new().bg(theme.selection_bg))
            } else {
                line
            }
        }
        CommentRowKind::Body { reply, text } => {
            let budget = width.saturating_sub(4);
            let indent = if *reply { "  " } else { "" };
            let shown = clip_to_width(&format!("{indent}{text}"), budget);
            let content = vec![Span::styled(shown, Style::new().fg(theme.fg))];
            comment_box_content_line(content, budget, border, hot, theme)
        }
    }
}

/// アクションリンク行の内容（`Reply · Resolve · Edit · Delete`）と、各リンクの
/// 「内容先頭からの列オフセット・表示幅」を組む（描画とクリック判定で共有する）。
fn comment_actions_line(
    reply: bool,
    root: bool,
    mine: bool,
    resolved: bool,
    theme: &Theme,
) -> (Line<'static>, Vec<(CommentAction, u16, u16)>) {
    let indent = if reply { "    " } else { "  " };
    let mut spans: Vec<Span<'static>> = vec![Span::raw(indent.to_string())];
    let mut segments = Vec::new();
    let mut offset = UnicodeWidthStr::width(indent) as u16;
    for (i, (action, label)) in comment_action_labels(root, mine, resolved)
        .into_iter()
        .enumerate()
    {
        if i > 0 {
            let sep = " · ";
            spans.push(Span::styled(sep.to_string(), Style::new().fg(theme.muted)));
            offset += UnicodeWidthStr::width(sep) as u16;
        }
        let label_width = UnicodeWidthStr::width(label) as u16;
        segments.push((action, offset, label_width));
        spans.push(Span::styled(
            label.to_string(),
            Style::new().fg(theme.info).underlined(),
        ));
        offset += label_width;
    }
    (Line::from(spans), segments)
}

/// アクションリンクのヒットボックスを収集する（`base_x` = ボックス内容の開始列、`y` = 行の
/// 絶対行）。内容が枠幅を超える場合はリンク位置が clip とずれるため収集しない。
fn collect_action_hits(
    hits: &mut Vec<CommentActionHit>,
    row: &CommentRow,
    theme: &Theme,
    inner_width: usize,
    base_x: u16,
    y: u16,
) {
    let CommentRowKind::Actions {
        reply,
        root,
        mine,
        resolved,
    } = &row.kind
    else {
        return;
    };
    let Some(comment_id) = row.comment_id else {
        return;
    };
    let budget = inner_width.max(4).saturating_sub(4);
    let (line, segments) = comment_actions_line(*reply, *root, *mine, *resolved, theme);
    let content_width: usize = line
        .spans
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    if content_width > budget {
        return;
    }
    for (action, offset, width) in segments {
        hits.push(CommentActionHit {
            area: Rect::new(base_x + offset, y, width, 1),
            action,
            comment_id,
            thread_root: row.thread_root,
        });
    }
}

/// コメントボックスの内容行（`│ … │`）を、幅 `budget` に合わせてパディングして組む。
fn comment_box_content_line(
    content: Vec<Span<'static>>,
    budget: usize,
    border: Style,
    hot: bool,
    theme: &Theme,
) -> Line<'static> {
    let content_width: usize = content
        .iter()
        .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
        .sum();
    let pad = budget.saturating_sub(content_width);
    let mut spans = vec![Span::styled("│ ".to_string(), border)];
    spans.extend(content);
    if pad > 0 {
        spans.push(Span::raw(" ".repeat(pad)));
    }
    spans.push(Span::styled(" │".to_string(), border));
    let line = Line::from(spans);
    if hot {
        line.style(Style::new().bg(theme.selection_bg))
    } else {
        line
    }
}

/// 文字列を表示幅 `budget` 列に収める（超過は末尾を落として `…` を付す）。
fn clip_to_width(text: &str, budget: usize) -> String {
    if UnicodeWidthStr::width(text) <= budget {
        return text.to_string();
    }
    let limit = budget.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let char_width = UnicodeWidthStr::width(ch.to_string().as_str());
        if width + char_width > limit {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    out
}

/// split 表示（左=旧ファイル/右=新ファイル）の本文を描画する。
///
/// `render_diff_body`（unified）と同じ Phase1 の性能方針を踏襲する: 着色済み行ペアは
/// `DiffState::rendered_split` に一度だけ構築してキャッシュし（diff ロード時・テーマ切替時
/// のみ無効化）、毎フレームは可視範囲（viewport 分）だけを切り出して描画する
/// （O(viewport)。全行を Paragraph へ渡さない）。長い行はそれぞれのペイン幅で自動的に
/// クリップされる（`Paragraph` は `.wrap()` を付けない限り折り返さず、area 幅を超えた
/// 分は単純に描画されない。水平スクロールが不要な今回はこれで十分）。
fn render_diff_body_split(
    frame: &mut Frame,
    area: Rect,
    diff: &mut DiffState,
    theme: &Theme,
) -> Vec<CommentActionHit> {
    let total = diff.parsed.split_lines.len();
    let position = diff_cursor_position_label(diff);
    let suffix = format!(" ({total} 行){position} ");

    if diff.rendered_split.is_none() {
        diff.rendered_split = Some(build_split_rendered_lines(&diff.parsed, theme));
    }
    let focused = diff.focus == DiffFocus::Body;
    let viewport = diff.viewport.max(1);
    let cols =
        Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]).split(area);
    // コメントは新側（右）カラムに枠付きで差し込む（ユーザー指定）。
    let right_inner = cols[1].width.saturating_sub(2) as usize;
    let focused_comment = diff.cursor_comment().map(|(_, comment_id)| comment_id);
    let focused_thread = diff.cursor_comment().map(|(thread_root, _)| thread_root);
    let now = now_unix();
    let mut visible_left: Vec<Line> = Vec::with_capacity(viewport);
    let mut visible_right: Vec<Line> = Vec::with_capacity(viewport);
    let mut hits: Vec<CommentActionHit> = Vec::new();
    let Some(rows) = diff.rendered_split.as_ref() else {
        return hits;
    };

    if diff.display_rows.is_empty() {
        let (start, end) = diff_visible_range(diff.scroll, viewport, rows.len());
        for (offset, (left, right)) in rows[start..end].iter().enumerate() {
            if start + offset == diff.cursor {
                visible_left.push(highlighted_diff_line(left, theme));
                visible_right.push(highlighted_diff_line(right, theme));
            } else {
                visible_left.push(left.clone());
                visible_right.push(right.clone());
            }
        }
    } else {
        for offset in 0..viewport {
            let index = diff.scroll + offset;
            let Some(row) = diff.display_rows.get(index) else {
                break;
            };
            match row {
                DisplayRow::Diff(i) => {
                    let (left, right) = rows.get(*i).cloned().unwrap_or_default();
                    if index == diff.cursor {
                        visible_left.push(highlighted_diff_line(&left, theme));
                        visible_right.push(highlighted_diff_line(&right, theme));
                    } else {
                        visible_left.push(left);
                        visible_right.push(right);
                    }
                }
                DisplayRow::Comment(comment_row) => {
                    visible_left.push(Line::raw(""));
                    visible_right.push(comment_box_line(
                        comment_row,
                        right_inner,
                        theme,
                        focused_comment,
                        focused_thread,
                        now,
                    ));
                    // アクション行のクリック用ヒットボックス（右カラム内容開始列 = 右カラム
                    // 枠線 1 + パディング 1 + ボックス左枠 2 = cols[1].x + 4）。
                    collect_action_hits(
                        &mut hits,
                        comment_row,
                        theme,
                        right_inner,
                        cols[1].x.saturating_add(4),
                        cols[1].y.saturating_add(1).saturating_add(offset as u16),
                    );
                }
            }
        }
    }

    let left_paragraph = Paragraph::new(visible_left)
        .block(themed_block(theme, focused).title(format!(" diff {} 旧{suffix}", diff.title)));
    let right_paragraph = Paragraph::new(visible_right)
        .block(themed_block(theme, focused).title(format!(" diff {} 新{suffix}", diff.title)));
    frame.render_widget(left_paragraph, cols[0]);
    frame.render_widget(right_paragraph, cols[1]);
    hits
}

/// [`DiffState::rendered_split`] キャッシュの中身を構築する（diff ロード時・テーマ切替時に
/// 一度だけ呼ばれる）。`parsed.split_lines` と同じ並び・同じ長さの `(左行, 右行)` を返す。
fn build_split_rendered_lines(
    parsed: &ParsedDiff,
    theme: &Theme,
) -> Vec<(Line<'static>, Line<'static>)> {
    parsed
        .split_lines
        .iter()
        .map(|row| {
            (
                split_pane_line(parsed, row.left, true, theme),
                split_pane_line(parsed, row.right, false, theme),
            )
        })
        .collect()
}

/// split 表示 1 セル分の着色済み `Line` を作る。
///
/// `index` が `None`（対応する unified 行が無い filler）なら空行を返す。`use_old` で行番号
/// ガターに `old_no`（左=旧ファイル側）/`new_no`（右=新ファイル側）のどちらを出すかを選ぶ。
fn split_pane_line(
    parsed: &ParsedDiff,
    index: Option<usize>,
    use_old: bool,
    theme: &Theme,
) -> Line<'static> {
    let Some(line) = index.and_then(|index| parsed.lines.get(index)) else {
        return Line::from("");
    };
    let number = if use_old { line.old_no } else { line.new_no };
    let gutter = match number {
        Some(no) => format!("{no:>4} "),
        None => "     ".to_string(),
    };
    Line::from(Span::styled(
        format!("{gutter}{}", line.text),
        diff_line_style(theme, line.kind),
    ))
}

/// 現在行を選択色（`theme.selection_bg`/`selection_fg`）で上書きした複製の `Line` を返す。
/// 元の `Line`（キャッシュ由来）は変更しない。
fn highlighted_diff_line(line: &Line, theme: &Theme) -> Line<'static> {
    let style = Style::new().bg(theme.selection_bg).fg(theme.selection_fg);
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|span| Span::styled(span.content.to_string(), style))
        .collect();
    Line::from(spans)
}

/// 現在行（`diff.cursor`）の位置表示（` パス:行番号 (新/旧)`）をタイトルへ差し込む断片。
///
/// コメント不可（メタ/ヘッダ/ハンク行）の場合はファイルパスのみ、ファイル境界すら
/// 判定できない場合（空 diff 等）は空文字を返す。先頭に区切り用の半角スペースを含む。
/// `diff.view_mode` に応じて unified/split いずれかの規則で解決する
/// （[`DiffState::current_file`]/[`DiffState::current_comment_anchor`]）。
fn diff_cursor_position_label(diff: &DiffState) -> String {
    let Some(path) = diff.current_file() else {
        return String::new();
    };
    match diff.current_comment_anchor() {
        Some(anchor) => {
            let side = match anchor.side {
                CommentSide::To => "新",
                CommentSide::From => "旧",
            };
            format!(" {path}:{} ({side})", anchor.line)
        }
        None => format!(" {path}"),
    }
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
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.pipelines.items.is_empty() {
        render_placeholder(
            frame,
            list_area,
            &app.status,
            "パイプラインがありません",
            &theme,
        );
    } else {
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
        frame.render_stateful_widget(list, list_area, &mut app.pipelines.state);
    }

    render_pager(frame, pager_area, app.pipelines_page_info, &theme);
    record_list(
        app,
        ListKind::Pipelines,
        list_area,
        app.pipelines.state.offset(),
    );
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
    app.layout
        .panes
        .extend([(PaneKind::Static, rows[0]), (PaneKind::Static, rows[1])]);
    record_list(
        app,
        ListKind::PipelineSteps,
        rows[1],
        app.pipeline_steps.state.offset(),
    );
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
    app.layout.panes.push((PaneKind::StepLog, area));
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
    record_list(
        app,
        ListKind::Branches,
        list_area,
        app.branches.state.offset(),
    );
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
    let (list_area, pager_area) = split_list_and_pager(area);

    if app.commits.items.is_empty() {
        render_placeholder(
            frame,
            list_area,
            &app.status,
            "コミットがありません",
            &theme,
        );
    } else {
        let items: Vec<ListItem> = app
            .commits
            .items
            .iter()
            .map(|commit| ListItem::new(commit_row(commit, &theme)))
            .collect();
        let revision = app.commits_revision.as_deref().unwrap_or("既定ブランチ");
        let title = format!(" コミット [{revision}] ({}) ", app.commits.items.len());
        let list = list_widget(&theme, items, title);
        frame.render_stateful_widget(list, list_area, &mut app.commits.state);
    }

    render_pager(frame, pager_area, app.commits_page_info(), &theme);
    record_list(
        app,
        ListKind::Commits,
        list_area,
        app.commits.state.offset(),
    );
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
    app.layout.panes.push((PaneKind::Static, area));
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
    let offset = source.entries.state.offset();
    record_list(app, ListKind::Source, area, offset);
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
    app.layout.panes.push((PaneKind::FileView, area));
}

/// PR 本文内の画像を表示する（`i` で開く ImageView）。
///
/// `ratatui_image`（8.1.1）のネイティブ画像プロトコル（`StatefulImage` + `StatefulProtocol`）で
/// 描画する。端末が Sixel/Kitty/iTerm2 のいずれにも対応しなければ `ratatui-image` 自身が内蔵の
/// ハーフブロック描画へ自動フォールバックする。`StatefulImage`（既定 `Resize::Fit`）は毎フレーム
/// 現在の描画エリアに合わせて再エンコードするため、端末リサイズに追従する。
fn render_image_view(frame: &mut Frame, area: Rect, app: &mut App) {
    let theme = app.theme;
    let Some(current) = app.image_refs.get(app.image_index) else {
        render_placeholder(frame, area, &app.status, "画像がありません", &theme);
        return;
    };
    let alt_suffix = if current.alt.is_empty() {
        String::new()
    } else {
        format!(" {}", current.alt)
    };
    let title = format!(
        " 画像 {}/{}{alt_suffix} ",
        app.image_index + 1,
        app.image_refs.len(),
    );

    match &app.current_image {
        None => {
            let text = if matches!(app.status, Status::Loading(_)) {
                "読み込み中…"
            } else {
                "画像を準備しています…"
            };
            let paragraph = Paragraph::new(Line::from(Span::styled(text, Style::new().dim())))
                .block(themed_block(&theme, true).title(title))
                .alignment(Alignment::Center);
            frame.render_widget(paragraph, area);
        }
        Some(Err(message)) => {
            let paragraph = Paragraph::new(Line::from(Span::styled(
                message.clone(),
                Style::new().fg(theme.danger),
            )))
            .block(themed_block(&theme, true).title(title))
            .alignment(Alignment::Center)
            .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        }
        Some(Ok(_)) => {
            let block = themed_block(&theme, true).title(title);
            let inner_area = block.inner(area);
            frame.render_widget(block, area);
            match app.image_protocol.as_mut() {
                Some(protocol) => {
                    frame.render_stateful_widget(StatefulImage::default(), inner_area, protocol);
                }
                None => {
                    // デコードは成功したが描画用 protocol が無い状態（`image_picker` が
                    // `None`＝起動時の端末検出失敗）。`open_image_view` で事前にガードしている
                    // ため通常は到達しないが、念のため案内を出す（アプリは落ちない）。
                    let paragraph = Paragraph::new(Line::from(Span::styled(
                        "この端末は画像表示に未対応です",
                        Style::new().dim(),
                    )))
                    .alignment(Alignment::Center);
                    frame.render_widget(paragraph, inner_area);
                }
            }
        }
    }
}

/// ISO8601 文字列を `YYYY-MM-DD HH:MM` へ短縮する（`T` を空白に）。
fn short_datetime(value: &str) -> String {
    let truncated: String = value.chars().take(16).collect();
    truncated.replacen('T', " ", 1)
}

/// コメント削除の確認モーダル（`d`）。破壊的なので枠線は危険色。
fn render_delete_comment_modal(
    frame: &mut Frame,
    area: Rect,
    modal: &DeleteCommentModal,
    theme: &Theme,
) {
    frame.render_widget(Clear, area);
    let mut lines = vec![
        Line::from(Span::styled(
            "このコメントを削除しますか？（取り消せません）",
            Style::new().fg(theme.danger).bold(),
        )),
        Line::raw(""),
    ];
    if modal.submitting {
        lines.push(Line::from(Span::styled(
            "削除中…",
            Style::new().fg(theme.warning),
        )));
    }
    lines.push(Line::from(Span::styled(
        "Enter/y: 削除   Esc/n: 取消",
        Style::new().dim(),
    )));
    let block = rounded_block(theme, theme.danger)
        .title(" コメント削除の確認 ")
        .style(Style::new().bg(theme.bg));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_confirm_modal(frame: &mut Frame, area: Rect, modal: &ConfirmModal, theme: &Theme) {
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

/// ページ番号ジャンプ（`g`）の入力プロンプトを描画する。数字入力 + `Enter` で移動、`Esc` で取消。
fn render_page_jump(frame: &mut Frame, area: Rect, modal: &PageJumpModal, theme: &Theme) {
    frame.render_widget(Clear, area);

    // 入力中のカーソルを反転ブロックで示す（未入力でも位置が分かるように）。
    let lines = vec![
        Line::from(vec![
            Span::styled("ページ番号: ", Style::new().fg(theme.accent).bold()),
            Span::styled(modal.input.clone(), Style::new().fg(theme.fg)),
            Span::styled(" ", Style::new().bg(theme.accent)),
        ]),
        Line::raw(""),
        Line::from(Span::styled("Enter: 移動   Esc: 取消", Style::new().dim())),
    ];

    let block = rounded_block(theme, theme.border_focus)
        .title(" ページ番号ジャンプ (g) ")
        .style(Style::new().bg(theme.bg));
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn render_link_palette(frame: &mut Frame, area: Rect, palette: &mut LinkPalette, theme: &Theme) {
    frame.render_widget(Clear, area);
    let items = palette
        .links
        .items
        .iter()
        .map(|link| {
            let domain = link
                .url
                .split_once("://")
                .map(|(_, rest)| rest.split('/').next().unwrap_or(rest))
                .unwrap_or("");
            ListItem::new(Line::from(vec![
                Span::raw(link.label.clone()),
                Span::styled(format!("  {domain}"), Style::new().fg(theme.muted)),
            ]))
        })
        .collect();
    let list = list_widget_with_focus(theme, items, " リンク (L) ".to_string(), true);
    frame.render_stateful_widget(list, area, &mut palette.links.state);
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
    list_widget_with_focus(theme, items, title, true)
}

fn list_widget_with_focus<'a>(
    theme: &Theme,
    items: Vec<ListItem<'a>>,
    title: String,
    focused: bool,
) -> List<'a> {
    List::new(items)
        .block(themed_block(theme, focused).title(title))
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
            ("Tab", "ペイン移動"),
            ("↑↓/jk", "フォーカス先を移動"),
            ("Shift+J/K", "10行/10件"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("d", "Diff"),
            ("c", "コメント"),
            ("a", "承認"),
            ("x", "変更要求"),
            ("M", "マージ"),
            ("o", "ブラウザで開く"),
            ("i", "画像を表示"),
            ("L", "リンク"),
            ("Esc", "戻る"),
        ],
        Screen::Diff => vec![
            ("Tab", "一覧/本文"),
            ("↑↓/jk", "選択/現在行移動"),
            ("Shift+J/K", "10行"),
            ("n/N", "ファイル境界"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("v", "表示切替(unified/split)"),
            ("t", "ファイル一覧 表示/非表示"),
            ("c", "コメント"),
            ("r", "返信"),
            ("e", "編集"),
            ("d", "削除"),
            ("R", "解決"),
            ("Enter", "折りたたみ"),
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
            ("[/]", "前/次ページ"),
            ("g", "ページ番号ジャンプ"),
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
            ("[/]", "前/次ページ"),
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
            ("Backspace/Esc", "親へ（ルートは戻る）"),
            ("r", "再読込"),
        ],
        Screen::FileView => vec![
            ("↑↓/jk", "スクロール"),
            ("Shift+J/K", "10行"),
            ("PgUp/PgDn", "1画面"),
            ("g/G", "先頭/末尾"),
            ("Esc", "戻る"),
        ],
        Screen::ImageView => vec![("←→/np", "前/次の画像"), ("Esc", "戻る")],
    };

    if screen != Screen::Onboarding {
        entries.push(("Ctrl+K", "ジャンプ"));
        entries.push(("?", "ヘルプ"));
        entries.push(("q", "終了"));
    }

    entries
}

fn render_hints(frame: &mut Frame, area: Rect, app: &mut App) {
    let screen = app.screen;
    let theme = app.theme;
    let mut spans = Vec::new();
    let mut x = area.x;
    for (index, (key, description)) in hint_entries(screen).iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw("  "));
            x = x.saturating_add(2);
        }
        let item_width = UnicodeWidthStr::width(format!("{key} {description}").as_str())
            .min(usize::from(u16::MAX)) as u16;
        let visible_width = item_width.min(area.x.saturating_add(area.width).saturating_sub(x));
        if visible_width > 0 {
            app.layout.hints.push(HintLayout {
                area: Rect::new(x, area.y, visible_width, area.height),
                key: hint_key(key),
            });
        }
        x = x.saturating_add(item_width);
        spans.push(Span::styled(*key, Style::new().fg(theme.accent)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(*description, Style::new().fg(theme.muted)));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn hint_key(label: &str) -> KeyEvent {
    let (code, modifiers) = match label {
        value if value.starts_with("Ctrl+C") => (KeyCode::Char('c'), KeyModifiers::CONTROL),
        value if value.starts_with("Ctrl+K") => (KeyCode::Char('k'), KeyModifiers::CONTROL),
        value if value.starts_with("Shift+J") => (KeyCode::Char('J'), KeyModifiers::SHIFT),
        value if value.starts_with("Tab") => (KeyCode::Tab, KeyModifiers::NONE),
        value if value.starts_with("Enter") => (KeyCode::Enter, KeyModifiers::NONE),
        value if value.starts_with("Esc") || value.starts_with("Backspace/Esc") => {
            (KeyCode::Esc, KeyModifiers::NONE)
        }
        value if value.starts_with("PgUp") => (KeyCode::PageDown, KeyModifiers::NONE),
        value if value.starts_with("↑↓") => (KeyCode::Down, KeyModifiers::NONE),
        value if value.starts_with("←→") => (KeyCode::Right, KeyModifiers::NONE),
        value if value.starts_with("[/]") => (KeyCode::Char(']'), KeyModifiers::NONE),
        value if value.starts_with("g/G") => (KeyCode::Char('G'), KeyModifiers::SHIFT),
        value if value.starts_with("n/N") => (KeyCode::Char('n'), KeyModifiers::NONE),
        _ => (
            KeyCode::Char(label.chars().next().unwrap_or('?')),
            KeyModifiers::NONE,
        ),
    };
    KeyEvent::new(code, modifiers)
}

fn render_comment_editor(frame: &mut Frame, area: Rect, editor: &CommentEditor, theme: &Theme) {
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

    // 編集/返信/インライン/一般コメントで見分けが付くようにタイトルを出し分ける。
    // 非破壊的な入力オーバーレイなのでフォーカス色の枠線。
    let title = if editor.editing.is_some() {
        " コメントを編集 ".to_string()
    } else if editor.reply_to.is_some() {
        " スレッドに返信 ".to_string()
    } else {
        match &editor.inline {
            Some(anchor) => format!(" {}:{} にコメント ", anchor.path, anchor.line),
            None => " コメントを書く ".to_string(),
        }
    };
    let block = rounded_block(theme, theme.border_focus)
        .title(title)
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
    area: Rect,
    modal: &MergeModal,
    pr: Option<&PullRequest>,
    theme: &Theme,
) {
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

fn render_help(frame: &mut Frame, area: Rect, screen: Screen, theme: &Theme) {
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
        Line::raw("マウス         ホイールはカーソル下を3行/3件移動。クリックはペイン/行/リンク/"),
        Line::raw("               画像/ヒントを操作（選択済み行の再クリックは Enter 相当）"),
        Line::raw("Shift+ドラッグ 端末本来のテキスト選択（マウスキャプチャ中）"),
        Line::raw("               多重化端末ではマウスイベントのパススルー実装に依存"),
        Line::raw("確認モーダル   外側クリックは取消。内側クリックでは決定せず Enter のみ実行"),
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
            "Tab / Shift+Tab ペイン移動（概要 → 変更ファイル → コメント）",
            "↑↓ / j k       フォーカス先を 1 行 / 1 件移動",
            "Shift+J / K    フォーカス先を 10 行 / 10 件移動",
            "PgUp/PgDn      概要・コメントを 1 画面スクロール",
            "g / G          フォーカス先の先頭 / 末尾へ",
            "d              Diff を開く",
            "c              コメント投稿（Enter 改行 / Ctrl+S 送信 / Esc 取消）",
            "a              approve / unapprove トグル",
            "x              request-changes / 取消 トグル",
            "M              マージ（確認モーダル: ←→/Tab 戦略切替, Space ブランチ削除切替,",
            "               Enter 実行, Esc 取消）",
            "o              ブラウザで開く（`open` コマンドで既定ブラウザに開く）",
            "i              本文の画像を表示（画像が無い/端末が未対応なら Status に案内）",
            "L              本文・コメントのリンク一覧（Enter でブラウザ、Esc で閉じる）",
        ],
        Screen::Diff => &[
            "Tab            ファイル一覧 / 本文フォーカス切替",
            "↑↓ / j k       (一覧) ファイル選択  /  (本文) 現在行を 1 行移動",
            "Shift+J / K    現在行を 10 行移動",
            "PgUp/PgDn / f/b 現在行を 1 画面ぶん移動",
            "g / Home, G / End 現在行を先頭 / 末尾へ",
            "n / N          次 / 前のファイル境界へ（現在行もその先頭へ、フォーカス問わず）",
            "v              表示モード切替（unified ⇔ split。設定に永続化）",
            "t              ファイル一覧 表示/非表示（境界のドラッグでも幅調整可。設定に永続化）",
            "c              現在行にインラインコメント投稿（PR 差分のみ。Ctrl+S 送信 / Esc 取消）",
            "↑↓             コメント行にもカーソルが乗り、コメントを選択できる",
            "r / e / d / R  選択中コメントに 返信 / 編集 / 削除 / 解決トグル（PR 差分のみ。",
            "               編集/削除は自分のコメントのみ。Reply 等のリンクはクリックでも動作）",
            "Enter          スレッドの折りたたみ/展開（解決済みは自動で折りたたみ）",
            "               コミット差分ではコメント操作できません",
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
            "r              一覧を再読込（現在ページ）",
            "a              自動更新の ON/OFF",
            "S              停止（進行中のみ・確認モーダル: Enter 実行 / Esc 取消）",
            "R              再実行（確認モーダル: Enter 実行 / Esc 取消）",
            "Shift+J / K    10 件下 / 上へ移動",
            "[ / ]          前 / 次ページ（1 ページ 40 件）",
            "g              ページ番号ジャンプ（数字入力 + Enter, Esc で取消）",
            "Esc            戻る（Repositories/PullRequests のうち入って来た画面へ）",
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
        Screen::ImageView => &[
            "→ / n          次の画像",
            "← / p          前の画像（いずれも境界でクランプ・循環しない）",
            "Esc            PR 詳細へ戻る",
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
fn render_jump_palette(
    frame: &mut Frame,
    area: Rect,
    palette: &mut JumpPaletteState,
    theme: &Theme,
) {
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

    use std::collections::HashMap;

    use crate::api::Comment;
    use crate::tui::app::{CommentLayout, build_comment_layout};
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
            rendered_split: None,
            file_index: 0,
            cursor: 0,
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
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
            rendered_split: None,
            file_index: 0,
            cursor: 0,
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
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

    #[test]
    fn render_diff_body_highlights_current_cursor_row_with_selection_colors() {
        let mut diff = make_diff_state(5);
        diff.viewport = 5;
        diff.cursor = 2;
        let theme = Theme::default();

        let backend = TestBackend::new(30, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        // 枠線(1) 分を差し引いた本文の先頭セル。cursor=2 行目はコンテンツの 3 行目
        // （y = 1(境界) + cursor）。
        let cursor_cell = buffer.cell((2, 1 + 2)).expect("cursor row cell exists");
        assert_eq!(cursor_cell.fg, theme.selection_fg);
        assert_eq!(cursor_cell.bg, theme.selection_bg);

        // 他の行（先頭の文脈行）はハイライトされない。
        let other_cell = buffer.cell((2, 1)).expect("other row cell exists");
        assert_ne!(other_cell.bg, theme.selection_bg);
    }

    #[test]
    fn render_diff_body_highlight_does_not_invalidate_rendered_lines_cache() {
        // Phase1 のキャッシュ（`rendered_lines`）は現在行ハイライトを挟んでも再構築されない
        // べき（ハイライトは viewport スライスの複製にのみ適用する契約）。
        let mut diff = make_diff_state(50);
        diff.viewport = 10;
        diff.cursor = 0;
        let theme = Theme::default();

        let backend = TestBackend::new(30, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("first draw succeeds");
        let first_ptr = diff
            .rendered_lines
            .as_ref()
            .expect("cache built on first render")
            .as_ptr();

        diff.cursor = 5;
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
            "現在行ハイライトを描画してもキャッシュは再構築されないべき"
        );
    }

    #[test]
    fn render_diff_body_shows_cursor_position_label_in_title() {
        let text =
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-old\n+new\n".to_string();
        let mut diff = DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 10,
            title: "#9".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 5, // "+new"（追加行 → 新ファイル側の行番号）
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        };
        let theme = Theme::default();

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("x:1"), "position label missing: {content}");
        assert!(content.contains('新'), "side marker missing: {content}");
    }

    #[test]
    fn render_diff_body_boxes_comment_thread_below_anchored_line() {
        // 行: 0=FileHeader 1=Hunk 2=Added(new_no=1)。新側 1 行目にスレッドを置く。
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -0,0 +1 @@\n+new line\n");
        let json = r#"{ "id": 1, "content": { "raw": "looks good to me" },
                        "user": { "display_name": "Bob" }, "deleted": false,
                        "created_on": "2026-05-27T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 1 } }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid inline comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Default::default());
        let mut diff = DiffState {
            parsed,
            viewport: 12,
            view_mode: DiffViewMode::Unified,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        let theme = Theme::default();
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                render_diff_body(frame, frame.area(), &mut diff, &theme);
            })
            .expect("draw succeeds");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Bob"), "comment author missing: {content}");
        assert!(
            content.contains("2026-05-27"),
            "comment timestamp missing: {content}"
        );
        assert!(
            content.contains("looks good to me"),
            "comment body missing: {content}"
        );
        // 枠（ボックス）で囲まれている。
        assert!(content.contains('┌'), "box top border missing: {content}");
        assert!(
            content.contains('└'),
            "box bottom border missing: {content}"
        );
        // アクションリンク行（Reply）と 🗨 アイコンも出る。
        assert!(content.contains("Reply"), "action links missing: {content}");
        assert!(content.contains('🗨'), "bubble icon missing: {content}");
    }

    #[test]
    fn render_diff_body_collects_action_hitboxes() {
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -0,0 +1 @@\n+new line\n");
        let json = r#"{ "id": 1, "content": { "raw": "hi" },
                        "user": { "display_name": "Bob" }, "deleted": false,
                        "created_on": "2026-05-27T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 1 } }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid inline comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Default::default());
        let mut diff = DiffState {
            parsed,
            viewport: 12,
            view_mode: DiffViewMode::Unified,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        let theme = Theme::default();
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        let mut hits = Vec::new();
        terminal
            .draw(|frame| {
                hits = render_diff_body(frame, frame.area(), &mut diff, &theme);
            })
            .expect("draw succeeds");
        // ルート 1 件・他人のコメント → Reply と Resolve の 2 リンク。
        let actions: Vec<CommentAction> = hits.iter().map(|hit| hit.action).collect();
        assert_eq!(actions, vec![CommentAction::Reply, CommentAction::Resolve]);
        assert!(hits.iter().all(|hit| hit.comment_id == 1));
        // ヒットボックスの行はボックス内のアクション行（枠+diff3行+Top+Header+Body の次 = y7）。
        assert!(hits.iter().all(|hit| hit.area.height == 1));
    }

    #[test]
    fn render_diff_body_shows_collapsed_row_for_resolved_thread() {
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -0,0 +1 @@\n+new line\n");
        let json = r#"{ "id": 1, "content": { "raw": "done" },
                        "user": { "display_name": "Bob" }, "deleted": false,
                        "created_on": "2026-05-27T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 1 },
                        "resolution": {} }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid resolved comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Default::default());
        let mut diff = DiffState {
            parsed,
            viewport: 12,
            view_mode: DiffViewMode::Unified,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        let theme = Theme::default();
        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                render_diff_body(frame, frame.area(), &mut diff, &theme);
            })
            .expect("draw succeeds");
        let content = buffer_text(terminal.backend().buffer());
        assert!(
            content.contains("resolved this thread"),
            "collapsed summary missing: {content}"
        );
        // 展開表示（枠・本文）は出ない。
        assert!(!content.contains('┌'), "box should be collapsed: {content}");
        assert!(
            !content.contains("done"),
            "body should be hidden: {content}"
        );
    }

    #[test]
    fn render_diff_sidebar_shows_tree_stats_and_comment_badge() {
        let text = "diff --git a/lib/a.rs b/lib/a.rs\n@@ -1 +1,2 @@\n ctx\n+added\n\
                    diff --git a/lib/b.rs b/lib/b.rs\n@@ -1 +0,0 @@\n-removed\n"
            .to_string();
        let parsed = parse_diff(&text);
        let sidebar_rows = crate::tui::diff::build_sidebar_rows(&parsed.files);
        let layout = CommentLayout {
            file_comment_counts: vec![2, 0],
            ..Default::default()
        };
        let diff = DiffState {
            parsed,
            scroll: 0,
            viewport: 10,
            title: "#1".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 0,
            focus: DiffFocus::Files,
            view_mode: DiffViewMode::Unified,
            comment_layout: layout,
            sidebar_rows,
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        };
        let theme = Theme::default();
        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render_diff_sidebar(frame, frame.area(), &diff, &theme))
            .expect("draw succeeds");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("lib"), "folder row missing: {content}");
        assert!(content.contains("a.rs"), "file row missing: {content}");
        assert!(content.contains("+1"), "added stat missing: {content}");
        assert!(content.contains('🗨'), "comment badge missing: {content}");
        // 変更ファイルの状態マーカー（M）。
        assert!(content.contains('M'), "status marker missing: {content}");
    }

    #[test]
    fn render_diff_sidebar_shows_added_and_deleted_status_markers() {
        let text = "diff --git a/n.rs b/n.rs\nnew file mode 100644\n@@ -0,0 +1 @@\n+x\n\
                    diff --git a/o.rs b/o.rs\ndeleted file mode 100644\n@@ -1 +0,0 @@\n-y\n"
            .to_string();
        let parsed = parse_diff(&text);
        let sidebar_rows = crate::tui::diff::build_sidebar_rows(&parsed.files);
        let diff = DiffState {
            parsed,
            viewport: 10,
            focus: DiffFocus::Files,
            view_mode: DiffViewMode::Unified,
            sidebar_rows,
            ..Default::default()
        };
        let theme = Theme::default();
        let backend = TestBackend::new(40, 8);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render_diff_sidebar(frame, frame.area(), &diff, &theme))
            .expect("draw succeeds");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains('A'), "added marker missing: {content}");
        assert!(content.contains('D'), "deleted marker missing: {content}");
    }

    #[test]
    fn render_diff_body_split_shows_comment_box_in_new_column() {
        // 新側 1 行目にコメント。split では右（新側）カラムに枠付きで出る。
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -0,0 +1 @@\n+new line\n");
        let json = r#"{ "id": 1, "content": { "raw": "hi" },
                        "user": { "display_name": "Bob" }, "deleted": false,
                        "created_on": "2026-05-27T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 1 } }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid inline comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Default::default());
        let mut diff = DiffState {
            parsed,
            viewport: 12,
            view_mode: DiffViewMode::Split,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        let theme = Theme::default();
        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                render_diff_body_split(frame, frame.area(), &mut diff, &theme);
            })
            .expect("draw succeeds");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("Bob"), "comment author missing: {content}");
        // 枠が新側（右カラム）に出る。座標: 中央より右。
        let pos = find_text_position(terminal.backend().buffer(), "Bob");
        assert!(
            pos.is_some_and(|(x, _)| x >= 40),
            "comment not in new column"
        );
    }

    // ---- split 表示（render_diff_body_split） ----

    /// バッファ全体から `needle` を探し、見つかった先頭セルの座標を返す（複数行にまたがる
    /// 文字列は対象外）。左右どちらのペインに描画されたかを列位置で判定するのに使う。
    fn find_text_position(buffer: &Buffer, needle: &str) -> Option<(u16, u16)> {
        let chars: Vec<char> = needle.chars().collect();
        let area = buffer.area;
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let matches = chars.iter().enumerate().all(|(offset, expected)| {
                    let cx = x + offset as u16;
                    cx < area.right()
                        && buffer
                            .cell((cx, y))
                            .is_some_and(|cell| cell.symbol() == expected.to_string())
                });
                if matches {
                    return Some((x, y));
                }
            }
        }
        None
    }

    #[test]
    fn render_diff_body_split_shows_old_text_on_left_and_new_text_on_right() {
        let text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,1 +1,1 @@\n-old text\n+new text\n"
            .to_string();
        let mut diff = DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 10,
            title: "#1".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 0,
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Split,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        };
        let theme = Theme::default();

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        let mid_x = buffer.area.width / 2;
        let (old_x, _) = find_text_position(buffer, "old text").expect("old text visible");
        let (new_x, _) = find_text_position(buffer, "new text").expect("new text visible");
        assert!(
            old_x < mid_x,
            "old text should render in the left (old) pane: x={old_x}, mid={mid_x}"
        );
        assert!(
            new_x >= mid_x,
            "new text should render in the right (new) pane: x={new_x}, mid={mid_x}"
        );
    }

    #[test]
    fn render_diff_body_split_caches_rows_and_reuses_allocation_across_frames() {
        let mut diff = make_diff_state(300);
        diff.view_mode = DiffViewMode::Split;
        diff.viewport = 15;
        diff.scroll = 0;
        let theme = Theme::default();

        let backend = TestBackend::new(60, 17);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &theme);
            })
            .expect("first draw succeeds");

        let cached = diff
            .rendered_split
            .as_ref()
            .expect("cache built on first render");
        assert_eq!(cached.len(), diff.parsed.split_lines.len());
        let first_ptr = cached.as_ptr();

        // scroll を変えて再描画してもキャッシュは再構築されず、同じアロケーションを使い回す。
        diff.scroll = 200;
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &theme);
            })
            .expect("second draw succeeds");
        let second_ptr = diff
            .rendered_split
            .as_ref()
            .expect("cache still present")
            .as_ptr();
        assert_eq!(
            first_ptr, second_ptr,
            "split 表示の着色済み行キャッシュは一度だけ構築され、以後は再利用されるべき"
        );
    }

    #[test]
    fn render_diff_body_split_only_draws_viewport_worth_of_lines() {
        let mut diff = make_diff_state(50);
        diff.view_mode = DiffViewMode::Split;
        diff.viewport = 5;
        diff.scroll = 3;
        let theme = Theme::default();

        // 幅十分・高さ = 可視 5 行 + 上下ボーダー。
        let backend = TestBackend::new(60, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &theme);
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
    fn render_diff_body_split_rebuilds_cache_with_new_theme_colors_after_invalidation() {
        let mut diff = make_diff_state(5);
        diff.view_mode = DiffViewMode::Split;
        diff.viewport = 5;
        let catppuccin = Theme::default();

        let backend = TestBackend::new(60, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &catppuccin);
            })
            .expect("first draw succeeds");
        assert!(diff.rendered_split.is_some());

        // テーマ切替相当（`App::cycle_theme` がやること）。
        diff.rendered_split = None;
        let nord = ThemeName::Nord.theme();
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &nord);
            })
            .expect("second draw succeeds");
        assert!(
            diff.rendered_split.is_some(),
            "無効化後はキャッシュが再構築されるべき"
        );
    }

    #[test]
    fn render_diff_body_split_highlights_current_cursor_row_on_both_panes() {
        let mut diff = make_diff_state(5);
        diff.view_mode = DiffViewMode::Split;
        diff.viewport = 5;
        diff.cursor = 2;
        let theme = Theme::default();

        let backend = TestBackend::new(60, 7);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff_body_split(frame, area, &mut diff, &theme);
            })
            .expect("draw succeeds");

        let buffer = terminal.backend().buffer();
        // 左ペインの本文先頭セル（枠線(1) + パディング(1)）。cursor=2 行目はコンテンツの
        // 3 行目（y = 1(境界) + cursor）。
        let left_cell = buffer.cell((2, 1 + 2)).expect("left cursor cell exists");
        assert_eq!(left_cell.fg, theme.selection_fg);
        assert_eq!(left_cell.bg, theme.selection_bg);

        // 右ペイン（幅 60 の半分 = x:30 から。枠線(1) + パディング(1)）。
        let right_cell = buffer.cell((32, 1 + 2)).expect("right cursor cell exists");
        assert_eq!(right_cell.fg, theme.selection_fg);
        assert_eq!(right_cell.bg, theme.selection_bg);

        // 他の行（先頭の文脈行）はハイライトされない。
        let other_left = buffer.cell((2, 1)).expect("other row cell exists");
        assert_ne!(other_left.bg, theme.selection_bg);
    }

    #[test]
    fn render_diff_split_mode_renders_sidebar_and_both_panes() {
        let mut diff = make_multi_file_diff_state(2, 3);
        diff.view_mode = DiffViewMode::Split;
        let mut app = app_with_diff(diff);

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("file0.txt"));
        assert!(content.contains("file1.txt"));
        assert!(content.contains("file0 line 0"));
        // split モードでは新/旧のペインタイトルが両方出る。
        assert!(content.contains('旧'));
        assert!(content.contains('新'));
    }

    /// `diff` を持つ最小構成の `App`（Diff 画面の 2 ペイン描画テスト用）。
    fn app_with_diff(diff: DiffState) -> App {
        let mut app = App::new(crate::config::Config::default(), None);
        app.screen = Screen::Diff;
        app.diff = Some(diff);
        app
    }

    #[test]
    fn render_diff_hides_sidebar_and_uses_full_width_when_toggled_off() {
        let mut app = app_with_diff(make_multi_file_diff_state(2, 3));
        app.diff_sidebar_visible = false;

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        // サイドバーの枠（タイトル）は出ず、本文だけが全幅で描画される
        // （diff 本文自体にはヘッダ行 `+++ b/file0.txt` 等が含まれるため、ファイル名の
        // 有無ではなくサイドバー固有のタイトル文字列で判定する）。
        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("ファイル (2)"));
        assert!(content.contains("file0 line 0"));

        assert_eq!(
            app.layout.panes,
            vec![(PaneKind::DiffBody, Rect::new(0, 0, 60, 12))],
            "非表示中は DiffFiles ペインを登録しない"
        );
        assert!(
            app.layout.lists.is_empty(),
            "非表示中はファイル一覧のヒットテストも登録しない"
        );
    }

    #[test]
    fn render_diff_split_mode_hides_sidebar_and_uses_full_width_when_toggled_off() {
        let mut diff = make_multi_file_diff_state(2, 3);
        diff.view_mode = DiffViewMode::Split;
        let mut app = app_with_diff(diff);
        app.diff_sidebar_visible = false;

        let backend = TestBackend::new(80, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        assert!(!content.contains("ファイル (2)"));
        assert!(content.contains('旧'));
        assert!(content.contains('新'));
        assert_eq!(
            app.layout.panes,
            vec![(PaneKind::DiffBody, Rect::new(0, 0, 80, 12))]
        );
    }

    #[test]
    fn render_diff_uses_saved_sidebar_width_when_present() {
        let mut app = app_with_diff(make_multi_file_diff_state(2, 3));
        app.diff_sidebar_width = Some(25);

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        let sidebar_pane = app
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffFiles)
            .map(|(_, area)| *area)
            .expect("sidebar pane registered");
        assert_eq!(sidebar_pane.width, 25);
        let body_pane = app
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffBody)
            .map(|(_, area)| *area)
            .expect("body pane registered");
        assert_eq!(body_pane.x, 25);
        assert_eq!(body_pane.width, 35);
    }

    #[test]
    fn render_diff_clamps_saved_sidebar_width_to_max_percent() {
        let mut app = app_with_diff(make_multi_file_diff_state(2, 3));
        app.diff_sidebar_width = Some(1000);

        let backend = TestBackend::new(60, 12);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_diff(frame, area, &mut app);
            })
            .expect("draw succeeds");

        let sidebar_pane = app
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffFiles)
            .map(|(_, area)| *area)
            .expect("sidebar pane registered");
        // 全体 60 の 70% = 42 が上限。
        assert_eq!(sidebar_pane.width, 42);
    }

    #[test]
    fn render_draws_page_jump_prompt_when_open() {
        // `g` のページ番号ジャンプはモーダルを開くだけで、トップレベル render() に描画が
        // 配線されていないと画面に何も出ず「効かない」ように見える。オーバーレイが確実に
        // 描画されることを保証する回帰テスト（入力中の数字も表示されること）。
        let mut app = App::new(crate::config::Config::default(), None);
        app.screen = Screen::PullRequests;
        app.page_jump = Some(PageJumpModal {
            input: "3".to_string(),
        });

        let backend = TestBackend::new(60, 20);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw succeeds");

        let content = buffer_text(terminal.backend().buffer());
        // `buffer_text` は全角文字をセル単位で空白区切りに展開するため、空白を除いて照合する。
        let compact = content.replace(' ', "");
        assert!(
            compact.contains("ページ番号ジャンプ"),
            "page-jump prompt not rendered: {content}"
        );
        assert!(
            compact.contains('3'),
            "typed page number missing: {content}"
        );
    }

    #[test]
    fn truncate_file_name_keeps_short_names_untouched() {
        assert_eq!(truncate_file_name("src/main.rs", 20), "src/main.rs");
    }

    #[test]
    fn truncate_middle_keeps_short_text() {
        assert_eq!(truncate_middle("main.rs", 20), "main.rs");
    }

    #[test]
    fn truncate_middle_ellipsizes_center_within_budget_and_keeps_extension() {
        let out = truncate_middle("very_long_file_name.dart", 12);
        assert!(
            UnicodeWidthStr::width(out.as_str()) <= 12,
            "over budget: {out}"
        );
        assert!(out.contains('…'), "should ellipsize: {out}");
        assert!(out.ends_with("dart"), "should keep extension tail: {out}");
    }

    #[test]
    fn truncate_middle_tiny_budget_is_single_ellipsis() {
        assert_eq!(truncate_middle("anything", 1), "…");
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
        assert_eq!(replaced, "[画像: Screenshot]（i で表示 / o でブラウザ）");
    }

    #[test]
    fn replace_image_syntax_replaces_inline_image_and_keeps_surrounding_text() {
        let replaced = replace_image_syntax("見て: ![図](https://example.com/a.png) です");
        assert_eq!(replaced, "見て: [画像: 図]（i で表示 / o でブラウザ） です");
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
            "[画像: a]（i で表示 / o でブラウザ） と [画像: b]（i で表示 / o でブラウザ）"
        );
    }

    #[test]
    fn render_markdown_lines_styles_heading_as_bold_colored_line() {
        // H2 は `cyan().bold()`（`tui_markdown::DefaultStyleSheet::heading`）。見出しの色/太字は
        // 行スタイル（`Line::style`）に載る（各スパン自体は無地）。
        let lines = render_markdown_lines("## Heading");
        assert_eq!(lines.len(), 1);
        assert!(line_text(&lines[0]).contains("Heading"));
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[0].style.fg, Some(Color::Cyan));
    }

    #[test]
    fn render_markdown_lines_renders_bullet_list_with_inline_code() {
        let lines = render_markdown_lines("- item `code`");
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.contains("- "));
        assert!(text.contains("item"));
        assert!(text.contains("code"));
        // インラインコードは白地に黒背景（`tui_markdown::DefaultStyleSheet::code`）。
        let code_span = lines[0]
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "code")
            .expect("code span present");
        assert_eq!(code_span.style.fg, Some(Color::White));
        assert_eq!(code_span.style.bg, Some(Color::Black));
    }

    #[test]
    fn render_markdown_lines_preserves_code_fence_content() {
        let lines = render_markdown_lines("```\nlet x = 1;\n```");
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("let x = 1;"));
    }

    #[test]
    fn render_markdown_lines_replaces_image_syntax_with_placeholder() {
        let lines = render_markdown_lines("![alt](https://example.com/x.png)");
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("[画像: alt]（i で表示 / o でブラウザ）"));
        // 画像記法そのもの（`![...]`）は残らない。
        assert!(!text.contains("!["));
    }

    #[test]
    fn render_markdown_lines_keeps_image_syntax_verbatim_inside_code_fence() {
        // コードフェンス内の `![...]()` は画像プレースホルダに置換されず、コード片として
        // そのまま残る（`replace_image_syntax_outside_code_fences` の契約）。
        let lines = render_markdown_lines("```\n![alt](u)\n```");
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("![alt](u)"));
    }

    #[test]
    fn render_markdown_lines_multi_block_body_produces_multiple_styled_lines() {
        // 見出し・箇条書き・コードブロックを含む本文が複数行の styled Text になることを確認する
        // （tui-markdown は入力行数と出力行数が一致しないため、単純な行数一致ではなく
        // 「複数行になっている」ことと「各要素の内容が含まれる」ことを検査する）。
        let body = "# Title\n\n- one\n- two\n\n```\ncode line\n```\n\nplain paragraph";
        let lines = render_markdown_lines(body);
        assert!(lines.len() > 4, "got {} lines", lines.len());
        let text = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(text.contains("Title"));
        assert!(text.contains("one"));
        assert!(text.contains("two"));
        assert!(text.contains("code line"));
        assert!(text.contains("plain paragraph"));
    }

    #[test]
    fn render_markdown_lines_does_not_panic_on_japanese_and_ragged_markdown() {
        // 日本語テキスト・崩れた画像記法・空文字列で panic しないこと。
        let _ = render_markdown_lines("");
        let _ = render_markdown_lines("見出し\n\n- 箇条書き\n\n![壊れた画像記法(url)\n\n> 引用");
        let _ = render_markdown_lines("![]()");
        let _ = render_markdown_lines("![alt](");
    }

    fn make_pr_with_participants(json: &str) -> PullRequest {
        serde_json::from_str(json).expect("valid pr json")
    }

    #[test]
    fn render_pull_request_detail_writes_back_rendered_body_line_count_for_scroll_clamp() {
        // 本文はソフト改行を含む複数段落（`tui_markdown` は 1 入力行 = 1 出力行を保証しない
        // ため、素朴な生行数カウントとは異なる行数になる）。実際に描画した行数が
        // `App::detail_body_rendered_lines` へ書き戻され、`PageDown` 連打で本文末尾に到達すると
        // スクロールが止まる（無制限に伸び続けない）ことを確認する。
        use crate::config::Config;
        use crate::tui::app::Msg;
        use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let mut paragraphs = Vec::new();
        for i in 0..30 {
            paragraphs.push(format!("paragraph {i} line a\nline b"));
        }
        let body = paragraphs.join("\n\n");
        let description = serde_json::to_string(&body).expect("json string");
        let pr = make_pr_with_participants(&format!(
            r#"{{ "id": 1, "description": {description}, "participants": [] }}"#
        ));

        let mut app = App::new(Config::default(), None);
        app.theme = Theme::default();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(pr);

        let backend = TestBackend::new(40, 20);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("first draw succeeds");
        assert!(
            app.detail_body_rendered_lines.is_some(),
            "初回描画で本文の実測行数が書き戻されるべき"
        );
        for _ in 0..200 {
            app.update(Msg::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE,
            )));
            terminal
                .draw(|frame| render(frame, &mut app))
                .expect("draw succeeds");
        }
        let stabilized = app.detail_scroll;

        // さらに押しても増えない（本文末尾で止まっている）。
        app.update(Msg::Key(KeyEvent::new(
            KeyCode::PageDown,
            KeyModifiers::NONE,
        )));
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw succeeds");
        assert_eq!(
            app.detail_scroll, stabilized,
            "スクロール上限で止まらず伸び続けている（clamp が破綻している）"
        );
    }

    #[test]
    fn overview_line_count_uses_post_wrap_paragraph_height() {
        use crate::config::Config;

        let body = "日本語の長い本文とhttps://example.com/a/very/long/pathを含む行".repeat(12);
        let description = serde_json::to_string(&body).expect("json string");
        let pr = make_pr_with_participants(&format!(
            r#"{{ "id": 1, "description": {description}, "participants": [] }}"#
        ));
        let mut app = App::new(Config::default(), None);
        app.theme = Theme::default();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(pr);

        let backend = TestBackend::new(32, 18);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw succeeds");

        let rendered = app.detail_body_rendered_lines.unwrap_or(0);
        assert!(rendered > 5);
        app.detail_scroll = u16::MAX;
        app.clamp_detail_scroll();
        assert_eq!(
            app.detail_scroll as usize,
            rendered.saturating_sub(app.detail_viewport.max(1))
        );
    }

    #[test]
    fn overview_render_exposes_rich_document_link_positions_on_app() {
        use crate::config::Config;

        let pr = make_pr_with_participants(
            r#"{ "id": 1, "description": "before [docs](https://example.com/docs) after", "participants": [] }"#,
        );
        let mut app = App::new(Config::default(), None);
        app.theme = Theme::default();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(pr);

        let backend = TestBackend::new(50, 18);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| render(frame, &mut app))
            .expect("draw succeeds");

        let position = app
            .overview_link_positions
            .first()
            .expect("overview link position");
        assert_eq!(position.urls, vec!["https://example.com/docs"]);
        assert!(position.column_range.start < position.column_range.end);
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
                render_comments(frame, frame.area(), &comments, 0, false, &theme);
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
    fn render_pipelines_with_pager_does_not_panic_and_shows_page_label() {
        let mut app = App::new(crate::config::Config::default(), None);
        app.pipelines.set_items(vec![]);
        app.pipelines_page_info = PageInfo {
            page: 2,
            total_pages: None,
            has_next: true,
        };
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_pipelines(frame, area, &mut app);
            })
            .expect("draw succeeds even with empty list");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("page 2"));
    }

    #[test]
    fn render_commits_with_pager_does_not_panic_and_shows_page_label() {
        let mut app = App::new(crate::config::Config::default(), None);
        app.commits.set_items(vec![]);
        app.commits_page = 2;
        app.commits_next_url = Some("https://api.example/commits?ctx=abc".to_string());
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).expect("terminal builds");
        terminal
            .draw(|frame| {
                let area = frame.area();
                render_commits(frame, area, &mut app);
            })
            .expect("draw succeeds even with empty list");
        let content = buffer_text(terminal.backend().buffer());
        assert!(content.contains("page 2"));
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

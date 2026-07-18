//! アプリ状態・画面遷移・`update()`。
//!
//! bubbletea の `Model`/`Msg`/`Cmd` に相当する構造。`update()` は状態を更新し、副作用を
//! [`Command`] として返す。実際の非同期実行（API 呼び出しの spawn）は `event` モジュールが行う。

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use image::DynamicImage;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Matcher, Utf32Str};
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::ListState;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

use crate::api::{
    ApiError, BitbucketClient, Branch, Comment, CommentSide, Commit, DiffStatEntry, ListSort,
    MergeParams, MergeStrategy, PageInfo, Pipeline, PipelineStep, PipelineTarget, PullRequest,
    Repository, SrcEntry, TargetBranch, User, Workspace,
};
use crate::auth;
use crate::config::Config;
use crate::tui::diff::{
    CommentAnchor, ParsedDiff, SidebarRow, build_sidebar_rows, parse as parse_diff,
};
use crate::tui::imageview::{self, ImageRef};
use crate::tui::logview::LogView;
use crate::tui::onboarding::{Field, OnboardingState, TextInput};
use crate::tui::richdoc::{self, ImagePresentation, LinkPosition};
use crate::tui::theme::{Theme, ThemeName};

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
    /// PR 本文内の画像を表示する画面（PR 詳細から `i`）。
    ImageView,
}

/// PR 詳細画面でキーボード操作の対象になっているペイン。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DetailFocus {
    #[default]
    Overview,
    Files,
    Comments,
}

/// 毎フレーム UI から書き戻されるマウスのヒットテスト対象。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PaneKind {
    Overview,
    ChangedFiles,
    Comments,
    DiffFiles,
    DiffBody,
    StepLog,
    FileView,
    ImageView,
    Static,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Workspaces,
    Repositories,
    PullRequests,
    ChangedFiles,
    Pipelines,
    PipelineSteps,
    Branches,
    Commits,
    Source,
    DiffFiles,
    LinkPalette,
    JumpPalette,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModalKind {
    Help,
    CommentEditor,
    MergeConfirm,
    PipelineConfirm,
    DeleteCommentConfirm,
    PageJump,
    LinkPalette,
    JumpPalette,
    PrFilter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListLayout {
    pub kind: ListKind,
    pub area: Rect,
    pub first_visible: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalLayout {
    pub kind: ModalKind,
    pub area: Rect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HintLayout {
    pub area: Rect,
    pub key: KeyEvent,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageHit {
    pub area: Rect,
    pub url: String,
}

/// 描画と入力を分離する App 所有のレイアウト表。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppLayout {
    pub panes: Vec<(PaneKind, Rect)>,
    pub lists: Vec<ListLayout>,
    pub modal: Option<ModalLayout>,
    pub hints: Vec<HintLayout>,
    pub overview_content: Option<Rect>,
    pub overview_images: Vec<ImageHit>,
    /// Diff 本文のコメントアクションリンク（Reply 等）のヒットボックス（毎フレーム再構築）。
    pub comment_actions: Vec<CommentActionHit>,
}

impl DetailFocus {
    fn next(self) -> Self {
        match self {
            Self::Overview => Self::Files,
            Self::Files => Self::Comments,
            Self::Comments => Self::Overview,
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Overview => Self::Comments,
            Self::Files => Self::Overview,
            Self::Comments => Self::Files,
        }
    }
}

/// PR 本文・コメントから抽出したブラウザで開けるリンク。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DetailLink {
    pub label: String,
    pub url: String,
}

#[derive(Debug, Default)]
pub struct LinkPalette {
    pub links: SelectList<DetailLink>,
}

/// PR の状態（一覧フィルタの選択肢）。
///
/// `Ord` は `BTreeSet` 内の順序（＝クエリ順・表示順）を宣言順（OPEN → MERGED → DECLINED →
/// SUPERSEDED）に固定するために導出する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PrState {
    Open,
    Merged,
    Declined,
    Superseded,
}

impl PrState {
    /// フィルタモーダルの表示順（`Ord`＝`BTreeSet` の走査順と一致させる）。
    pub const ALL: [PrState; 4] = [
        PrState::Open,
        PrState::Merged,
        PrState::Declined,
        PrState::Superseded,
    ];

    /// API へ渡す `state` 値（config.toml への保存値も同じ文字列）。
    pub fn api_value(self) -> &'static str {
        match self {
            PrState::Open => "OPEN",
            PrState::Merged => "MERGED",
            PrState::Declined => "DECLINED",
            PrState::Superseded => "SUPERSEDED",
        }
    }

    /// 設定ファイルの文字列から解釈する（未知の値は `None`）。
    pub fn from_config_str(value: &str) -> Option<PrState> {
        PrState::ALL
            .iter()
            .copied()
            .find(|state| state.api_value() == value)
    }
}

/// PR 一覧の author フィルタ候補（`uuid` を `q` フィルタに渡し、`display_name` を UI に出す）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrAuthor {
    pub uuid: String,
    pub display_name: String,
}

/// PR 一覧のフィルタ（state の複数選択 + author + target branch）。
///
/// `Hash` は PR 一覧キャッシュ（[`RevisitCache`]）のキーの一部として使うために導出する。
/// `states` は空にしない（フィルタモーダルが空集合の適用を拒否する。空のまま送ると
/// Bitbucket 既定＝OPEN のみになり表示と食い違うため）。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PrStateFilter {
    pub states: BTreeSet<PrState>,
    /// author フィルタ。リポジトリ/ワークスペース依存のためセッション内のみ（保存しない）。
    pub author: Option<PrAuthor>,
    /// target branch フィルタ（BBQL `destination.branch.name`）。author と同じく
    /// リポジトリ依存のためセッション内のみ（保存しない）。
    pub target_branch: Option<TargetBranch>,
}

impl Default for PrStateFilter {
    /// 既定は OPEN のみ・author なし。
    fn default() -> Self {
        Self::only(PrState::Open)
    }
}

impl PrStateFilter {
    /// 単一 state のみのフィルタ（`o`/`m`/`d` 単発キーの写像）。
    pub fn only(state: PrState) -> Self {
        Self {
            states: BTreeSet::from([state]),
            author: None,
            target_branch: None,
        }
    }

    /// 全 state のフィルタ（`a`（All）キーの写像）。
    pub fn all() -> Self {
        Self {
            states: BTreeSet::from(PrState::ALL),
            author: None,
            target_branch: None,
        }
    }

    /// config.toml の保存値（state 文字列の配列）から復元する。不正値は無視し、1 つも
    /// 残らなければ既定（OPEN のみ）に落とす。author / target branch は保存しないため
    /// 常に `None`。
    pub fn from_config(states: Option<&[String]>) -> Self {
        let parsed: BTreeSet<PrState> = states
            .unwrap_or_default()
            .iter()
            .filter_map(|value| PrState::from_config_str(value))
            .collect();
        if parsed.is_empty() {
            Self::default()
        } else {
            Self {
                states: parsed,
                author: None,
                target_branch: None,
            }
        }
    }

    /// config.toml への保存値（state 文字列の配列、`BTreeSet` の順）。
    pub fn config_states(&self) -> Vec<String> {
        self.states
            .iter()
            .map(|state| state.api_value().to_string())
            .collect()
    }

    /// API へ渡す `state` 値の並び（`BTreeSet` の `Ord` 順で安定）。
    pub fn state_values(&self) -> Vec<&'static str> {
        self.states.iter().map(|state| state.api_value()).collect()
    }

    /// author の uuid（`q` フィルタ用）。
    pub fn author_uuid(&self) -> Option<&str> {
        self.author.as_ref().map(|author| author.uuid.as_str())
    }

    /// UI 表示ラベル（例: `OPEN+MERGED, author: Alice, target~"release"`。全 state 選択時は
    /// `ALL`。target は完全一致なら `target: main`、部分一致なら `target~"release"`）。
    pub fn label(&self) -> String {
        let mut label = if self.states.len() == PrState::ALL.len() {
            "ALL".to_string()
        } else {
            self.state_values().join("+")
        };
        if let Some(author) = &self.author {
            label = format!("{label}, author: {}", author.display_name);
        }
        match &self.target_branch {
            Some(target) if target.exact => format!("{label}, target: {}", target.text),
            Some(target) => format!("{label}, target~\"{}\"", target.text),
            None => label,
        }
    }
}

/// PR フィルタモーダルのフォーカス中セクション（`Tab` で移動）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrFilterSection {
    States,
    Author,
    Target,
}

impl PrFilterSection {
    /// `Tab` の巡回順（States → Author → Target → States）。
    fn next(self) -> Self {
        match self {
            Self::States => Self::Author,
            Self::Author => Self::Target,
            Self::Target => Self::States,
        }
    }

    /// `Shift+Tab` の逆順。
    fn previous(self) -> Self {
        match self {
            Self::States => Self::Target,
            Self::Author => Self::States,
            Self::Target => Self::Author,
        }
    }
}

/// PR フィルタモーダル（`f`）の状態。`Enter` で適用するまで [`App::pr_state_filter`] には
/// 反映しない（作業コピー）。
#[derive(Debug)]
pub struct PrFilterModal {
    pub section: PrFilterSection,
    /// state チェックボックスの作業コピー。
    pub states: BTreeSet<PrState>,
    /// State セクションのカーソル（[`PrState::ALL`] のインデックス）。
    pub state_cursor: usize,
    /// Author セクションのカーソル（表示行のインデックス。クエリ空なら 0 = All authors、
    /// `1..` = `authors[author_matches[i-1]]`。クエリ非空なら `authors[author_matches[i]]`）。
    /// Author セクションは単一選択のため、カーソル位置がそのまま選択を表す。
    pub author_cursor: usize,
    /// author 候補（`None` は読み込み中）。
    pub authors: Option<Vec<PrAuthor>>,
    /// Author セクションの検索クエリ（印字文字で追記・`Backspace` で削除。`j`/`k` も文字と
    /// して扱う。セクション移動（Tab）では保持し、モーダルを閉じたら破棄する）。
    pub author_query: String,
    /// `author_query` を通過した候補（`authors` へのインデックス）。クエリ空なら恒等写像、
    /// 非空なら fuzzy スコア降順（[`fuzzy_match_indices`]）。表示・カーソル・適用はこの
    /// 並びを辿る。
    pub author_matches: Vec<usize>,
    /// Target セクションのカーソル（表示行のインデックス。行 0 = All branches、クエリ非空
    /// なら行 1 = 「『{query}』で部分一致」、以降 = `branches[target_matches[..]]`）。
    /// 単一選択のため、カーソル位置がそのまま選択を表す。
    pub target_cursor: usize,
    /// target branch 候補（ブランチ名。`None` は読み込み中。取得失敗時は空の `Some` =
    /// 候補なし。自由入力の部分一致は候補が無くても使える）。
    pub branches: Option<Vec<String>>,
    /// Target セクションの検索クエリ（操作系は `author_query` と同じ。部分一致フィルタ
    /// 適用中にモーダルを開いたときは、その文字列で初期化する）。
    pub target_query: String,
    /// `target_query` を通過した候補（`branches` へのインデックス）。クエリ空なら恒等写像、
    /// 非空なら fuzzy スコア降順（[`fuzzy_match_indices`]）。
    pub target_matches: Vec<usize>,
}

/// フィルタモーダルの表示行が指す選択内容。行 → 選択の写像を `App` の適用
/// （[`App::apply_pr_filter_modal`]）と `ui` の描画（`render_pr_filter_modal`）で共有し、
/// 「見えている選択」と「Enter で適用される選択」のずれを構造的に防ぐ。
pub enum PrFilterRow<T> {
    /// All authors / All branches 行（フィルタ解除）。
    All,
    /// 部分一致行（Target 専用。クエリ非空のとき行 1）。
    Partial,
    /// 候補行。
    Candidate(T),
    /// 選択を指さない行（読み込み中・候補 0 件・範囲外）。適用時は現用フィルタを維持する。
    Missing,
}

impl PrFilterModal {
    /// Author セクションの表示行数。クエリ空 = 「All authors」+ 全候補（読み込み中は
    /// All authors のみ）、クエリ非空 = マッチした候補のみ（0 件なら 0 行）。
    /// カーソル移動の範囲と `ui` の候補窓計算の双方で使う。
    pub fn author_row_count(&self) -> usize {
        self.author_matches.len() + usize::from(self.author_query.is_empty())
    }

    /// Target セクションの表示行数。「All branches」1 行 + クエリ非空なら部分一致行 1 行 +
    /// マッチしたブランチ候補（候補なしは 0 行）。読み込み中かつクエリ空は選択可能な行なし
    /// （「読み込み中…」プレースホルダのみ。Enter は現用維持なので、選べる見た目を出さない）。
    /// カーソル移動の範囲と `ui` の候補窓計算の双方で使う。
    pub fn target_row_count(&self) -> usize {
        if self.branches.is_none() && self.target_query.is_empty() {
            return 0;
        }
        1 + usize::from(!self.target_query.is_empty()) + self.target_matches.len()
    }

    /// Author セクションの表示行 `row` が指す選択内容（クエリ空なら行 0 = All authors、
    /// 以降 = `authors[author_matches[row-1]]`。クエリ非空なら `authors[author_matches[row]]`。
    /// 読み込み中は行を出さないため常に [`PrFilterRow::Missing`]）。
    pub fn author_row(&self, row: usize) -> PrFilterRow<&PrAuthor> {
        let Some(authors) = self.authors.as_deref() else {
            return PrFilterRow::Missing;
        };
        let query_empty = self.author_query.is_empty();
        if query_empty && row == 0 {
            return PrFilterRow::All;
        }
        match row
            .checked_sub(usize::from(query_empty))
            .and_then(|index| self.author_matches.get(index))
            .and_then(|&index| authors.get(index))
        {
            Some(author) => PrFilterRow::Candidate(author),
            None => PrFilterRow::Missing,
        }
    }

    /// Target セクションの表示行 `row` が指す選択内容（行 0 = All branches、クエリ非空なら
    /// 行 1 = 部分一致、以降 = `branches[target_matches[..]]`。読み込み中かつクエリ空は行を
    /// 出さないため常に [`PrFilterRow::Missing`]）。
    pub fn target_row(&self, row: usize) -> PrFilterRow<&str> {
        let query_empty = self.target_query.is_empty();
        if self.branches.is_none() && query_empty {
            return PrFilterRow::Missing;
        }
        if row == 0 {
            return PrFilterRow::All;
        }
        if !query_empty && row == 1 {
            return PrFilterRow::Partial;
        }
        match row
            .checked_sub(1 + usize::from(!query_empty))
            .and_then(|index| self.target_matches.get(index))
            .and_then(|&index| self.branches.as_deref()?.get(index))
        {
            Some(name) => PrFilterRow::Candidate(name.as_str()),
            None => PrFilterRow::Missing,
        }
    }
}

/// 現在適用中の author が候補（uuid 照合）に無ければ挿入する（表示名の大文字小文字を
/// 無視した昇順 = [`users_to_authors`] の並びを維持）。PR 集約の結果や
/// フォールバック候補に現用 author が含まれない場合（直近 PR に登場しない著者等）でも、
/// モーダル上で現在の選択が見え、そのまま維持・解除できるようにするため。
fn insert_current_author(authors: &mut Vec<PrAuthor>, current: Option<&PrAuthor>) {
    let Some(current) = current else {
        return;
    };
    if authors.iter().any(|author| author.uuid == current.uuid) {
        return;
    }
    let key = current.display_name.to_lowercase();
    let index = authors.partition_point(|author| author.display_name.to_lowercase() <= key);
    authors.insert(index, current.clone());
}

/// モーダルの author カーソル初期位置（現在のフィルタの author が候補にあればその行、
/// 無ければ 0 = All authors）。
fn author_cursor_for(authors: Option<&[PrAuthor]>, current: Option<&PrAuthor>) -> usize {
    let (Some(authors), Some(current)) = (authors, current) else {
        return 0;
    };
    authors
        .iter()
        .position(|author| author.uuid == current.uuid)
        .map_or(0, |index| index + 1)
}

/// 現在適用中の完全一致 target branch が候補（ブランチ名照合）に無ければ先頭へ挿入する。
/// 候補（最終コミット日時降順の 1 ページ目）から漏れたブランチでも、モーダル上で現在の
/// 選択が見え、そのまま維持・解除できるようにするため（[`insert_current_author`] と同じ
/// 狙い。候補の並びに整列キーが無いため位置は先頭とする）。部分一致（`exact=false`）は
/// 検索クエリ側（[`App::open_pr_filter_modal`] の初期化）で表現するため対象外。
fn insert_current_target(branches: &mut Vec<String>, current: Option<&TargetBranch>) {
    let Some(current) = current else {
        return;
    };
    if !current.exact || branches.iter().any(|name| name == &current.text) {
        return;
    }
    branches.insert(0, current.text.clone());
}

/// モーダルの target カーソル初期位置（クエリ空の前提。現在のフィルタが完全一致でその
/// ブランチが候補にあればその行、無ければ 0 = All branches）。
fn target_cursor_for(branches: Option<&[String]>, current: Option<&TargetBranch>) -> usize {
    let (Some(branches), Some(current)) = (branches, current) else {
        return 0;
    };
    if !current.exact {
        return 0;
    }
    branches
        .iter()
        .position(|name| name == &current.text)
        .map_or(0, |index| index + 1)
}

/// ユーザー一覧を author 候補へ変換する（uuid 無しは除外、uuid で重複排除、表示名の
/// 大文字小文字を無視した昇順）。PR 集約（この repo の PR 著者）の結果と、取得失敗時の
/// 「読み込み済み PR の author」フォールバックの双方で使う共通経路。
fn users_to_authors(users: Vec<User>) -> Vec<PrAuthor> {
    let mut seen = HashSet::new();
    let mut authors: Vec<PrAuthor> = users
        .into_iter()
        .filter_map(|user| {
            let uuid = user.uuid?;
            if !seen.insert(uuid.clone()) {
                return None;
            }
            let display_name = user.display_name.unwrap_or_else(|| uuid.clone());
            Some(PrAuthor { uuid, display_name })
        })
        .collect();
    authors.sort_by(|a, b| {
        a.display_name
            .to_lowercase()
            .cmp(&b.display_name.to_lowercase())
    });
    authors
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

/// コメント削除の確認モーダル状態（`d` で開き、Enter で確定・Esc で取消）。
#[derive(Debug, Clone)]
pub struct DeleteCommentModal {
    /// 削除対象のコメント id。
    pub comment_id: u64,
    pub submitting: bool,
}

/// コメント投稿の簡易エディタ状態。
///
/// `inline` が `Some` なら Diff 画面の特定行への返信（インラインコメント）、`None` なら
/// PR 全体への一般コメント。編集 UI（`on_key_comment_editor`）はどちらも共通で、投稿時
/// （`submit_comment`）にどちらの `Command`（`CreateComment`/`CreateInlineComment`）を
/// 発行するかだけが分岐する。
#[derive(Debug, Clone, Default)]
pub struct CommentEditor {
    pub text: String,
    /// `text` への挿入位置（**char 単位**、`0..=chars().count()`）。描画側はこの位置の
    /// 1 文字を反転表示する。byte index ではないのでマルチバイト文字でも安全。
    pub cursor: usize,
    pub submitting: bool,
    pub inline: Option<CommentAnchor>,
    /// `Some(root_id)` なら既存スレッドへの返信（`parent` を送る）。`inline` とは排他で使う。
    pub reply_to: Option<u64>,
    /// `Some(comment_id)` なら既存コメントの編集（本文を上書き）。他と排他で使う。
    pub editing: Option<u64>,
}

impl CommentEditor {
    fn is_submittable(&self) -> bool {
        !self.text.trim().is_empty()
    }

    /// カーソル（char 単位）に対応する byte 位置。挿入/削除はこの位置で行う。
    fn cursor_byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.text.len())
    }

    /// カーソル位置に 1 文字挿入し、カーソルを右へ進める（改行も同じ経路）。
    fn insert_char(&mut self, ch: char) {
        let at = self.cursor_byte();
        self.text.insert(at, ch);
        self.cursor += 1;
    }

    /// カーソル直前の 1 文字を削除する（Backspace。先頭では何もしない）。
    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.cursor_byte();
        self.text.remove(at);
    }

    /// カーソルを 1 文字左へ。
    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// カーソルを 1 文字右へ（末尾でクランプ）。
    fn move_right(&mut self) {
        if self.cursor < self.text.chars().count() {
            self.cursor += 1;
        }
    }

    /// カーソルの (論理行, 行内 char 列)。描画側の反転位置決定にも使う。
    pub fn cursor_line_col(&self) -> (usize, usize) {
        let mut line = 0;
        let mut col = 0;
        for ch in self.text.chars().take(self.cursor) {
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        (line, col)
    }

    /// 各論理行の char 数（改行は含まない）。空文字でも 1 行（長さ 0）になる。
    fn line_char_lens(&self) -> Vec<usize> {
        self.text
            .split('\n')
            .map(|line| line.chars().count())
            .collect()
    }

    /// 指定の (論理行, 列) に対応するカーソル位置（列は行長にクランプ）。
    fn cursor_at(&self, target_line: usize, target_col: usize) -> usize {
        let mut cursor = 0;
        for (line, len) in self.line_char_lens().into_iter().enumerate() {
            if line == target_line {
                return cursor + target_col.min(len);
            }
            cursor += len + 1; // 改行 1 文字ぶん
        }
        self.text.chars().count()
    }

    /// カーソルを論理行の行頭へ（Home）。
    fn move_line_home(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.cursor_at(line, 0);
    }

    /// カーソルを論理行の行末へ（End）。
    fn move_line_end(&mut self) {
        let (line, _) = self.cursor_line_col();
        self.cursor = self.cursor_at(line, usize::MAX);
    }

    /// カーソルを前の論理行へ（Up。列は char 数で維持し行長にクランプ。先頭行では何もしない）。
    fn move_line_up(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line == 0 {
            return;
        }
        self.cursor = self.cursor_at(line - 1, col);
    }

    /// カーソルを次の論理行へ（Down。列は char 数で維持し行長にクランプ。最終行では何もしない）。
    fn move_line_down(&mut self) {
        let (line, col) = self.cursor_line_col();
        if line + 1 >= self.line_char_lens().len() {
            return;
        }
        self.cursor = self.cursor_at(line + 1, col);
    }

    /// インラインコメント用のエディタを作る（対象アンカー付き＝新規スレッド）。
    fn inline(anchor: CommentAnchor) -> Self {
        Self {
            inline: Some(anchor),
            ..Self::default()
        }
    }

    /// 既存スレッドへの返信用エディタを作る（`root_id` は返信先スレッドのルートコメント id）。
    fn reply(root_id: u64) -> Self {
        Self {
            reply_to: Some(root_id),
            ..Self::default()
        }
    }

    /// 既存コメントの編集用エディタを作る（現在の本文をプリフィルし、カーソルは末尾）。
    fn edit(comment_id: u64, text: String) -> Self {
        Self {
            cursor: text.chars().count(),
            text,
            editing: Some(comment_id),
            ..Self::default()
        }
    }
}

/// Diff 本文にインライン表示するコメントスレッドの整形済み中間データ（描画非依存）。
///
/// [`build_comment_layout`] が `parsed` と `comments` から構築し、`DiffLoaded`/`CommentsLoaded`
/// のたびに作り直す。色は持たず（状態層の `Color` 非依存を維持）、行の種別のみを持ち、色は
/// `ui` 側が種別から解決する。
#[derive(Debug, Clone, Default)]
pub struct CommentLayout {
    /// unified 行インデックス → その行にアンカーされたスレッド群（表示順）。
    pub threads_by_line: BTreeMap<usize, Vec<CommentThreadView>>,
    /// ファイルごとの inline コメント総数（サイドバーのバッジ用。`parsed.files` と同順・同長）。
    pub file_comment_counts: Vec<usize>,
}

/// 1 スレッド（ルート + 返信）の表示ビュー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentThreadView {
    /// ルートコメント id（Resolve の対象・スレッド識別）。
    pub root_id: u64,
    /// 解決済みか。
    pub resolved: bool,
    /// ルート→返信の順（`created_on` 昇順）のコメント。
    pub comments: Vec<CommentView>,
}

/// 1 コメントの表示ビュー（色は持たず、`ui` が解決する）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentView {
    pub id: u64,
    pub author: String,
    /// 生の created_on（RFC3339）。表示整形（相対時刻）は描画側が毎フレーム行う。
    pub when: String,
    /// 本文行（改行で分割済み）。
    pub body: Vec<String>,
    /// 返信（ルートでない）なら 1 段インデント。
    pub reply: bool,
    /// 自分の投稿か（アクション行の Edit/Delete 表示可否）。
    pub mine: bool,
}

/// コメントのアクションリンク種別（各コメント下のリンク行。クリック/キー両対応）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentAction {
    Reply,
    Resolve,
    Edit,
    Delete,
}

/// 描画時に収集するアクションリンクのヒットボックス（クリック判定用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentActionHit {
    pub area: Rect,
    pub action: CommentAction,
    pub comment_id: u64,
    pub thread_root: u64,
}

/// Diff 本文の 1 表示行。[`DiffState::scroll`]/[`DiffState::cursor`] はこの配列の添字。
/// コメント行を diff 行と同じ並びに畳み込むことで、`↑↓` がコメントにも乗り、スクロールも
/// 一様に扱える（`display_rows` が空のときは diff 行と 1:1 とみなすフォールバック）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    /// diff 行。unified なら `parsed.lines` 添字、split なら `parsed.split_lines` 添字。
    Diff(usize),
    /// コメントボックスの 1 行。
    Comment(CommentRow),
}

/// コメントボックスの 1 行（枠上/ヘッダ/本文/枠下）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentRow {
    /// スレッドのルートコメント id（Resolve・ハイライト範囲の識別）。
    pub thread_root: u64,
    /// この行が属するコメント id（Header/Body のみ。枠線は `None`）。Edit/Delete/Reply の対象。
    pub comment_id: Option<u64>,
    pub kind: CommentRowKind,
}

/// コメントボックス行の種別。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentRowKind {
    /// 枠上端。
    Top,
    /// ヘッダ（著者 · 日付・解決状態）。カーソル停止対象。
    Header {
        reply: bool,
        author: String,
        when: String,
        resolved: bool,
    },
    /// 本文 1 行。
    Body { reply: bool, text: String },
    /// アクションリンク行（Reply · Resolve · Edit · Delete。クリックで動作）。
    Actions {
        reply: bool,
        /// ルートコメントなら Resolve/Reopen リンクを出す。
        root: bool,
        /// 自分の投稿なら Edit/Delete リンクを出す。
        mine: bool,
        resolved: bool,
    },
    /// 枠下端。
    Bottom,
    /// コラプス（折りたたみ）中のスレッドの 1 行表示。Enter/クリックで展開。
    Collapsed {
        author: String,
        count: usize,
        resolved: bool,
    },
    /// スレッドブロック直後の間隔用の空行（non-focusable。クリックは no-op）。
    Spacer,
}

impl DisplayRow {
    /// カーソルが停止できる行か（diff 行、コメントのヘッダ、またはコラプス行）。
    pub fn is_focusable(&self) -> bool {
        match self {
            DisplayRow::Diff(_) => true,
            DisplayRow::Comment(row) => matches!(
                row.kind,
                CommentRowKind::Header { .. } | CommentRowKind::Collapsed { .. }
            ),
        }
    }

    /// この行が指す diff 行インデックス（Diff 行のみ。Comment 行は `None`）。
    pub fn diff_index(&self) -> Option<usize> {
        match self {
            DisplayRow::Diff(index) => Some(*index),
            DisplayRow::Comment(_) => None,
        }
    }
}

/// `parsed` と `comments` から Diff 本文へインライン表示するスレッド配置を構築する。
///
/// - inline アンカーを持つルート（`parent` 無し）ごとに、親チェーンで集約した返信を
///   `created_on` 昇順に並べて 1 段インデント表示する。
/// - アンカー行はファイル範囲内で `to`→`new_no` / `from`→`old_no` を照合して特定し、
///   見つからなければそのファイルの先頭行へフォールバックする（情報を落とさない）。
pub fn build_comment_layout(parsed: &ParsedDiff, comments: &[Comment], me: &Me) -> CommentLayout {
    let mut layout = CommentLayout {
        threads_by_line: BTreeMap::new(),
        file_comment_counts: vec![0; parsed.files.len()],
    };
    if parsed.files.is_empty() || comments.is_empty() {
        return layout;
    }
    let by_id: HashMap<u64, &Comment> = comments.iter().map(|c| (c.id, c)).collect();

    // ルート（parent 無し）ごとにスレッドを集約する。
    let mut threads: HashMap<u64, Vec<&Comment>> = HashMap::new();
    for comment in comments {
        let root = root_comment_id(comment, &by_id);
        threads.entry(root).or_default().push(comment);
    }

    // 出力順を安定させるためルートを id 昇順で処理する。
    let mut roots: Vec<u64> = threads.keys().copied().collect();
    roots.sort_unstable();

    // パス→ファイル添字は一度だけ、行番号→行インデックスはファイル添字ごとに遅延構築して
    // 再利用する（従来のコメント×行の線形走査を O(全行数) の一度きりの前計算に置き換える）。
    let mut path_to_file: HashMap<&str, usize> = HashMap::new();
    for (index, file) in parsed.files.iter().enumerate() {
        // 先勝ち（従来の線形 `position` と同じ）。実 diff に同一パス重複は無い想定。
        path_to_file.entry(file.name.as_str()).or_insert(index);
    }
    let mut file_line_maps: HashMap<usize, FileLineMaps> = HashMap::new();

    for root_id in roots {
        let Some(root) = by_id.get(&root_id) else {
            continue;
        };
        // inline アンカーを持つスレッドのみ Diff 本文に出す（一般コメントは PR 詳細で見る）。
        if root.inline.is_none() {
            continue;
        }
        let Some((file_index, line)) =
            comment_line_anchor(parsed, root, &path_to_file, &mut file_line_maps)
        else {
            continue;
        };

        let mut thread = threads.remove(&root_id).unwrap_or_default();
        thread.sort_by(|a, b| a.created_on.cmp(&b.created_on));

        if let Some(count) = layout.file_comment_counts.get_mut(file_index) {
            *count += thread.len();
        }

        let comments_view: Vec<CommentView> = thread
            .iter()
            .map(|comment| CommentView {
                id: comment.id,
                author: comment.author_name().to_string(),
                // 生の created_on を保持し、表示整形（相対時刻）は描画時に毎フレーム行う
                // （焼き込むと開きっぱなしで古くなり、PR 詳細ペインと食い違うため）。
                when: comment.created_on.clone().unwrap_or_default(),
                body: comment.raw().lines().map(|l| l.to_string()).collect(),
                // 「返信」判定はスレッドのルートかどうか（`parent.is_some()` だと、親が
                // 削除/未取得でルートに昇格したコメントに誤って返信扱いになる）。
                reply: comment.id != root_id,
                mine: user_is_me(comment.user.as_ref(), me),
            })
            .collect();

        layout
            .threads_by_line
            .entry(line)
            .or_default()
            .push(CommentThreadView {
                root_id,
                resolved: root.is_resolved(),
                comments: comments_view,
            });
    }
    layout
}

/// コメントレイアウトを表示行列（[`DisplayRow`]）へ展開する。表示行 `display_index`（unified
/// なら `parsed.lines` 添字、split なら `parsed.split_lines` 添字）の順に `Diff` 行を並べ、その
/// 直後へ `anchor_lines[display_index]` の各 unified 行にアンカーされたスレッドのボックス行を
/// 差し込む。split の 1 行は旧側・新側の 2 つの unified 行を持ち得る（置換の削除側コメントを
/// 落とさないため両方を集める。重複は 1 度だけ）。コラプス中のスレッド
/// （`collapse_overrides` の明示指定、無ければ解決済み）は 1 行の [`CommentRowKind::Collapsed`]
/// にまとめる。
fn build_display_rows(
    anchor_lines: &[Vec<usize>],
    layout: &CommentLayout,
    collapse_overrides: &HashMap<u64, bool>,
) -> Vec<DisplayRow> {
    let mut rows = Vec::with_capacity(anchor_lines.len());
    for (display_index, unified_lines) in anchor_lines.iter().enumerate() {
        rows.push(DisplayRow::Diff(display_index));
        let mut seen: Vec<usize> = Vec::new();
        for &unified_line in unified_lines {
            if seen.contains(&unified_line) {
                continue;
            }
            seen.push(unified_line);
            let Some(threads) = layout.threads_by_line.get(&unified_line) else {
                continue;
            };
            for thread in threads {
                let collapsed = collapse_overrides
                    .get(&thread.root_id)
                    .copied()
                    .unwrap_or(thread.resolved);
                if collapsed {
                    push_collapsed_row(&mut rows, thread);
                } else {
                    push_thread_rows(&mut rows, thread);
                }
            }
        }
    }
    rows
}

/// コラプス中のスレッドを 1 行で表す（Enter/クリックで展開）。直後に間隔用の空行を挟む。
fn push_collapsed_row(rows: &mut Vec<DisplayRow>, thread: &CommentThreadView) {
    let author = thread
        .comments
        .first()
        .map(|comment| comment.author.clone())
        .unwrap_or_default();
    rows.push(DisplayRow::Comment(CommentRow {
        thread_root: thread.root_id,
        comment_id: Some(thread.root_id),
        kind: CommentRowKind::Collapsed {
            author,
            count: thread.comments.len(),
            resolved: thread.resolved,
        },
    }));
    push_spacer_row(rows, thread.root_id);
}

/// スレッドブロック直後の間隔用の空行を 1 行足す（次のスレッド/diff 行との密着を防ぐ）。
fn push_spacer_row(rows: &mut Vec<DisplayRow>, thread_root: u64) {
    rows.push(DisplayRow::Comment(CommentRow {
        thread_root,
        comment_id: None,
        kind: CommentRowKind::Spacer,
    }));
}

/// 1 スレッドを表示行列へ展開する（枠上 → 各コメントの[ヘッダ + 本文 + アクション行] → 枠下 →
/// 間隔用の空行）。
fn push_thread_rows(rows: &mut Vec<DisplayRow>, thread: &CommentThreadView) {
    rows.push(DisplayRow::Comment(CommentRow {
        thread_root: thread.root_id,
        comment_id: None,
        kind: CommentRowKind::Top,
    }));
    for comment in &thread.comments {
        rows.push(DisplayRow::Comment(CommentRow {
            thread_root: thread.root_id,
            comment_id: Some(comment.id),
            kind: CommentRowKind::Header {
                reply: comment.reply,
                author: comment.author.clone(),
                when: comment.when.clone(),
                resolved: thread.resolved,
            },
        }));
        for body_line in &comment.body {
            rows.push(DisplayRow::Comment(CommentRow {
                thread_root: thread.root_id,
                comment_id: Some(comment.id),
                kind: CommentRowKind::Body {
                    reply: comment.reply,
                    text: body_line.clone(),
                },
            }));
        }
        rows.push(DisplayRow::Comment(CommentRow {
            thread_root: thread.root_id,
            comment_id: Some(comment.id),
            kind: CommentRowKind::Actions {
                reply: comment.reply,
                root: !comment.reply,
                mine: comment.mine,
                resolved: thread.resolved,
            },
        }));
    }
    rows.push(DisplayRow::Comment(CommentRow {
        thread_root: thread.root_id,
        comment_id: None,
        kind: CommentRowKind::Bottom,
    }));
    push_spacer_row(rows, thread.root_id);
}

/// アクション行に表示するリンクの並び（描画とクリック判定で共有する）。
pub fn comment_action_labels(
    root: bool,
    mine: bool,
    resolved: bool,
) -> Vec<(CommentAction, &'static str)> {
    let mut labels = vec![(CommentAction::Reply, "Reply")];
    if root {
        labels.push((
            CommentAction::Resolve,
            if resolved { "Reopen" } else { "Resolve" },
        ));
    }
    if mine {
        labels.push((CommentAction::Edit, "Edit"));
        labels.push((CommentAction::Delete, "Delete"));
    }
    labels
}

/// 現在時刻の unix 秒。コメントの相対時刻表示（[`format_when`]）の基準に使う。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// RFC3339/ISO8601（`2026-05-27T12:34:56.789+09:00` / 末尾 `Z`）を unix 秒へ変換する。
/// 依存を増やさないための最小実装（コメントの相対時刻表示専用）。パース不能は `None`。
fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> { s.get(range)?.parse().ok() };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // 小数秒を読み飛ばす。
    let mut idx = 19;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        while bytes.get(idx).is_some_and(u8::is_ascii_digit) {
            idx += 1;
        }
    }
    // タイムゾーン（Z / ±HH:MM / ±HHMM / 無し=UTC 扱い）。
    let offset_secs = match bytes.get(idx) {
        None | Some(b'Z') | Some(b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let digits: String = s[idx + 1..].chars().filter(char::is_ascii_digit).collect();
            let hh = digits.get(0..2)?.parse::<i64>().ok()?;
            let mm = digits
                .get(2..4)
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(0);
            let total = hh * 3600 + mm * 60;
            if *sign == b'+' { total } else { -total }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset_secs)
}

/// 1970-01-01 からの日数（proleptic Gregorian）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// コメントの時刻表示。24 時間以内は相対（`just now`/`Nm ago`/`Nh ago`）、それより古い・
/// パース不能は日付（先頭 10 文字）へフォールバックする。
pub fn format_when(created_on: &str, now_unix: i64) -> String {
    let fallback = || created_on.chars().take(10).collect::<String>();
    let Some(t) = parse_rfc3339_unix(created_on) else {
        return fallback();
    };
    let delta = now_unix - t;
    // 5 分を超える未来（時計ずれの範囲外）は相対表示せず日付へ（情報を失わない）。
    if delta < -300 {
        return fallback();
    }
    // わずかな時計ずれ（小さな負値）は「just now」に丸める。
    if delta < 60 {
        return "just now".to_string();
    }
    if delta < 3600 {
        return format!("{}m ago", delta / 60);
    }
    if delta < 86_400 {
        return format!("{}h ago", delta / 3600);
    }
    fallback()
}

/// コメントの親チェーンを辿ってスレッドのルートコメント id を返す（循環・欠落に耐性）。
fn root_comment_id(comment: &Comment, by_id: &HashMap<u64, &Comment>) -> u64 {
    let mut cur = comment;
    let mut guard = 0;
    while let Some(parent) = cur.parent.as_ref() {
        let Some(next) = by_id.get(&parent.id) else {
            break;
        };
        cur = next;
        guard += 1;
        if guard > comment_thread_depth_limit() {
            break;
        }
    }
    cur.id
}

/// 親チェーン探索の上限（想定外の循環でも止まるための保険）。
fn comment_thread_depth_limit() -> usize {
    10_000
}

/// 1 ファイル区間の行番号→行インデックスの逆引き表（新側 `new_no` / 旧側 `old_no`）。
/// 同じ行番号は最初の行インデックスを採る（従来の線形走査の先勝ちと同じ）。
struct FileLineMaps {
    new_no: HashMap<u32, usize>,
    old_no: HashMap<u32, usize>,
}

/// 指定ファイル区間の行番号逆引き表を構築する。
fn build_file_line_maps(parsed: &ParsedDiff, file_index: usize) -> FileLineMaps {
    let mut new_no = HashMap::new();
    let mut old_no = HashMap::new();
    if let Some(file) = parsed.files.get(file_index) {
        for (i, line) in parsed.lines[file.start..file.end].iter().enumerate() {
            let index = file.start + i;
            if let Some(n) = line.new_no {
                new_no.entry(n).or_insert(index);
            }
            if let Some(o) = line.old_no {
                old_no.entry(o).or_insert(index);
            }
        }
    }
    FileLineMaps { new_no, old_no }
}

/// inline コメントの `(ファイル添字, unified 行インデックス)` を解決する。
///
/// `to`→`new_no` / `from`→`old_no` を前計算した逆引き表で照合し、見つからなければファイル
/// 先頭行へフォールバックする。パスが `parsed.files` に無い場合は `None`（Diff に出さない）。
fn comment_line_anchor(
    parsed: &ParsedDiff,
    comment: &Comment,
    path_to_file: &HashMap<&str, usize>,
    file_line_maps: &mut HashMap<usize, FileLineMaps>,
) -> Option<(usize, usize)> {
    let inline = comment.inline.as_ref()?;
    let path = inline.path.as_deref()?;
    let file_index = *path_to_file.get(path)?;
    let maps = file_line_maps
        .entry(file_index)
        .or_insert_with(|| build_file_line_maps(parsed, file_index));
    if let Some(to) = inline.to
        && let Ok(to) = u32::try_from(to)
        && let Some(&index) = maps.new_no.get(&to)
    {
        return Some((file_index, index));
    }
    if let Some(from) = inline.from
        && let Ok(from) = u32::try_from(from)
        && let Some(&index) = maps.old_no.get(&from)
    {
        return Some((file_index, index));
    }
    let start = parsed.files.get(file_index).map_or(0, |file| file.start);
    Some((file_index, start))
}

/// Diff 画面内のフォーカス（ファイル一覧サイドバー / 本文）。`Tab` で切り替える。
///
/// 既定は `Body`（サイドバー導入前の挙動＝矢印キーが直接本文をスクロールする、を維持する
/// ため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffFocus {
    /// ファイル一覧サイドバー。↑↓/jk でファイル選択（本文の `scroll` が追従する）。
    Files,
    /// 差分本文。既存の ↑↓/jk PgUp/PgDn g/G スクロール。
    #[default]
    Body,
}

/// Diff 画面の本文表示モード（`v` で切替、`config.toml` の `diff_view` に永続化）。
///
/// [`DiffState::cursor`]/`scroll` はどちらのモードでも「現在アクティブなモードの行列
/// （unified なら `ParsedDiff::lines`、split なら `ParsedDiff::split_lines`）上のインデックス」
/// として扱う（`DiffState::total_lines`/`active_file_starts` がモードに応じて切り替える）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffViewMode {
    /// 通常のユニファイド diff 表示（1 カラム）。
    #[default]
    Unified,
    /// 左=旧ファイル/右=新ファイルの並列表示（2 カラム）。
    Split,
}

impl DiffViewMode {
    /// `config.toml` へ保存する文字列表現。
    pub const fn as_str(self) -> &'static str {
        match self {
            DiffViewMode::Unified => "unified",
            DiffViewMode::Split => "split",
        }
    }

    /// `config.toml` から読み込んだ文字列をモードへ変換する。未知の値は既定（unified）。
    pub fn from_config_str(value: &str) -> DiffViewMode {
        match value {
            "split" => DiffViewMode::Split,
            _ => DiffViewMode::Unified,
        }
    }

    /// もう一方のモードへ切り替える（`v`）。
    fn toggled(self) -> DiffViewMode {
        match self {
            DiffViewMode::Unified => DiffViewMode::Split,
            DiffViewMode::Split => DiffViewMode::Unified,
        }
    }
}

/// Diff サイドバー（ファイル一覧）の既定幅比率。[`App::diff_sidebar_width`] が未ドラッグ
/// （`None`）のときに使う。右の本文が主ペインなので控えめに 30%。
pub const DIFF_SIDEBAR_DEFAULT_PERCENT: u16 = 30;

/// マウスドラッグでサイドバーを縮められる下限（セル数）。ドラッグでこれ未満まで縮めると
/// 自動的に非表示へ切り替える（[`App::on_mouse_drag`]。ユーザ要望「一点以上小さくしたら
/// 非表示に」）。
pub const DIFF_SIDEBAR_MIN_WIDTH: u16 = 12;

/// マウスドラッグでサイドバーを広げられる上限（全体幅に対する割合、%）。
const DIFF_SIDEBAR_MAX_PERCENT: u16 = 70;

/// Diff 画面全体の幅 `total`（セル数）と、保存済みの希望幅 `desired`
/// （`config.toml` の `diff_sidebar_width`。`None` は未ドラッグ＝既定比率）から、実際に
/// 描画すべきサイドバー幅（セル数）を求める。`[DIFF_SIDEBAR_MIN_WIDTH, 全体の
/// DIFF_SIDEBAR_MAX_PERCENT%]` へクランプする（`total` が極端に狭く上限が下限を下回る場合は
/// 上限を優先する。下限を割り込んだからといって非表示にはしない＝非表示判定はドラッグ操作
/// 側 [`App::on_mouse_drag`] の責務であり、ここは純粋な描画幅の算出のみを担う）。
pub fn resolve_diff_sidebar_width(total: u16, desired: Option<u16>) -> u16 {
    let max_width = ((u32::from(total) * u32::from(DIFF_SIDEBAR_MAX_PERCENT)) / 100) as u16;
    let max_width = max_width.min(total);
    let min_width = DIFF_SIDEBAR_MIN_WIDTH.min(max_width);
    let base = desired.unwrap_or_else(|| {
        ((u32::from(total) * u32::from(DIFF_SIDEBAR_DEFAULT_PERCENT)) / 100) as u16
    });
    base.clamp(min_width, max_width)
}

/// Diff 画面の表示状態（スクロール・現在行カーソル・ファイル境界ジャンプ・サイドバー選択）。
#[derive(Debug, Clone, Default)]
pub struct DiffState {
    pub parsed: ParsedDiff,
    /// 先頭からのスクロール行数（現在行 `cursor` が常に viewport 内に入るよう
    /// `ensure_cursor_visible` が自動調整する。直接ユーザ操作で動かすことはもう無い）。
    pub scroll: usize,
    /// 直近描画時のビューポート高さ（スクロール上限計算に使う。`ui` が毎フレーム更新）。
    pub viewport: usize,
    /// 見出し（例: `#12`）。
    pub title: String,
    /// 着色済み行（`ratatui::text::Line`）の遅延キャッシュ。
    ///
    /// `parsed` から毎フレーム再構築すると全行ぶんのヒープ確保が走るため、初回描画時に
    /// `ui` 側が一度だけ構築して書き戻す。新しい diff をロードした際は `DiffState` 自体を
    /// 作り直す（`rendered_lines: None` で始まる）ため、別途の無効化ロジックは不要。
    /// 現在行ハイライトはこのキャッシュに焼き込まず、描画時に viewport 内の該当行だけ
    /// スタイルを上書きする（`ui::render_diff_body`）。
    pub rendered_lines: Option<Vec<Line<'static>>>,
    /// split 表示（左=旧ファイル/右=新ファイル）用の着色済み行ペアの遅延キャッシュ。
    /// `rendered_lines` と同じ理由・同じ無効化ルール（テーマ変更時に `None` へ戻す）で
    /// 一度だけ構築し使い回す（`ui::render_diff_body_split`）。要素は
    /// `parsed.split_lines` と同じ並び・同じ長さ（`(左ペインの行, 右ペインの行)`）。
    pub rendered_split: Option<Vec<(Line<'static>, Line<'static>)>>,
    /// サイドバーで選択中のファイルインデックス（`parsed.files` へのインデックス）。
    ///
    /// `next_file`/`prev_file`（`n`/`N`）とサイドバー選択（`Tab` でフォーカス移動後の
    /// ↑↓/jk）の双方から更新され、常に `scroll` と同期する（本文側からジャンプしても
    /// サイドバーの選択が追従し、その逆も成り立つ）。
    pub file_index: usize,
    /// 現在行（本文フォーカス時の ↑↓/jk/Shift+J/K/PgUp/PgDn/g/G/n/N が動かす「今見ている行」）。
    /// `comment_anchor`/位置表示（`ファイルパス:行番号`）の基準にもなる。`view_mode` に応じて
    /// unified 行インデックス（[`ParsedDiff::lines`]）か split 行インデックス
    /// （[`ParsedDiff::split_lines`]）のどちらかを指す（[`DiffState::total_lines`] 参照）。
    pub cursor: usize,
    /// 画面内フォーカス（ファイル一覧 / 本文）。
    pub focus: DiffFocus,
    /// 本文の表示モード（unified / split）。`v` で切替。
    pub view_mode: DiffViewMode,
    /// Diff 本文へインライン表示するコメントスレッド配置（`DiffLoaded`/`CommentsLoaded` 時に
    /// [`build_comment_layout`] で再構築）。unified 表示でのみ描画・行数計算に使う。
    pub comment_layout: CommentLayout,
    /// サイドバーのフォルダ階層ツリー表示行列（`DiffLoaded` 時に [`build_sidebar_rows`] で構築）。
    /// 空のときは `parsed.files` のフラット順にフォールバックする。
    pub sidebar_rows: Vec<SidebarRow>,
    /// 本文の表示行列（diff 行 + インラインコメント行）。`scroll`/`cursor` はこの添字。
    /// `comment_layout`/`view_mode` から [`DiffState::rebuild_display_rows`] で組む。空のときは
    /// diff 行と 1:1 とみなす（コメント無し・未構築のフォールバック）。
    pub display_rows: Vec<DisplayRow>,
    /// スレッドのコラプス状態の手動上書き（key = ルートコメント id）。無指定のスレッドは
    /// 「解決済みならコラプス」が既定。Enter/クリックでトグルする。
    pub thread_collapse: HashMap<u64, bool>,
}

impl DiffState {
    /// 現在モードの diff 行数（`display_rows` を持たないときの 1:1 フォールバック用）。
    fn diff_line_count(&self) -> usize {
        match self.view_mode {
            DiffViewMode::Unified => self.parsed.lines.len(),
            DiffViewMode::Split => self.parsed.split_lines.len(),
        }
    }

    /// 表示行の総数（`scroll`/`cursor` の範囲）。`display_rows` が空なら diff 行と 1:1。
    fn total_rows(&self) -> usize {
        if self.display_rows.is_empty() {
            self.diff_line_count()
        } else {
            self.display_rows.len()
        }
    }

    /// 現在のモードでアクティブなファイル境界（diff 行インデックス列）。
    fn active_file_starts(&self) -> &[usize] {
        match self.view_mode {
            DiffViewMode::Unified => &self.parsed.file_starts,
            DiffViewMode::Split => &self.parsed.split_file_starts,
        }
    }

    /// `comment_layout`/`view_mode` から表示行列（[`DisplayRow`]）を組み直す。diff 行の順に、
    /// その行にアンカーされたスレッドのボックス行を差し込む。split の 1 行は旧側・新側の
    /// 両 unified 行のコメントを集める（置換の削除側コメントを落とさない）。
    pub fn rebuild_display_rows(&mut self) {
        let anchor_lines: Vec<Vec<usize>> = match self.view_mode {
            DiffViewMode::Unified => (0..self.parsed.lines.len()).map(|i| vec![i]).collect(),
            DiffViewMode::Split => self
                .parsed
                .split_lines
                .iter()
                .map(|row| {
                    let mut lines = Vec::new();
                    if let Some(left) = row.left {
                        lines.push(left);
                    }
                    if let Some(right) = row.right {
                        lines.push(right);
                    }
                    lines
                })
                .collect(),
        };
        self.display_rows =
            build_display_rows(&anchor_lines, &self.comment_layout, &self.thread_collapse);
        self.snap_cursor_focusable();
    }

    /// 指定表示行がカーソル停止対象か（diff 行 or コメントのヘッダ）。空なら全て diff 行。
    fn is_focusable(&self, index: usize) -> bool {
        match self.display_rows.get(index) {
            Some(row) => row.is_focusable(),
            None => index < self.diff_line_count(),
        }
    }

    /// 指定表示行が指す diff 行インデックス（Comment 行は `None`）。空なら 1:1。
    fn row_diff_index(&self, index: usize) -> Option<usize> {
        match self.display_rows.get(index) {
            Some(row) => row.diff_index(),
            None => (index < self.diff_line_count()).then_some(index),
        }
    }

    /// diff 行インデックスに対応する表示行インデックス（`display_rows` 内で最初の `Diff(line)`）。
    /// 空なら 1:1。見つからなければ末尾へクランプ。
    fn display_index_for_diff(&self, diff_line: usize) -> usize {
        if self.display_rows.is_empty() {
            return diff_line.min(self.total_rows().saturating_sub(1));
        }
        self.display_rows
            .iter()
            .position(|row| row.diff_index() == Some(diff_line))
            .unwrap_or_else(|| self.total_rows().saturating_sub(1))
    }

    /// スクロールの上限（表示行数 − viewport）。
    pub fn max_scroll(&self) -> usize {
        self.total_rows().saturating_sub(self.viewport.max(1))
    }

    /// 現在行（`cursor`）を `delta` ステップ移動する（focusable な表示行だけを辿る）。
    fn move_cursor(&mut self, delta: i64) {
        let total = self.total_rows();
        if total == 0 {
            return;
        }
        let step: i64 = if delta >= 0 { 1 } else { -1 };
        let mut remaining = delta.abs();
        let mut idx = self.cursor.min(total - 1) as i64;
        while remaining > 0 {
            let mut next = idx + step;
            while next >= 0 && (next as usize) < total && !self.is_focusable(next as usize) {
                next += step;
            }
            if next < 0 || next as usize >= total {
                break;
            }
            idx = next;
            remaining -= 1;
        }
        self.cursor = idx.clamp(0, total as i64 - 1) as usize;
        self.snap_cursor_focusable();
        self.ensure_cursor_visible();
    }

    /// カーソルが非 focusable 行に居るとき、最寄りの focusable 行へ寄せる。
    fn snap_cursor_focusable(&mut self) {
        let total = self.total_rows();
        if total == 0 {
            self.cursor = 0;
            return;
        }
        if self.cursor >= total {
            self.cursor = total - 1;
        }
        if self.is_focusable(self.cursor) {
            return;
        }
        for offset in 1..total {
            if self.cursor >= offset && self.is_focusable(self.cursor - offset) {
                self.cursor -= offset;
                return;
            }
            if self.cursor + offset < total && self.is_focusable(self.cursor + offset) {
                self.cursor += offset;
                return;
            }
        }
    }

    /// 現在行を先頭へ（`g`/`Home`）。
    fn cursor_to_top(&mut self) {
        self.cursor = 0;
        self.snap_cursor_focusable();
        self.ensure_cursor_visible();
    }

    /// 現在行を末尾へ（`G`/`End`）。最後の **diff 行** へ着地する（末尾コメント行ではなく、
    /// 最後のコード行で `c`（新規コメント）が使えるように）。
    fn cursor_to_bottom(&mut self) {
        let last_diff = self.diff_line_count().saturating_sub(1);
        self.cursor = self.display_index_for_diff(last_diff);
        self.snap_cursor_focusable();
        self.ensure_cursor_visible();
    }

    /// 現在行を 1 画面ぶん移動する（`PgUp`/`PgDn`）。focusable の数ではなく **表示行数** で
    /// 動かす（コメント行を挟んでも 1 キーで 1 画面ぶんに収める）。
    fn page_cursor(&mut self, delta: i64) {
        let total = self.total_rows();
        if total == 0 {
            return;
        }
        let next = (self.cursor as i64 + delta).clamp(0, total as i64 - 1);
        self.cursor = next as usize;
        self.snap_cursor_focusable();
        self.ensure_cursor_visible();
    }

    /// 現在行が可視範囲 `[scroll, scroll+viewport)` に入るよう `scroll` を最小限だけ動かす。
    fn ensure_cursor_visible(&mut self) {
        let viewport = self.viewport.max(1);
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll.saturating_add(viewport) {
            self.scroll = self.cursor + 1 - viewport;
        }
        self.scroll = self.scroll.min(self.max_scroll());
    }

    /// カーソル位置に対応する diff 行インデックス（現モード）。コメント行にいるときは直前の
    /// diff 行（そのコメントのアンカー行）を採る。
    fn cursor_diff_line(&self) -> usize {
        let mut idx = self.cursor.min(self.total_rows().saturating_sub(1));
        loop {
            if let Some(diff_index) = self.row_diff_index(idx) {
                return diff_index;
            }
            if idx == 0 {
                return 0;
            }
            idx -= 1;
        }
    }

    /// 次のファイル境界へジャンプする（`n`）。diff 行空間（単調）で比較するので O(files)。
    fn next_file(&mut self) {
        let current = self.cursor_diff_line();
        let found = self
            .active_file_starts()
            .iter()
            .enumerate()
            .find(|&(_, &start)| start > current)
            .map(|(index, &start)| (index, start));
        if let Some((index, start)) = found {
            self.jump_to_file(index, start);
        }
    }

    /// 前のファイル境界へジャンプする（`N`）。diff 行空間で比較するので O(files)。
    fn prev_file(&mut self) {
        let current = self.cursor_diff_line();
        let found = self
            .active_file_starts()
            .iter()
            .enumerate()
            .rev()
            .find(|&(_, &start)| start < current)
            .map(|(index, &start)| (index, start));
        if let Some((index, start)) = found {
            self.jump_to_file(index, start);
        }
    }

    /// サイドバーで指定インデックスのファイルを選択し、本文をその先頭行へ合わせる。
    fn select_file(&mut self, index: usize) {
        let start = match self.view_mode {
            DiffViewMode::Unified => self.parsed.files.get(index).map(|file| file.start),
            DiffViewMode::Split => self.parsed.split_file_starts.get(index).copied(),
        };
        if let Some(start) = start {
            self.jump_to_file(index, start);
        }
    }

    /// `file_index`/`scroll`/`cursor` を指定ファイルの先頭行（表示行）へまとめて同期する。
    fn jump_to_file(&mut self, index: usize, start: usize) {
        self.file_index = index;
        let display = self.display_index_for_diff(start);
        self.cursor = display.min(self.total_rows().saturating_sub(1));
        self.snap_cursor_focusable();
        self.scroll = self.cursor.min(self.max_scroll());
    }

    /// サイドバー選択を 1 つ下へ（末尾で停止）。
    /// サイドバーのツリー表示順に並べたファイル添字列。`sidebar_rows` が空（未構築）の
    /// ときは `parsed.files` のフラット順にフォールバックする。
    fn sidebar_file_order(&self) -> Vec<usize> {
        if self.sidebar_rows.is_empty() {
            (0..self.parsed.files.len()).collect()
        } else {
            self.sidebar_rows
                .iter()
                .filter_map(SidebarRow::file_index)
                .collect()
        }
    }

    /// 現在選択中ファイル（`file_index`）のサイドバー表示行インデックス。`sidebar_rows` が
    /// 空、または未検出のときは `file_index` そのもの（フラット順）へフォールバックする。
    pub fn selected_sidebar_row(&self) -> usize {
        self.sidebar_rows
            .iter()
            .position(|row| row.file_index() == Some(self.file_index))
            .unwrap_or(self.file_index)
    }

    /// サイドバー選択を表示順で 1 つ下へ（末尾で停止）。
    fn select_file_next(&mut self) {
        let order = self.sidebar_file_order();
        match order.iter().position(|&fi| fi == self.file_index) {
            Some(pos) => {
                if let Some(&next) = order.get(pos + 1) {
                    self.select_file(next);
                }
            }
            None => {
                if let Some(&first) = order.first() {
                    self.select_file(first);
                }
            }
        }
    }

    /// サイドバー選択を表示順で 1 つ上へ（先頭で停止）。
    fn select_file_prev(&mut self) {
        let order = self.sidebar_file_order();
        match order.iter().position(|&fi| fi == self.file_index) {
            Some(pos) if pos > 0 => {
                if let Some(&prev) = order.get(pos - 1) {
                    self.select_file(prev);
                }
            }
            None => {
                if let Some(&first) = order.first() {
                    self.select_file(first);
                }
            }
            _ => {}
        }
    }

    /// Diff 画面内のフォーカスを切り替える（`Tab`）。
    fn toggle_focus(&mut self) {
        self.focus = match self.focus {
            DiffFocus::Files => DiffFocus::Body,
            DiffFocus::Body => DiffFocus::Files,
        };
    }

    /// 本文の表示モードを `mode` へ切り替える（`v`）。カーソルが乗っていたコメント（あれば）と
    /// unified 行を覚えておき、作り直したあと同じコメント／行へ戻す。既に同じモードなら何もしない。
    fn set_view_mode(&mut self, mode: DiffViewMode) {
        if self.view_mode == mode {
            return;
        }
        let prev_comment = self.cursor_comment();
        let unified = self.cursor_unified_line();
        self.view_mode = mode;
        self.rebuild_display_rows();
        self.reanchor_cursor(prev_comment, unified);
        self.ensure_cursor_visible();
    }

    /// 表示行列を作り直したあと、カーソルを元の位置へ戻す。まず同じコメント（返信/編集後も
    /// そのコメントに留まる）、次に同じスレッドの focusable 行（返信ヘッダに居たまま自動
    /// コラプスされた場合のコラプス行）を探し、無ければアンカーの diff 行へ寄せる。
    fn reanchor_cursor(&mut self, prev_comment: Option<(u64, u64)>, unified: Option<usize>) {
        if let Some((thread_root, comment_id)) = prev_comment {
            if let Some(index) = self.display_rows.iter().position(|row| {
                matches!(
                    row,
                    DisplayRow::Comment(CommentRow {
                        comment_id: Some(id),
                        kind: CommentRowKind::Header { .. } | CommentRowKind::Collapsed { .. },
                        ..
                    }) if *id == comment_id
                )
            }) {
                self.cursor = index;
                self.snap_cursor_focusable();
                return;
            }
            if let Some(index) = self.display_rows.iter().position(|row| {
                matches!(row, DisplayRow::Comment(comment_row) if comment_row.thread_root == thread_root)
                    && row.is_focusable()
            }) {
                self.cursor = index;
                self.snap_cursor_focusable();
                return;
            }
        }
        if let Some(unified) = unified {
            self.reanchor_cursor_to_unified(unified);
        }
    }

    /// 指定 unified 行に対応する表示行へカーソルを移す（現モード基準）。
    fn reanchor_cursor_to_unified(&mut self, unified: usize) {
        let diff_line = match self.view_mode {
            DiffViewMode::Unified => Some(unified),
            DiffViewMode::Split => self
                .parsed
                .split_lines
                .iter()
                .position(|row| row.left == Some(unified) || row.right == Some(unified)),
        };
        if let Some(diff_line) = diff_line {
            self.cursor = self.display_index_for_diff(diff_line);
        }
        self.snap_cursor_focusable();
    }

    /// カーソル位置（表示行）に対応する unified 行インデックス。コメント行にいるときは直前の
    /// diff 行（＝そのコメントのアンカー行）を採る。
    fn cursor_unified_line(&self) -> Option<usize> {
        let mut idx = self.cursor.min(self.total_rows().saturating_sub(1));
        loop {
            if let Some(diff_index) = self.row_diff_index(idx) {
                return Some(self.diff_index_to_unified(diff_index));
            }
            if idx == 0 {
                return None;
            }
            idx -= 1;
        }
    }

    /// diff 行インデックス（現モード）を unified 行インデックスへ変換する。
    fn diff_index_to_unified(&self, diff_index: usize) -> usize {
        match self.view_mode {
            DiffViewMode::Unified => diff_index,
            DiffViewMode::Split => self
                .parsed
                .split_lines
                .get(diff_index)
                .and_then(|row| row.right.or(row.left))
                .unwrap_or(diff_index),
        }
    }

    /// 現在行が属するファイルの表示名。
    pub fn current_file(&self) -> Option<&str> {
        let unified = self.cursor_unified_line()?;
        self.parsed.file_for_line(unified)
    }

    /// 現在行のインラインコメント投稿アンカー（diff 行にカーソルがあるときのみ）。コメント行や
    /// メタ/ヘッダ行では `None`。
    pub fn current_comment_anchor(&self) -> Option<CommentAnchor> {
        let diff_index = self.row_diff_index(self.cursor)?;
        match self.view_mode {
            DiffViewMode::Unified => self.parsed.comment_anchor(diff_index),
            DiffViewMode::Split => self.parsed.split_comment_anchor(diff_index),
        }
    }

    /// カーソルがコメントのヘッダにあるとき `(スレッドのルート id, コメント id)` を返す。
    /// Reply/Edit/Delete/Resolve の対象。
    pub fn cursor_comment(&self) -> Option<(u64, u64)> {
        match self.display_rows.get(self.cursor) {
            Some(DisplayRow::Comment(row)) => row
                .comment_id
                .map(|comment_id| (row.thread_root, comment_id)),
            _ => None,
        }
    }

    /// カーソルが乗っている行のスレッド（ルート id）。コメント行ならその thread_root。
    pub fn cursor_thread(&self) -> Option<u64> {
        match self.display_rows.get(self.cursor) {
            Some(DisplayRow::Comment(row)) => Some(row.thread_root),
            _ => None,
        }
    }

    /// 指定スレッドが解決済みか（`comment_layout` から引く。未検出は `false`）。
    pub fn thread_resolved(&self, root: u64) -> bool {
        self.comment_layout
            .threads_by_line
            .values()
            .flatten()
            .find(|thread| thread.root_id == root)
            .is_some_and(|thread| thread.resolved)
    }

    /// 指定スレッドのコラプス状態（手動上書きが無ければ「解決済みならコラプス」）。
    fn thread_collapsed(&self, root: u64) -> bool {
        self.thread_collapse
            .get(&root)
            .copied()
            .unwrap_or_else(|| self.thread_resolved(root))
    }

    /// Diff 本文ペイン内のクリック位置（絶対座標）を表示行へ写し、カーソル移動やコラプスの
    /// トグルを行う（ペイン枠線 1 行ぶんを差し引く。アクションリンクのクリックは `App` 側が
    /// `layout.comment_actions` で先に処理する）。
    pub fn click_body_row(&mut self, area: Rect, point: (u16, u16)) {
        let inner_top = area.y.saturating_add(1);
        let rows = usize::from(area.height.saturating_sub(2));
        if point.1 < inner_top || usize::from(point.1 - inner_top) >= rows {
            return;
        }
        let index = self.scroll + usize::from(point.1 - inner_top);
        if index >= self.total_rows() {
            return;
        }
        match self.display_rows.get(index).cloned() {
            // diff 行（またはフォールバックの 1:1）はその行へカーソル移動。
            None | Some(DisplayRow::Diff(_)) => {
                self.cursor = index;
                self.snap_cursor_focusable();
                self.ensure_cursor_visible();
            }
            Some(DisplayRow::Comment(row)) => {
                if matches!(row.kind, CommentRowKind::Spacer) {
                    // スレッド間の空行は無操作（枠上下端の「スレッド先頭選択」に落とさない）。
                } else if matches!(row.kind, CommentRowKind::Collapsed { .. }) {
                    // コラプス行のクリックは展開。
                    self.toggle_thread_collapse(row.thread_root);
                } else if let Some(comment_id) = row.comment_id {
                    // ヘッダ/本文/アクション行はそのコメントを選択。
                    self.reanchor_cursor(Some((row.thread_root, comment_id)), None);
                    self.ensure_cursor_visible();
                } else if let Some(i) = self.display_rows.iter().position(|r| {
                    matches!(r, DisplayRow::Comment(cr) if cr.thread_root == row.thread_root)
                        && r.is_focusable()
                }) {
                    // 枠上下端はスレッド先頭のコメントを選択。
                    self.cursor = i;
                    self.ensure_cursor_visible();
                }
            }
        }
    }

    /// 指定スレッドのコラプスをトグルし、表示行列を組み直してカーソルをそのスレッドの
    /// 先頭（focusable 行）へ合わせる（Enter/クリック）。
    pub fn toggle_thread_collapse(&mut self, root: u64) {
        let collapsed = self.thread_collapsed(root);
        self.thread_collapse.insert(root, !collapsed);
        self.rebuild_display_rows();
        if let Some(index) = self.display_rows.iter().position(|row| match row {
            DisplayRow::Comment(comment_row) => {
                comment_row.thread_root == root && row.is_focusable()
            }
            DisplayRow::Diff(_) => false,
        }) {
            self.cursor = index;
        }
        self.snap_cursor_focusable();
        self.ensure_cursor_visible();
    }
}

/// Source（ソースツリー閲覧）画面の状態。
///
/// 現在の `reference`（ブランチ名/ハッシュ）と `path`（ルートからのディレクトリパス。
/// 空文字がルート）を保持し、ディレクトリ列挙を選択リストで表示する。`path` は
/// API 応答由来ではなく TUI 自身が `open_source`/`source_enter`/`source_up` で
/// 一貫して更新する自己追跡状態（`child_path`/`parent_dir` はこの `path` にのみ
/// 依存し、`SrcEntry.path` の値には依存しない）。
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
    /// フィルタ済みのマウス入力（左押下・ホイールのみ）。
    Mouse(MouseEvent),
    /// Onboarding の認証検証に成功（`GET /2.0/user`）。
    AuthValidated {
        email: String,
        token: String,
        user: User,
    },
    /// Onboarding の認証検証に失敗。
    AuthFailed(ApiError),
    /// ワークスペース一覧（1 ページ分）の取得完了。
    WorkspacesLoaded {
        workspaces: Vec<Workspace>,
        page_info: PageInfo,
    },
    /// リポジトリ一覧（1 ページ分）の取得完了。
    RepositoriesLoaded {
        workspace: String,
        /// リクエスト時に指定していたソート順（ステイル判定・キャッシュキーに使う）。
        sort: ListSort,
        repos: Vec<Repository>,
        page_info: PageInfo,
    },
    /// ワークスペース/リポジトリ取得の失敗。
    LoadFailed(ApiError),
    /// PR 一覧（1 ページ分）の取得完了。
    PullRequestsLoaded {
        repo: String,
        filter: PrStateFilter,
        /// リクエスト時に指定していたソート順（ステイル判定・キャッシュキーに使う）。
        sort: ListSort,
        prs: Vec<PullRequest>,
        page_info: PageInfo,
    },
    /// PR フィルタの author 候補ソース（この repo の PR 集約）の取得完了/失敗。
    ///
    /// 失敗はフィルタモーダル内のフォールバック（読み込み済み PR の author）に切り替える
    /// 材料になるため、`Msg::LoadFailed`（Status エラー表示）とは分けて運ぶ。
    PrAuthorsLoaded {
        repo_full_name: String,
        result: Result<Vec<PullRequest>, ApiError>,
    },
    /// PR フィルタの target branch 候補（ブランチ一覧の 1 ページ目）の取得完了/失敗。
    ///
    /// 失敗時は候補なし（自由入力の部分一致のみ）へ確定させるため、`Msg::LoadFailed`
    /// （Status エラー表示）とは分けて運ぶ。
    FilterBranchesLoaded {
        repo_full_name: String,
        result: Result<Vec<Branch>, ApiError>,
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
    /// コメントの編集/削除/解決トグルの成功（`message` を Status に出す）。
    CommentActionDone { id: u64, message: String },
    /// merge の成功（202 の「処理中」を含む）。
    MergeDone { id: u64 },
    /// レビュー系アクション（approve/comment/merge 等）の失敗。
    ActionFailed(ApiError),
    /// 自動ポーリングのタイマ tick（進行中パイプラインの定期リフレッシュ）。
    Tick,
    /// パイプライン一覧（1 ページ分）の取得完了。
    PipelinesLoaded {
        repo: String,
        pipelines: Vec<Pipeline>,
        page_info: PageInfo,
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
    /// ブランチ一覧（1 ページ分）の取得完了。
    BranchesLoaded {
        repo: String,
        branches: Vec<Branch>,
        page_info: PageInfo,
    },
    /// コミット履歴の 1 ページの取得完了。
    CommitsLoaded {
        revision: Option<String>,
        commits: Vec<Commit>,
        /// 次ページ取得用の `next` URL（無ければ次ページなし）。
        next: Option<String>,
        /// このページのページ番号（現在ページとの照合に使う）。
        page: u32,
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
    /// PR 本文内の画像の取得完了（`result` は生バイト、デコードは `update()` 側で行う）。
    /// 古い（もう表示していない）URL の結果は無視する（`App::current_image` への反映のみ
    /// ガードする。キャッシュ自体は常に最新化する）。
    ImageLoaded {
        url: String,
        result: Result<Vec<u8>, String>,
    },
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
    /// ワークスペース一覧の指定ページを取得する（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    LoadWorkspaces { client: BitbucketClient, page: u32 },
    /// 指定ワークスペースのリポジトリ一覧の指定ページを、指定ソート順で取得する。
    LoadRepositories {
        client: BitbucketClient,
        workspace: String,
        sort: ListSort,
        page: u32,
    },
    /// PR 一覧の指定ページを、指定ソート順で取得する。
    LoadPullRequests {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        filter: PrStateFilter,
        sort: ListSort,
        page: u32,
    },
    /// PR フィルタの author 候補ソース（この repo の PR。全 state・最新順・最大 3 ページ）を
    /// 取得する。`repo_full_name` は候補キャッシュのキー（応答の照合にも使う）。
    LoadPrAuthors {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        repo_full_name: String,
    },
    /// PR フィルタの target branch 候補（ブランチ一覧の 1 ページ目）を取得する。
    /// `repo_full_name` は候補キャッシュのキー（応答の照合にも使う）。
    LoadFilterBranches {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        repo_full_name: String,
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
    /// インラインコメント（Diff 画面の特定行への新規スレッド）を投稿する。
    CreateInlineComment {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        path: String,
        side: CommentSide,
        line: u32,
        raw: String,
    },
    /// 既存スレッドへの返信を投稿する（`parent_id` は返信先スレッドのルートコメント id）。
    CreateReply {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        parent_id: u64,
        raw: String,
    },
    /// 既存コメントの本文を編集する（`PUT .../comments/{comment_id}`）。
    EditComment {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        comment_id: u64,
        raw: String,
    },
    /// 既存コメントを削除する（`DELETE .../comments/{comment_id}`）。
    DeleteComment {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        comment_id: u64,
    },
    /// スレッドの解決/再オープンをトグルする（`comment_id` はスレッドのルート）。
    ResolveComment {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        comment_id: u64,
        resolve: bool,
    },
    /// PR をマージする。
    Merge {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        id: u64,
        params: MergeParams,
    },
    /// パイプライン一覧の指定ページを取得する（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    LoadPipelines {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        page: u32,
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
    /// ブランチ一覧の指定ページを取得する（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    LoadBranches {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        page: u32,
    },
    /// コミット履歴の 1 ページを取得する（`revision` 省略時は既定ブランチ）。
    ///
    /// commits は `page` 番号ジャンプ非対応のため `cursor`（前ページ応答の `next` URL、
    /// 先頭ページは `None`）で辿る。`page` は表示・応答照合用のページ番号。
    LoadCommits {
        client: BitbucketClient,
        workspace: String,
        repo: String,
        revision: Option<String>,
        cursor: Option<String>,
        page: u32,
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
    /// PR 本文内の画像を取得する（`url` は本文の Markdown から抽出した絶対 URL）。
    LoadImage {
        client: BitbucketClient,
        url: String,
    },
}

/// `filter` に対する fuzzy マッチで `items` のインデックス列を返す（`nucleo_matcher` 使用。
/// [`SelectList`] の検索と PR フィルタモーダルの author 検索が共有する経路）。
///
/// 空フィルタは全件（`items` の並び順そのまま = 恒等写像）。非空フィルタはスコア > 0 の
/// 要素のみをスコア降順（同点は `items` の順序を維持する安定ソート）で並べる。大文字小文字は
/// 常に無視する（クエリの大文字小文字によらず一貫させるため、既存の `to_lowercase` 部分一致と
/// 同じ挙動を保つ）。
fn fuzzy_match_indices<T, F>(
    filter: &str,
    items: &[T],
    key_fn: F,
    matcher: &mut Matcher,
) -> Vec<usize>
where
    F: Fn(&T) -> String,
{
    if filter.is_empty() {
        return (0..items.len()).collect();
    }
    let pattern = Pattern::parse(filter, CaseMatching::Ignore, Normalization::Smart);
    let mut buf = Vec::new();
    let mut scored: Vec<(usize, u32)> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let text = key_fn(item);
            let haystack = Utf32Str::new(&text, &mut buf);
            pattern.score(haystack, matcher).map(|score| (index, score))
        })
        .collect();
    scored.sort_by_key(|&(_, score)| std::cmp::Reverse(score));
    scored.into_iter().map(|(index, _)| index).collect()
}

/// 選択状態を持つリスト。ratatui の `ListState` を内包し、スクロールは List ウィジェットに委ねる。
///
/// `matches` は `items` のうちフィルタ（`filter`）を通過した要素のインデックスを表示順で
/// 保持する。`filter` が空文字なら `matches` は恒等写像（`0..items.len()`）になるため、
/// 検索を使わない画面（pipelines/branches/commits/source 等）は `filter`/`matches` に
/// 一切触れないままで従来と同じ挙動（全件・`items` のインデックスそのままの選択）になる。
/// 選択（`state`）は常に「`matches` 上の位置」を指す。
///
/// `T: Default` を要求しないよう `Default` は手動実装する。
///
/// `matcher` は fuzzy 検索（[`SelectList::set_filter`]）用のスクラッチメモリを再利用するために
/// 保持する（`nucleo_matcher::Matcher` は生成コストが高いため、フィルタ再計算のたびに作り直さ
/// ない）。検索を使わない画面（pipelines/branches/commits/source 等）では一度も使われない。
#[derive(Debug)]
pub struct SelectList<T> {
    pub items: Vec<T>,
    pub state: ListState,
    /// 検索フィルタ文字列（空ならフィルタなし）。検索を使わない画面では常に空のまま。
    pub filter: String,
    /// フィルタ通過した `items` のインデックス（表示順、スコア降順）。`ui` の一覧描画はこれを
    /// 辿る。フィルタが空なら `items` の並び順そのまま（恒等写像）。
    pub matches: Vec<usize>,
    matcher: Matcher,
}

impl<T> Default for SelectList<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            state: ListState::default(),
            filter: String::new(),
            matches: Vec::new(),
            matcher: Matcher::default(),
        }
    }
}

impl<T> SelectList<T> {
    /// 要素を差し替え、選択位置を先頭（空なら未選択）にリセットする。フィルタも解除する
    /// （新しいデータセットに古いフィルタを引き継がないため）。
    pub fn set_items(&mut self, items: Vec<T>) {
        self.filter.clear();
        self.matches = (0..items.len()).collect();
        self.state
            .select(if items.is_empty() { None } else { Some(0) });
        self.items = items;
    }

    /// 要素を差し替えつつ、選択インデックスを可能な限り維持する（新しい件数にクランプ）。
    ///
    /// 自動ポーリングでの一覧リフレッシュ時に、選択位置が毎回先頭へ戻らないようにするために使う。
    /// 検索を使わない画面専用（`filter` は常に空のまま・`matches` は恒等写像を保つ）。
    pub fn set_items_keep_selection(&mut self, items: Vec<T>) {
        self.matches = (0..items.len()).collect();
        let selection = if items.is_empty() {
            None
        } else {
            Some(self.state.selected().unwrap_or(0).min(items.len() - 1))
        };
        self.state.select(selection);
        self.items = items;
    }

    /// 要素を差し替えつつ、`key_fn` で選択中要素と一致するものを新しい一覧から探して選択する
    /// （見つからなければ現在のインデックスを新件数にクランプする＝`set_items_keep_selection`
    /// と同じフォールバック）。
    ///
    /// 同一文脈（同じ画面/フィルタ/ページ/ソート）の再検証結果が届いたときに使う。並び順が
    /// 変わり得る場合（サーバソート下で他者の更新により順序が変動する等）でも、識別子
    /// （slug/full_name/PR id 等）で同一アイテムを追従できるため、単純なインデックス維持より
    /// 頑健。検索を使わない画面専用（`set_items_keep_selection` と同じ制約）。
    pub fn set_items_keep_selection_by<F, K>(&mut self, items: Vec<T>, key_fn: F)
    where
        F: Fn(&T) -> K,
        K: PartialEq,
    {
        let current_key = self.selected().map(&key_fn);
        self.matches = (0..items.len()).collect();
        let selection = current_key
            .and_then(|key| items.iter().position(|item| key_fn(item) == key))
            .or_else(|| {
                if items.is_empty() {
                    None
                } else {
                    Some(self.state.selected().unwrap_or(0).min(items.len() - 1))
                }
            });
        self.state.select(selection);
        self.items = items;
    }

    /// 検索フィルタ文字列を更新し、`key_fn` が返す文字列に対する fuzzy マッチ（大文字小文字は
    /// 無視、スコア降順）で `matches` を再計算する。選択位置は新しい `matches` の範囲に
    /// クランプする。
    pub fn set_filter<F>(&mut self, filter: String, key_fn: F)
    where
        F: Fn(&T) -> String,
    {
        self.filter = filter;
        self.recompute_matches(key_fn);
    }

    /// `filter` に対する fuzzy マッチで `matches` を再計算する（[`fuzzy_match_indices`]）。
    /// 選択位置は新しい `matches` の範囲にクランプする。
    fn recompute_matches<F>(&mut self, key_fn: F)
    where
        F: Fn(&T) -> String,
    {
        self.matches = fuzzy_match_indices(&self.filter, &self.items, key_fn, &mut self.matcher);
        let selection = if self.matches.is_empty() {
            None
        } else {
            Some(
                self.state
                    .selected()
                    .unwrap_or(0)
                    .min(self.matches.len() - 1),
            )
        };
        self.state.select(selection);
    }

    /// 選択を 1 つ下へ（末尾で停止）。フィルタ未使用なら全件、使用中なら `matches` の範囲。
    pub fn select_next(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let next = match self.state.selected() {
            Some(index) if index + 1 < self.matches.len() => index + 1,
            Some(index) => index,
            None => 0,
        };
        self.state.select(Some(next));
    }

    /// 選択を 1 つ上へ（先頭で停止）。
    pub fn select_prev(&mut self) {
        if self.matches.is_empty() {
            return;
        }
        let prev = match self.state.selected() {
            Some(0) | None => 0,
            Some(index) => index - 1,
        };
        self.state.select(Some(prev));
    }

    /// 選択を `amount` 件下へ（末尾でクランプ）。`Shift+J` の 10 件移動に使う。
    pub fn select_next_by(&mut self, amount: usize) {
        if self.matches.is_empty() {
            return;
        }
        let next = match self.state.selected() {
            Some(index) => (index + amount).min(self.matches.len() - 1),
            None => 0,
        };
        self.state.select(Some(next));
    }

    /// 選択を `amount` 件上へ（先頭でクランプ）。`Shift+K` の 10 件移動に使う。
    pub fn select_prev_by(&mut self, amount: usize) {
        if self.matches.is_empty() {
            return;
        }
        let prev = match self.state.selected() {
            Some(index) => index.saturating_sub(amount),
            None => 0,
        };
        self.state.select(Some(prev));
    }

    /// 表示順（`matches` 上）の位置を直接選択する。
    pub fn select_position(&mut self, position: usize) {
        if !self.matches.is_empty() {
            self.state
                .select(Some(position.min(self.matches.len() - 1)));
        }
    }

    /// 現在選択中の要素（`matches` 上の位置を `items` のインデックスへ変換して引く）。
    pub fn selected(&self) -> Option<&T> {
        let position = self.state.selected()?;
        let index = *self.matches.get(position)?;
        self.items.get(index)
    }

    /// 表示順（フィルタ適用後）で要素を辿るイテレータ。`ui` の一覧描画に使う。
    pub fn visible(&self) -> impl Iterator<Item = &T> + '_ {
        self.matches.iter().map(move |&index| &self.items[index])
    }
}

/// 直近開いた PR（ジャンプパレットの候補に使う）。
#[derive(Debug, Clone)]
pub struct RecentPr {
    pub workspace: String,
    /// `workspace/repo` 形式（`App::selected_repo` と同じ形式）。
    pub repo_full_name: String,
    pub pr: PullRequest,
}

/// ジャンプパレットの候補が実行する遷移。
#[derive(Debug, Clone)]
pub enum JumpAction {
    /// 画面だけを切り替える（保持済みデータをそのまま使う）。
    Screen(Screen),
    /// 指定ワークスペースへ入る（未取得ならリポジトリ取得を発行）。
    Workspace(String),
    /// 指定リポジトリへ入る（PR 一覧取得を発行）。
    Repository(Box<Repository>),
    /// 直近開いた PR の詳細へ直接ジャンプする。
    RecentPr(Box<RecentPr>),
}

/// ジャンプパレットの 1 候補。
#[derive(Debug, Clone)]
pub struct JumpEntry {
    /// 一覧に表示するラベル。
    pub label: String,
    /// 検索対象文字列（`label` より緩く、複数の呼び名を含められる）。
    pub search_key: String,
    pub action: JumpAction,
}

/// ジャンプパレット（`Ctrl+K`）の状態。保持済みデータへの一気ジャンプ・画面ジャンプに使う。
#[derive(Debug)]
pub struct JumpPaletteState {
    pub entries: SelectList<JumpEntry>,
}

/// ページ番号ジャンプ（`g`）の入力状態。Workspaces/Repositories/PullRequests 共通。
///
/// 数字のみを受け付ける簡易バッファ（`on_key_page_jump` 参照）。`Enter` で確定し
/// [`App::goto_page`] を呼ぶ、`Esc` で取消。
#[derive(Debug, Clone, Default)]
pub struct PageJumpModal {
    pub input: String,
}

/// `JumpEntry` の検索キー抽出（`SelectList::set_filter`/`reorder` に渡す）。
fn jump_entry_key(entry: &JumpEntry) -> String {
    entry.search_key.clone()
}

/// 1 キャッシュ（[`RevisitCache`]）あたりの最大保持件数。
///
/// 超過時は最も古いキー（挿入順）を 1 件追い出す（FIFO）。ワークスペース/リポジトリ/PR は
/// セッション中に無制限に増え得るため、素の `HashMap` のままだと長時間のレビューセッションで
/// メモリが際限なく肥大する。真の LRU（アクセス順追跡）は実装コストの割に本用途では過剰と
/// 判断し、挿入順ベースの単純な FIFO 追い出しのみを採用する。
const REVISIT_CACHE_MAX_ENTRIES: usize = 30;

/// 画面再訪時の stale-while-revalidate キャッシュ用コンテナ。
///
/// 「再訪時にキャッシュがあれば即表示し、裏で再取得して届いたら差し替える」ために使う
/// 単純なキー・バリューストア。挿入順を [`VecDeque`] で追跡し、
/// [`REVISIT_CACHE_MAX_ENTRIES`] を超えたら最も古いキーを追い出す。
#[derive(Debug)]
pub struct RevisitCache<K, V> {
    map: HashMap<K, V>,
    order: VecDeque<K>,
}

impl<K, V> Default for RevisitCache<K, V> {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }
}

impl<K, V> RevisitCache<K, V>
where
    K: std::hash::Hash + Eq + Clone,
{
    /// キーに対応する値への参照を返す（キャッシュ命中判定に使う）。
    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    /// キーに対応する値への可変参照を返す（部分更新の upsert に使う）。
    fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key)
    }

    /// 挿入/上書きする。新規キーの追加で上限を超える場合は最も古いキーを 1 件追い出す
    /// （既存キーの上書きは追い出しを起こさない＝挿入順は保持する）。
    fn insert(&mut self, key: K, value: V) {
        if !self.map.contains_key(&key) {
            if self.order.len() >= REVISIT_CACHE_MAX_ENTRIES
                && let Some(oldest) = self.order.pop_front()
            {
                self.map.remove(&oldest);
            }
            self.order.push_back(key.clone());
        }
        self.map.insert(key, value);
    }

    /// 全エントリを走査する（順序は不定）。キーの部分一致（例: repo slug）でページや
    /// フィルタを跨いでエントリを集約したい場合に使う（[`App::loaded_pr_authors`] 参照）。
    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        self.map.iter()
    }

    /// 述語 `keep` が `false` を返すキーをすべて取り除く。
    ///
    /// キーの一部（例: repo slug）だけが一致すればページ番号を問わず一括無効化したい場合に使う
    /// （[`App::invalidate_pull_requests_cache_for_current_repo`] 参照。ページ番号までは
    /// 追跡していないため、`remove` を全ページ分呼ぶ代わりにこちらを使う）。
    fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&K) -> bool,
    {
        let removed: Vec<K> = self
            .order
            .iter()
            .filter(|key| !keep(key))
            .cloned()
            .collect();
        for key in removed {
            self.map.remove(&key);
        }
        self.order.retain(|key| keep(key));
    }
}

/// PR 詳細キャッシュの 1 エントリ。詳細・diffstat・コメントをまとめて保持し、
/// [`App::open_pr_detail_with`] での即時表示と 3 種の `Msg::*Loaded` からの部分更新に使う。
#[derive(Debug, Clone)]
pub struct PrDetailCache {
    pub pr: PullRequest,
    pub diffstat: Vec<DiffStatEntry>,
    pub comments: Vec<Comment>,
}

/// PR 一覧の再訪キャッシュの型（[`App::pull_requests_cache`]）。
///
/// キー（repo slug, state フィルタ, ソート, ページ番号）のタプルが 4 要素になり
/// `clippy::type_complexity` に抵触するため、型エイリアスへ切り出している。
type PullRequestsCache =
    RevisitCache<(String, PrStateFilter, ListSort, u32), (Vec<PullRequest>, PageInfo)>;

/// アプリ全体の状態。
pub struct App {
    pub screen: Screen,
    pub config: Config,
    pub client: Option<BitbucketClient>,
    /// 現在の配色テーマ（`ThemeName` から導出。`Ctrl+T` で巡回）。
    pub theme: Theme,
    /// 現在のテーマ名（`config.theme` の永続化・巡回の起点に使う）。
    pub theme_name: ThemeName,
    /// Diff 画面の表示モード（`config.diff_view` の永続化の起点。`v` で切替。新しく
    /// diff を読み込むたび、この値で `DiffState::view_mode` を初期化する）。
    pub diff_view_mode: DiffViewMode,
    /// Diff 画面のファイル一覧サイドバーの表示/非表示（`t` で切替。`config.diff_sidebar_visible`
    /// へ永続化。既定は表示）。非表示中は本文が全幅になり、`Tab` でのフォーカス移動もサイド
    /// バーへは行かない（[`App::on_key_diff`]）。
    pub diff_sidebar_visible: bool,
    /// Diff 画面のファイル一覧サイドバーの幅（セル数）。`None` は未ドラッグ＝既定比率
    /// （[`DIFF_SIDEBAR_DEFAULT_PERCENT`]）を使う。マウスドラッグで変更すると `Some` になり
    /// `config.diff_sidebar_width` へ永続化する（実際の描画幅は [`resolve_diff_sidebar_width`]
    /// が `DIFF_SIDEBAR_MIN_WIDTH`〜全体の70%へクランプする。[`App::diff_sidebar_render_width`]）。
    pub diff_sidebar_width: Option<u16>,
    /// Diff サイドバー境界のドラッグ中かどうか（マウス `Down`→`Drag`→`Up` の間だけ `true`）。
    /// ドラッグ中は他のクリック処理（一覧行選択等）を発火させない
    /// （[`App::on_mouse_left`]/[`App::on_mouse_drag`]/[`App::on_mouse_up`]）。
    pub diff_sidebar_dragging: bool,
    /// 直近描画フレームのペイン・一覧・モーダル・ヒント配置。
    pub layout: AppLayout,
    pub me: Me,
    pub onboarding: OnboardingState,
    pub workspaces: SelectList<Workspace>,
    /// Workspaces 一覧のページ状態（1 ページ = [`crate::api::client::PAGE_SIZE`] 件）。
    /// `[`/`]` でページ間移動、`g` でページ番号ジャンプ。
    pub workspaces_page_info: PageInfo,
    /// ワークスペース一覧の再訪キャッシュ（キー = ページ番号）。`[`/`]`/`g` でのページ移動と
    /// 画面再訪の双方で即時表示に使い、`Msg::WorkspacesLoaded` 受信のたびに最新化する
    /// （stale-while-revalidate）。
    pub workspaces_cache: RevisitCache<u32, (Vec<Workspace>, PageInfo)>,
    pub repositories: SelectList<Repository>,
    /// Repositories 一覧の現在のサーバソート順（`S` キーで巡回。Bitbucket ブラウザ版と同じ
    /// 4 種類）。切り替え時は 1 ページ目から再取得する（[`App::cycle_repositories_sort`]）。
    pub repositories_sort: ListSort,
    /// Repositories 一覧のページ状態。
    pub repositories_page_info: PageInfo,
    /// リポジトリ一覧の再訪キャッシュ（キー = (workspace slug, ソート, ページ番号)）。
    /// [`App::load_repositories_page`] が再訪時に即表示するために使い、`Msg::RepositoriesLoaded`
    /// 受信のたびに最新化する（stale-while-revalidate）。
    pub repositories_cache: RevisitCache<(String, ListSort, u32), (Vec<Repository>, PageInfo)>,
    pub selected_workspace: Option<String>,
    pub selected_repo: Option<String>,
    /// 選択リポジトリの既定ブランチ名（`mainbranch.name`）。Source ルートに使う。
    pub repo_main_branch: Option<String>,
    pub pull_requests: SelectList<PullRequest>,
    /// PullRequests 一覧の現在のサーバソート順（`S` キーで巡回）。切り替え時は 1 ページ目から
    /// 再取得する（[`App::cycle_pull_requests_sort`]）。
    pub pull_requests_sort: ListSort,
    /// PullRequests 一覧のページ状態。
    pub pull_requests_page_info: PageInfo,
    /// PR 一覧の再訪キャッシュ（キー = (repo slug, state フィルタ, ソート, ページ番号)）。
    /// `repo` は `Msg::PullRequestsLoaded` が運ぶ値（`review_context()` 由来の repo slug）に
    /// 合わせており、既存のステイル判定ガード（`repo_slug()` 一致チェック）と精度を揃えている
    /// （workspace をまたいだ同名 repo slug の衝突は既存ガードと同じ既知の制約）。
    pub pull_requests_cache: PullRequestsCache,
    pub pr_state_filter: PrStateFilter,
    /// PR フィルタモーダル（`f`）。開いている間は最優先でキー入力を奪う。
    pub pr_filter_modal: Option<PrFilterModal>,
    /// この repo の PR 著者（author 候補）のキャッシュ（キー = repo full_name）。
    /// フィルタモーダルの初回表示時に PR 一覧 API の集約（全 state・最新順・最大 3 ページ）で
    /// 遅延取得し、**成功時のみ**保存する（失敗時は読み込み済み PR の author への
    /// フォールバックを表示し、キャッシュしない＝次回開いたときに再試行する）。
    pub pr_authors_cache: RevisitCache<String, Vec<PrAuthor>>,
    /// target branch フィルタ候補（ブランチ名）のキャッシュ（キー = repo full_name）。
    /// フィルタモーダルの初回表示時にブランチ一覧の 1 ページ目を遅延取得し、**成功時のみ**
    /// 保存する（失敗時は候補なし＝自由入力の部分一致のみ。キャッシュしないため次回開いた
    /// ときに再試行する）。
    pub branch_candidates_cache: RevisitCache<String, Vec<String>>,
    /// PR フィルタモーダルの author 検索用スクラッチ（[`fuzzy_match_indices`]）。
    /// [`SelectList::matcher`] と同じく、生成コストの高い `Matcher` をキー入力のたびに
    /// 作り直さないために App が保持する（モーダル自体は純データに保つ）。
    pr_filter_matcher: Matcher,
    pub current_pr: Option<PullRequest>,
    /// PR 詳細の再訪キャッシュ（キー = (repo full_name, PR id)）。
    /// [`App::open_pr_detail_with`] が即時表示に使い、`Msg::PrDetailLoaded`/`DiffStatLoaded`/
    /// `CommentsLoaded` の受信ごとに該当フィールドだけ部分更新する。
    pub pr_detail_cache: RevisitCache<(String, u64), PrDetailCache>,
    pub diffstat: SelectList<DiffStatEntry>,
    pub comments: Vec<Comment>,
    pub detail_focus: DetailFocus,
    pub detail_scroll: u16,
    /// 直近描画時の PR 詳細本文のビューポート高さ（`detail_scroll` の上限計算に使う。
    /// `ui` が毎フレーム更新する。`DiffState::viewport` / `LogView::viewport` と同じ役割）。
    pub detail_viewport: usize,
    /// 直近描画時の概要リッチドキュメントの仮想高さ（折り返し済み Text 行 + Image 高）。
    /// `ui` が現在の pane 幅と画像状態から毎フレーム算出して書き戻す。
    /// `None` は「まだ描画していない」を表し、その間は生の行数から近似する。
    pub detail_body_rendered_lines: Option<usize>,
    /// 概要リッチドキュメント内のリンク位置（5.4 のヒットテスト用）。
    /// `ui` が現在の幅・画像高・wrap 結果に合わせて毎フレーム書き戻す。
    pub overview_link_positions: Vec<LinkPosition>,
    pub comments_scroll: u16,
    pub comments_viewport: usize,
    pub comments_rendered_lines: Option<usize>,
    pub diff: Option<DiffState>,
    pub comment_editor: Option<CommentEditor>,
    /// コメント削除の確認モーダル（`d` で開く）。
    pub delete_comment_modal: Option<DeleteCommentModal>,
    pub merge_modal: Option<MergeModal>,
    pub pipelines: SelectList<Pipeline>,
    /// Pipelines 一覧のページ状態。`[`/`]` でページ間移動、`g` でページ番号ジャンプ。
    pub pipelines_page_info: PageInfo,
    /// パイプライン一覧の再訪キャッシュ（キー = (repo slug, ページ番号)）。
    /// [`App::load_pipelines_page`] が再訪時に即表示するために使い、`Msg::PipelinesLoaded`
    /// 受信のたびに最新化する（stale-while-revalidate）。自動ポーリング（tick）による
    /// 再取得結果もここへ流れるため、進行中パイプラインがある間は最新のページ内容に保たれる。
    pub pipelines_cache: RevisitCache<(String, u32), (Vec<Pipeline>, PageInfo)>,
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
    /// Pipelines/Branches/Source を `Repositories`/`PullRequests` から開いたときの「戻り先」
    /// 画面。各画面での `p`/`P`/`b`/`s` キー押下時に記録し、Pipelines 画面・Branches 画面の
    /// `Esc`・Source 画面のルートでの `Esc`/`Backspace`（親が無い＝これ以上遡れない）がこの
    /// 戻り先を使う。Branches 経由で Source を開いた場合（Branches 画面の `s`）は更新しない
    /// （最初に入って来た画面を保つ）。
    pub browse_return: Screen,
    pub branches: SelectList<Branch>,
    /// Branches 一覧のページ状態。`[`/`]` でページ間移動、`g` でページ番号ジャンプ。
    pub branches_page_info: PageInfo,
    /// ブランチ一覧の再訪キャッシュ（キー = (repo slug, ページ番号)）。
    /// [`App::load_branches_page`] が再訪時に即表示するために使い、`Msg::BranchesLoaded`
    /// 受信のたびに最新化する（stale-while-revalidate）。
    pub branches_cache: RevisitCache<(String, u32), (Vec<Branch>, PageInfo)>,
    pub commits: SelectList<Commit>,
    /// Commits 画面が表示中の revision（ブランチ名/ハッシュ、既定は `None`）。
    pub commits_revision: Option<String>,
    /// Commits 画面の現在ページ番号（1 始まり、ページャ表示用）。
    pub commits_page: u32,
    /// 現在ページの次ページ取得用 `next` URL（無ければ次ページなし）。commits は `page` 番号
    /// ジャンプ非対応のため cursor（`next` URL）で前後する。
    pub commits_next_url: Option<String>,
    /// 前ページへ戻るための cursor スタック。各要素は「そのページを取得した cursor」
    /// （先頭ページは `None`）で、末尾が現在ページの cursor。
    pub commits_page_cursors: Vec<Option<String>>,
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
    /// Workspaces/Repositories/PullRequests でのインクリメンタル検索の編集中フラグ
    /// （`/` で開始、対象のリストは `screen` から決まる）。
    pub search_editing: bool,
    /// ジャンプパレット（`Ctrl+K`）の状態。開いている間は最優先でキー入力を奪う。
    pub jump_palette: Option<JumpPaletteState>,
    /// ページ番号ジャンプ（`g`）の入力状態。開いている間は最優先でキー入力を奪う。
    pub page_jump: Option<PageJumpModal>,
    pub link_palette: Option<LinkPalette>,
    /// 直近開いた PR（新しい順）。ジャンプパレットの候補に使う。
    pub recent_prs: Vec<RecentPr>,
    /// ImageView で表示対象の画像一覧（PR 本文から抽出。`i` キー押下時に確定する）。
    pub image_refs: Vec<ImageRef>,
    /// `image_refs` のうち現在表示中のインデックス。
    pub image_index: usize,
    /// 現在表示中の画像のデコード結果（`None` は読み込み中）。
    pub current_image: Option<Result<DynamicImage, String>>,
    /// 画像のデコード結果キャッシュ（キー = URL）。同一 URL の再取得を避ける（上限あり）。
    pub image_cache: RevisitCache<String, Result<DynamicImage, String>>,
    /// 起動時に検出したこの端末向けの `ratatui_image::picker::Picker`。
    /// `Picker::from_query_stdio` の検出結果を `main.rs` が起動時に一度だけ設定する。
    /// `None` は検出失敗＝画像表示機能を無効化する。
    pub image_picker: Option<Picker>,
    /// 現在表示中の画像の `StatefulImage` 描画状態（`ratatui_image::protocol::StatefulProtocol`）。
    /// `current_image` が `Ok` かつ `image_picker` が `Some` のときのみ
    /// `Picker::new_resize_protocol` で生成する（[`App::set_current_image`]）。実際のリサイズ・
    /// エンコードは描画時（`ui::render_image_view` の `StatefulImage`）に遅延される。
    pub image_protocol: Option<StatefulProtocol>,
    /// 概要内インライン画像ごとの描画状態（URL キー）。ImageView の状態とは分離する。
    pub overview_image_protocols: HashMap<String, StatefulProtocol>,
    /// 概要表示から非同期取得を発行済みで、まだ結果を受け取っていない画像 URL。
    overview_images_loading: HashSet<String>,
}

/// PR 詳細本文の固定ヘッダ行数（`ui::render_pr_meta_body` が積む先頭 4 行:
/// タイトル / 状態・ブランチ / author 情報 / 空行）。`detail_scroll` の上限計算に使う。
/// `ui::render_pr_meta_body` のヘッダ構成を変えたらここも合わせて更新すること。
/// （承認/変更要求パネルの行数は可変なので別途 [`participant_panel_line_count`] で加算する）。
const PR_DETAIL_HEADER_LINES: usize = 4;

/// 承認/変更要求パネルの行数（`ui::render_pr_meta_body` が積む参加者パネルと対応させる）。
/// 承認者がいれば 1 行、変更要求者がいれば 1 行、どちらも無ければ 0 行。
fn participant_panel_line_count(pr: &PullRequest) -> usize {
    let mut lines = 0;
    if !pr.approved_names().is_empty() {
        lines += 1;
    }
    if !pr.changes_requested_names().is_empty() {
        lines += 1;
    }
    lines
}

fn add_signed(value: u16, amount: i32) -> u16 {
    if amount >= 0 {
        value.saturating_add(amount.min(u16::MAX as i32) as u16)
    } else {
        value.saturating_sub(amount.unsigned_abs().min(u16::MAX as u32) as u16)
    }
}

fn rect_contains(area: Rect, point: (u16, u16)) -> bool {
    point.0 >= area.x
        && point.0 < area.x.saturating_add(area.width)
        && point.1 >= area.y
        && point.1 < area.y.saturating_add(area.height)
}

fn modal_accepts_list(modal: ModalKind, list: ListKind) -> bool {
    matches!(
        (modal, list),
        (ModalKind::LinkPalette, ListKind::LinkPalette)
            | (ModalKind::JumpPalette, ListKind::JumpPalette)
    )
}

fn batch_or_none(commands: Vec<Command>) -> Command {
    if commands.is_empty() {
        Command::None
    } else {
        Command::Batch(commands)
    }
}

/// Markdown リンクと裸 URL をコードフェンス外から抽出する。画像記法は対象外。
fn extract_links(markdown: &str, links: &mut Vec<DetailLink>) {
    let mut in_fence = false;
    for line in markdown.lines() {
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }

        let mut masked = line.to_string();
        let mut cursor = 0;
        while let Some(open_rel) = line[cursor..].find('[') {
            let open = cursor + open_rel;
            let is_image = open > 0 && line.as_bytes().get(open - 1) == Some(&b'!');
            let Some(close_rel) = line[open + 1..].find("](") else {
                cursor = open + 1;
                continue;
            };
            let close = open + 1 + close_rel;
            let url_start = close + 2;
            let Some(end_rel) = line[url_start..].find(')') else {
                cursor = url_start;
                continue;
            };
            let end = url_start + end_rel;
            let url = &line[url_start..end];
            if !is_image && is_http_url(url) {
                push_unique_link(links, &line[open + 1..close], url);
            }
            let mask_start = if is_image { open - 1 } else { open };
            // Spaces prevent the same URL from being found again as a bare URL.
            masked.replace_range(mask_start..=end, &" ".repeat(end + 1 - mask_start));
            cursor = end + 1;
        }

        for token in masked.split_whitespace() {
            let Some(start) = token.find("http://").or_else(|| token.find("https://")) else {
                continue;
            };
            let url = token[start..].trim_end_matches(|ch: char| {
                matches!(ch, '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}')
            });
            if is_http_url(url) {
                push_unique_link(links, url, url);
            }
        }
    }
}

fn is_http_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

fn push_unique_link(links: &mut Vec<DetailLink>, label: &str, url: &str) {
    if links.iter().any(|link| link.url == url) {
        return;
    }
    links.push(DetailLink {
        label: if label.is_empty() { url } else { label }.to_string(),
        url: url.to_string(),
    });
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
        let theme_name = config
            .theme
            .as_deref()
            .map(ThemeName::from_config_str)
            .unwrap_or_default();
        let theme = theme_name.theme();
        let diff_view_mode = config
            .diff_view
            .as_deref()
            .map(DiffViewMode::from_config_str)
            .unwrap_or_default();
        let diff_sidebar_visible = config.diff_sidebar_visible.unwrap_or(true);
        let diff_sidebar_width = config.diff_sidebar_width;
        let pr_state_filter = PrStateFilter::from_config(config.pr_states.as_deref());
        Self {
            screen: Screen::Onboarding,
            config,
            client,
            theme,
            theme_name,
            diff_view_mode,
            diff_sidebar_visible,
            diff_sidebar_width,
            diff_sidebar_dragging: false,
            layout: AppLayout::default(),
            me,
            onboarding,
            workspaces: SelectList::default(),
            workspaces_page_info: PageInfo::default(),
            workspaces_cache: RevisitCache::default(),
            repositories: SelectList::default(),
            repositories_sort: ListSort::default(),
            repositories_page_info: PageInfo::default(),
            repositories_cache: RevisitCache::default(),
            selected_workspace: None,
            selected_repo: None,
            repo_main_branch: None,
            pull_requests: SelectList::default(),
            pull_requests_sort: ListSort::default(),
            pull_requests_page_info: PageInfo::default(),
            pull_requests_cache: RevisitCache::default(),
            pr_state_filter,
            pr_filter_modal: None,
            pr_authors_cache: RevisitCache::default(),
            branch_candidates_cache: RevisitCache::default(),
            pr_filter_matcher: Matcher::default(),
            current_pr: None,
            pr_detail_cache: RevisitCache::default(),
            diffstat: SelectList::default(),
            comments: Vec::new(),
            detail_focus: DetailFocus::default(),
            detail_scroll: 0,
            detail_viewport: 0,
            detail_body_rendered_lines: None,
            overview_link_positions: Vec::new(),
            comments_scroll: 0,
            comments_viewport: 0,
            comments_rendered_lines: None,
            diff: None,
            comment_editor: None,
            delete_comment_modal: None,
            merge_modal: None,
            pipelines: SelectList::default(),
            pipelines_page_info: PageInfo::default(),
            pipelines_cache: RevisitCache::default(),
            current_pipeline: None,
            pipeline_steps: SelectList::default(),
            step_log: None,
            open_step_uuid: None,
            confirm_modal: None,
            auto_refresh: true,
            diff_return: Screen::PullRequestDetail,
            browse_return: Screen::Repositories,
            branches: SelectList::default(),
            branches_page_info: PageInfo::default(),
            branches_cache: RevisitCache::default(),
            commits: SelectList::default(),
            commits_revision: None,
            commits_page: 1,
            commits_next_url: None,
            commits_page_cursors: vec![None],
            current_commit: None,
            commit_scroll: 0,
            source: None,
            file_view: None,
            open_file_path: None,
            open_file_mimetype: None,
            status: Status::Idle,
            show_help: false,
            search_editing: false,
            jump_palette: None,
            page_jump: None,
            link_palette: None,
            recent_prs: Vec::new(),
            image_refs: Vec::new(),
            image_index: 0,
            current_image: None,
            image_cache: RevisitCache::default(),
            image_picker: None,
            image_protocol: None,
            overview_image_protocols: HashMap::new(),
            overview_images_loading: HashSet::new(),
        }
    }

    /// 起動直後に実行すべきコマンドを返し、初期画面を確定する。
    ///
    /// 認証済みなら Workspaces へ進み 1 ページ目の取得を開始、未認証なら Onboarding に留まる。
    pub fn init_command(&mut self) -> Command {
        if self.client.is_some() {
            self.screen = Screen::Workspaces;
            return self.load_workspaces_page(1);
        }
        self.screen = Screen::Onboarding;
        Command::None
    }

    /// メッセージを適用し、必要な副作用を返す。
    pub fn update(&mut self, msg: Msg) -> Command {
        match msg {
            Msg::Key(key) => self.on_key(key),
            Msg::Mouse(mouse) => self.on_mouse(mouse),
            Msg::AuthValidated { email, token, user } => self.on_auth_validated(email, token, user),
            Msg::AuthFailed(error) => {
                self.onboarding.validating = false;
                self.onboarding.error = Some(error.to_string());
                Command::None
            }
            Msg::WorkspacesLoaded {
                workspaces,
                page_info,
            } => {
                // ページ単位でキャッシュを最新化する（他ページへ移動していても次回再訪時に活かす）。
                self.workspaces_cache
                    .insert(page_info.page, (workspaces.clone(), page_info));
                // 取得中に別ページへ移動していた場合（要求ページと不一致）は画面反映のみ破棄。
                if self.workspaces_page_info.page == page_info.page {
                    self.status = Status::Idle;
                    // 同一文脈（同じページ）への再検証: 選択位置は識別子（slug）で追従し、
                    // 見つからなければインデックスをクランプする（先頭へは戻さない）。
                    self.workspaces
                        .set_items_keep_selection_by(workspaces, |workspace| {
                            workspace.slug.clone()
                        });
                    self.workspaces_page_info = page_info;
                }
                Command::None
            }
            Msg::RepositoriesLoaded {
                workspace,
                sort,
                repos,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（裏で他ワークスペース/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.repositories_cache.insert(
                    (workspace.clone(), sort, page_info.page),
                    (repos.clone(), page_info),
                );
                // 取得中に別ワークスペース/ページ/ソートへ切り替えていた場合は画面反映のみ破棄。
                if self.selected_workspace.as_deref() == Some(workspace.as_str())
                    && self.repositories_sort == sort
                    && self.repositories_page_info.page == page_info.page
                {
                    self.status = Status::Idle;
                    // 同一文脈への再検証: 選択位置は識別子（full_name）で追従する。
                    self.repositories
                        .set_items_keep_selection_by(repos, |repo| repo.full_name.clone());
                    self.repositories_page_info = page_info;
                }
                Command::None
            }
            Msg::LoadFailed(error) => {
                self.status = Status::Error(error.to_string());
                Command::None
            }
            Msg::PullRequestsLoaded {
                repo,
                filter,
                sort,
                prs,
                page_info,
            } => {
                // 取得中に別 repo/フィルタ/ソート/ページへ切り替えていた場合は画面反映のみ
                // 破棄する文脈ガード（キャッシュ挿入で filter を move する前に判定する）。
                let fresh = self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.pr_state_filter == filter
                    && self.pull_requests_sort == sort
                    && self.pull_requests_page_info.page == page_info.page;
                // 表示中かどうかに関わらずキャッシュは最新化する（他 repo/フィルタ/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.pull_requests_cache.insert(
                    (repo, filter, sort, page_info.page),
                    (prs.clone(), page_info),
                );
                if fresh {
                    self.status = Status::Idle;
                    // 同一文脈への再検証: 選択位置は識別子（PR id）で追従する。
                    self.pull_requests
                        .set_items_keep_selection_by(prs, |pr| pr.id);
                    self.pull_requests_page_info = page_info;
                }
                Command::None
            }
            Msg::PrAuthorsLoaded {
                repo_full_name,
                result,
            } => {
                match result {
                    Ok(prs) => {
                        let authors =
                            users_to_authors(prs.into_iter().filter_map(|pr| pr.author).collect());
                        // 表示中かどうかに関わらずキャッシュは最新化する（次回モーダルを
                        // 開いたときの即時表示に活かす）。
                        self.pr_authors_cache
                            .insert(repo_full_name.clone(), authors.clone());
                        self.fill_pr_filter_modal_authors(&repo_full_name, authors);
                    }
                    Err(error) => {
                        // 取得できない場合は読み込み済み PR の author へフォールバックする
                        // （キャッシュには入れない＝次回開いたときに再試行する）。
                        tracing::warn!(%error, "author 候補（PR 集約）の取得に失敗しました");
                        let fallback = self.loaded_pr_authors();
                        self.fill_pr_filter_modal_authors(&repo_full_name, fallback);
                    }
                }
                Command::None
            }
            Msg::FilterBranchesLoaded {
                repo_full_name,
                result,
            } => {
                match result {
                    Ok(branches) => {
                        let names: Vec<String> = branches
                            .into_iter()
                            .filter_map(|branch| branch.name)
                            .collect();
                        // 表示中かどうかに関わらずキャッシュは最新化する（成功時のみ）。
                        self.branch_candidates_cache
                            .insert(repo_full_name.clone(), names.clone());
                        self.fill_pr_filter_modal_branches(&repo_full_name, names);
                    }
                    Err(error) => {
                        // 候補が無くても自由入力の部分一致は使えるため、空候補で確定させる
                        // （キャッシュには入れない＝次回開いたときに再試行する）。
                        tracing::warn!(%error, "target branch 候補の取得に失敗しました");
                        self.fill_pr_filter_modal_branches(&repo_full_name, Vec::new());
                    }
                }
                Command::None
            }
            Msg::PrDetailLoaded { id, pr } => {
                if self.current_pr_id() == Some(id) {
                    self.clear_loading();
                    let pr = *pr;
                    self.current_pr = Some(pr.clone());
                    self.update_pr_detail_cache(id, move |entry| entry.pr = pr);
                    if let Some(client) = self.client.clone() {
                        return batch_or_none(self.queue_overview_images(&client));
                    }
                }
                Command::None
            }
            Msg::DiffStatLoaded { id, entries } => {
                if self.current_pr_id() == Some(id) {
                    self.diffstat.set_items(entries.clone());
                    self.update_pr_detail_cache(id, move |entry| entry.diffstat = entries);
                }
                Command::None
            }
            Msg::CommentsLoaded { id, comments } => {
                if self.current_pr_id() == Some(id) {
                    let comments: Vec<Comment> = comments
                        .into_iter()
                        .filter(|comment| !comment.deleted)
                        .collect();
                    self.comments = comments.clone();
                    self.update_pr_detail_cache(id, move |entry| entry.comments = comments);
                    // Diff を開いている場合はインライン表示のスレッド配置を作り直す。
                    self.rebuild_diff_derived();
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
                        rendered_lines: None,
                        rendered_split: None,
                        file_index: 0,
                        cursor: 0,
                        focus: DiffFocus::Body,
                        view_mode: self.diff_view_mode,
                        comment_layout: CommentLayout::default(),
                        sidebar_rows: Vec::new(),
                        display_rows: Vec::new(),
                        thread_collapse: HashMap::new(),
                    });
                    self.rebuild_diff_derived();
                }
                Command::None
            }
            Msg::ReviewActionDone { id, message } => {
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(message);
                    // 承認/変更要求の状態は PR 一覧の ✔n/m バッジにも出るため、一覧キャッシュは
                    // 無効化する（詳細側は refresh_detail() の再取得結果が update_pr_detail_cache
                    // 経由で自動的に最新化される）。
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_detail();
                }
                Command::None
            }
            Msg::CommentPosted { id } => {
                self.comment_editor = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success("コメントを投稿しました".to_string());
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_comments();
                }
                Command::None
            }
            Msg::CommentActionDone { id, message } => {
                self.comment_editor = None;
                self.delete_comment_modal = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(message);
                    self.invalidate_pull_requests_cache_for_current_repo();
                    return self.refresh_comments();
                }
                Command::None
            }
            Msg::MergeDone { id } => {
                self.merge_modal = None;
                if self.current_pr_id() == Some(id) {
                    self.status = Status::Success(format!("PR #{id} をマージしました"));
                    // マージで PR の state が変わり一覧の掲載フィルタ（OPEN/MERGED/…）を
                    // またぐため、一覧キャッシュは無効化して次回表示を必ず再取得させる。
                    self.invalidate_pull_requests_cache_for_current_repo();
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
            Msg::PipelinesLoaded {
                repo,
                pipelines,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（裏で他 repo/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.pipelines_cache.insert(
                    (repo.clone(), page_info.page),
                    (pipelines.clone(), page_info),
                );
                // 取得中に別 repo/ページへ切り替えていた場合（自動ポーリングの tick が古い
                // ページの結果を運んできた場合を含む）は画面反映のみ破棄する文脈ガード。
                if self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.pipelines_page_info.page == page_info.page
                {
                    self.clear_loading();
                    // 自動ポーリング/再検証で一覧が毎回先頭に戻らないよう選択位置を維持する。
                    self.pipelines.set_items_keep_selection(pipelines);
                    self.pipelines_page_info = page_info;
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
                        // 新しい実行は 1 ページ目の先頭に現れるため、一覧の 1 ページ目へ戻し
                        // 静かに再取得する（他ページ表示中に rerun した場合も先頭が見える位置
                        // へ揃える）。
                        self.screen = Screen::Pipelines;
                        self.pipelines_page_info.page = 1;
                        self.refresh_pipelines_silent()
                    }
                }
            }
            Msg::BranchesLoaded {
                repo,
                branches,
                page_info,
            } => {
                // 表示中かどうかに関わらずキャッシュは最新化する（裏で他 repo/ページへ
                // 切り替えていても、その結果は次回再訪時に活かす）。
                self.branches_cache.insert(
                    (repo.clone(), page_info.page),
                    (branches.clone(), page_info),
                );
                // 取得中に別 repo/ページへ切り替えていた場合は画面反映のみ破棄。
                if self.repo_slug().as_deref() == Some(repo.as_str())
                    && self.branches_page_info.page == page_info.page
                {
                    self.clear_loading();
                    // 同一文脈への再検証: 選択位置は識別子（ブランチ名）で追従する。
                    self.branches
                        .set_items_keep_selection_by(branches, |branch| branch.name.clone());
                    self.branches_page_info = page_info;
                }
                Command::None
            }
            Msg::CommitsLoaded {
                revision,
                commits,
                next,
                page,
            } => {
                // 取得中に別 revision/ページへ切り替えていた場合は画面反映を破棄する（文脈ガード）。
                if self.commits_revision == revision && self.commits_page == page {
                    self.clear_loading();
                    self.commits.set_items(commits);
                    self.commits_next_url = next;
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
                        rendered_lines: None,
                        rendered_split: None,
                        file_index: 0,
                        cursor: 0,
                        focus: DiffFocus::Body,
                        view_mode: self.diff_view_mode,
                        comment_layout: CommentLayout::default(),
                        sidebar_rows: Vec::new(),
                        display_rows: Vec::new(),
                        thread_collapse: HashMap::new(),
                    });
                    self.rebuild_diff_derived();
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
            Msg::ImageLoaded { url, result } => {
                let decoded = result.and_then(|bytes| imageview::decode_image(&bytes));
                self.overview_images_loading.remove(&url);
                // 表示中かどうかに関わらずキャッシュは常に最新化する（他画像/他 PR へ切り替えて
                // いても、その結果は次回再訪時に活かす）。
                self.image_cache.insert(url.clone(), decoded.clone());
                match (&decoded, self.image_picker.as_ref()) {
                    (Ok(image), Some(picker)) => {
                        self.overview_image_protocols
                            .insert(url.clone(), picker.new_resize_protocol(image.clone()));
                    }
                    _ => {
                        self.overview_image_protocols.remove(&url);
                    }
                }
                // 取得中に別の画像へ切り替えていた場合（古い URL の結果）は画面反映のみ破棄する。
                if self.screen == Screen::ImageView
                    && self
                        .image_refs
                        .get(self.image_index)
                        .is_some_and(|current| current.url == url)
                {
                    self.set_current_image(decoded);
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
        self.client = Some(client);
        self.screen = Screen::Workspaces;
        self.load_workspaces_page(1)
    }

    /// キー入力の処理。グローバルキー（Ctrl+C / Ctrl+T / ジャンプパレット / ヘルプ / モーダル）
    /// を先に捌く。
    fn on_key(&mut self, key: KeyEvent) -> Command {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::Quit;
        }
        if key.code == KeyCode::Char('t') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.cycle_theme();
        }

        // ジャンプパレット（`Ctrl+K`）: 開いている間は最優先で入力を奪う。
        if self.jump_palette.is_some() {
            return self.on_key_jump_palette(key);
        }
        if self.link_palette.is_some() {
            return self.on_key_link_palette(key);
        }
        // 開くトリガーは show_help と同格（他のモーダル/検索編集中/Onboarding では無効）。
        // Onboarding だけは対象外: `Ctrl+K` は emacs 風の「行末まで削除」で既に使用中で、
        // 認証前は保持済みデータも無くジャンプ先が無いため衝突を避ける。
        if key.code == KeyCode::Char('k')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.screen != Screen::Onboarding
            && !self.show_help
            && !self.search_editing
            && self.comment_editor.is_none()
            && self.merge_modal.is_none()
            && self.confirm_modal.is_none()
            && self.pr_filter_modal.is_none()
            && self.delete_comment_modal.is_none()
            && self.page_jump.is_none()
        {
            return self.open_jump_palette();
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
        if self.delete_comment_modal.is_some() {
            return self.on_key_delete_comment_modal(key);
        }
        if self.merge_modal.is_some() {
            return self.on_key_merge_modal(key);
        }
        if self.confirm_modal.is_some() {
            return self.on_key_confirm_modal(key);
        }
        if self.pr_filter_modal.is_some() {
            return self.on_key_pr_filter_modal(key);
        }
        if self.page_jump.is_some() {
            return self.on_key_page_jump(key);
        }
        if self.search_editing {
            return self.on_key_search_editing(key);
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
            Screen::ImageView => self.on_key_image_view(key),
        }
    }

    /// 直近フレームの [`AppLayout`] を使ってマウス入力を既存のキー操作へ変換する。
    ///
    /// `Drag`/`Up` は Diff サイドバーの幅調整専用（[`App::on_mouse_drag`]/[`App::on_mouse_up`]）。
    /// それ以外のドラッグ中の移動・離指は無視する（`event::run` 側もこの 2 種類の
    /// `MouseEventKind` だけを追加で `Msg::Mouse` へ変換する）。
    fn on_mouse(&mut self, mouse: MouseEvent) -> Command {
        let point = (mouse.column, mouse.row);
        match mouse.kind {
            MouseEventKind::ScrollUp => self.on_mouse_wheel(point, false),
            MouseEventKind::ScrollDown => self.on_mouse_wheel(point, true),
            MouseEventKind::Down(MouseButton::Left) => self.on_mouse_left(point),
            MouseEventKind::Drag(MouseButton::Left) => self.on_mouse_drag(point),
            MouseEventKind::Up(MouseButton::Left) => self.on_mouse_up(point),
            _ => Command::None,
        }
    }

    fn on_mouse_wheel(&mut self, point: (u16, u16), downward: bool) -> Command {
        if let Some(modal) = self.layout.modal.clone() {
            if !rect_contains(modal.area, point) {
                return Command::None;
            }
            if let Some(list) = self
                .layout
                .lists
                .iter()
                .find(|list| {
                    modal_accepts_list(modal.kind, list.kind) && rect_contains(list.area, point)
                })
                .cloned()
            {
                self.move_list(list.kind, downward, 3);
            }
            return Command::None;
        }

        if let Some(list) = self
            .layout
            .lists
            .iter()
            .find(|list| rect_contains(list.area, point))
            .cloned()
        {
            self.move_list(list.kind, downward, 3);
            return Command::None;
        }
        let pane = self
            .layout
            .panes
            .iter()
            .find(|(_, area)| rect_contains(*area, point))
            .map(|(kind, _)| *kind);
        match pane {
            Some(PaneKind::Overview) => {
                self.detail_scroll = add_signed(self.detail_scroll, if downward { 3 } else { -3 });
                self.detail_scroll = self.detail_scroll.min(self.detail_max_scroll());
            }
            Some(PaneKind::Comments) => {
                self.comments_scroll =
                    add_signed(self.comments_scroll, if downward { 3 } else { -3 });
                self.clamp_comments_scroll();
            }
            Some(PaneKind::DiffBody) => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.focus = DiffFocus::Body;
                    diff.move_cursor(if downward { 3 } else { -3 });
                }
            }
            Some(PaneKind::StepLog) => {
                if let Some(log) = self.step_log.as_mut() {
                    if downward {
                        log.scroll_down(3);
                    } else {
                        log.scroll_up(3);
                    }
                }
            }
            Some(PaneKind::FileView) => {
                if let Some(view) = self.file_view.as_mut() {
                    if downward {
                        view.scroll_down(3);
                    } else {
                        view.scroll_up(3);
                    }
                }
            }
            _ => {}
        }
        Command::None
    }

    fn on_mouse_left(&mut self, point: (u16, u16)) -> Command {
        if let Some(modal) = self.layout.modal.clone() {
            if !rect_contains(modal.area, point) {
                return self.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            // 破壊的操作はモーダル内のどこをクリックしても決定しない。
            if matches!(
                modal.kind,
                ModalKind::MergeConfirm
                    | ModalKind::PipelineConfirm
                    | ModalKind::DeleteCommentConfirm
            ) {
                return Command::None;
            }
            if let Some(list) = self
                .layout
                .lists
                .iter()
                .find(|list| {
                    modal_accepts_list(modal.kind, list.kind) && rect_contains(list.area, point)
                })
                .cloned()
            {
                return self.click_list_row(&list, point.1);
            }
            return Command::None;
        }

        // Diff サイドバーと本文の境界列（±1 セル）は他のクリック処理より優先してドラッグ開始
        // とみなす（一覧行選択・ペインフォーカス切替等を発火させない）。M5 のペイン/一覧
        // ヒットテストと同じ `self.layout` 経由の座標判定に乗せる。
        if self.diff_sidebar_visible && self.diff_sidebar_boundary_hit(point) {
            self.diff_sidebar_dragging = true;
            return Command::None;
        }

        if let Some(image) = self
            .layout
            .overview_images
            .iter()
            .find(|image| rect_contains(image.area, point))
            .cloned()
        {
            self.detail_focus = DetailFocus::Overview;
            return self.open_clicked_image(&image.url);
        }

        if let Some(content) = self.layout.overview_content
            && rect_contains(content, point)
        {
            self.detail_focus = DetailFocus::Overview;
            if let Some(urls) = self.overview_urls_at(point) {
                if urls.len() == 1 {
                    return self.open_url_in_browser(&urls[0]);
                }
                let mut palette = LinkPalette::default();
                palette.links.set_items(
                    urls.into_iter()
                        .map(|url| DetailLink {
                            label: url.clone(),
                            url,
                        })
                        .collect(),
                );
                self.link_palette = Some(palette);
                return Command::None;
            }
        }

        // Diff 本文内のコメントアクションリンク（Reply 等）はペイン処理より先に判定する。
        if let Some(hit) = self
            .layout
            .comment_actions
            .iter()
            .find(|hit| rect_contains(hit.area, point))
            .cloned()
        {
            if let Some(diff) = self.diff.as_mut() {
                diff.focus = DiffFocus::Body;
                // クリックしたコメントへカーソルも移す（対象の視覚フィードバック）。
                diff.reanchor_cursor(Some((hit.thread_root, hit.comment_id)), None);
                diff.ensure_cursor_visible();
            }
            return match hit.action {
                CommentAction::Reply => self.reply_to_comment(hit.comment_id),
                CommentAction::Resolve => self.resolve_thread(hit.thread_root),
                CommentAction::Edit => self.edit_comment_by_id(hit.comment_id),
                CommentAction::Delete => self.request_delete_by_id(hit.comment_id),
            };
        }

        if let Some(list) = self
            .layout
            .lists
            .iter()
            .find(|list| rect_contains(list.area, point))
            .cloned()
        {
            return self.click_list_row(&list, point.1);
        }

        let pane = self
            .layout
            .panes
            .iter()
            .find(|(_, area)| rect_contains(*area, point))
            .cloned();
        match pane {
            Some((PaneKind::Overview, _)) => self.detail_focus = DetailFocus::Overview,
            Some((PaneKind::ChangedFiles, _)) => self.detail_focus = DetailFocus::Files,
            Some((PaneKind::Comments, _)) => self.detail_focus = DetailFocus::Comments,
            Some((PaneKind::DiffFiles, _)) => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.focus = DiffFocus::Files;
                }
            }
            Some((PaneKind::DiffBody, area)) => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.focus = DiffFocus::Body;
                    // クリック行へカーソル移動（コラプス行は展開、枠行はコメント選択へ）。
                    diff.click_body_row(area, point);
                }
            }
            _ => {}
        }

        if let Some(hint) = self
            .layout
            .hints
            .iter()
            .find(|hint| rect_contains(hint.area, point))
            .cloned()
        {
            return self.on_key(hint.key);
        }
        Command::None
    }

    /// マウス座標が Diff サイドバーと本文の境界列（±1 セル）に乗っているかを判定する
    /// （[`App::on_mouse_left`] からのドラッグ開始判定に使う）。両ペインが `self.layout.panes`
    /// に無ければ（Diff 画面以外、またはサイドバー非表示）常に `false`。
    fn diff_sidebar_boundary_hit(&self, point: (u16, u16)) -> bool {
        let Some(&(_, files_rect)) = self
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffFiles)
        else {
            return false;
        };
        let Some(&(_, body_rect)) = self
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffBody)
        else {
            return false;
        };
        if point.1 < files_rect.y || point.1 >= files_rect.y.saturating_add(files_rect.height) {
            return false;
        }
        point.0.abs_diff(body_rect.x) <= 1
    }

    /// マウスドラッグ（`Down` 済みの境界列を追従）。Diff サイドバーの幅調整専用
    /// （[`App::on_mouse`]）。
    fn on_mouse_drag(&mut self, point: (u16, u16)) -> Command {
        if !self.diff_sidebar_dragging {
            return Command::None;
        }
        self.update_diff_sidebar_drag(point);
        Command::None
    }

    /// ドラッグ中のサイドバー幅を `point` の列位置から再計算する。サイドバー左端
    /// （`DiffFiles` ペインの `x`）からの相対列数をそのまま新しい幅とする。
    /// `DIFF_SIDEBAR_MIN_WIDTH` 未満まで縮めた場合は幅を更新せず非表示へ切り替える
    /// （直前の幅は次回表示（`t`）のために保つ）。
    fn update_diff_sidebar_drag(&mut self, point: (u16, u16)) {
        let Some(&(_, files_rect)) = self
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffFiles)
        else {
            return;
        };
        let Some(&(_, body_rect)) = self
            .layout
            .panes
            .iter()
            .find(|(kind, _)| *kind == PaneKind::DiffBody)
        else {
            return;
        };
        let total = files_rect.width.saturating_add(body_rect.width);
        let raw = point.0.saturating_sub(files_rect.x);
        if raw < DIFF_SIDEBAR_MIN_WIDTH {
            self.diff_sidebar_visible = false;
            if let Some(diff) = self.diff.as_mut() {
                diff.focus = DiffFocus::Body;
            }
            return;
        }
        self.diff_sidebar_width = Some(resolve_diff_sidebar_width(total, Some(raw)));
    }

    /// マウスボタンを離した（`Up`）。Diff サイドバーのドラッグ中であれば幅・表示状態を
    /// `config.toml` へ確定保存する（[`App::persist_diff_sidebar`]）。ドラッグ中でなければ
    /// 何もしない（他画面・他操作の `Up` は無視する）。
    fn on_mouse_up(&mut self, _point: (u16, u16)) -> Command {
        if !self.diff_sidebar_dragging {
            return Command::None;
        }
        self.diff_sidebar_dragging = false;
        self.persist_diff_sidebar();
        Command::None
    }

    fn overview_urls_at(&self, point: (u16, u16)) -> Option<Vec<String>> {
        let content = self.layout.overview_content?;
        if !rect_contains(content, point) {
            return None;
        }
        let visual_line = usize::from(self.detail_scroll)
            .saturating_add(usize::from(point.1.saturating_sub(content.y)));
        let column = point.0.saturating_sub(content.x);
        self.overview_link_positions
            .iter()
            .find(|position| {
                position.visual_line == visual_line && position.column_range.contains(&column)
            })
            .map(|position| position.urls.clone())
    }

    fn move_list(&mut self, kind: ListKind, downward: bool, amount: usize) {
        macro_rules! move_selection {
            ($list:expr) => {
                if downward {
                    $list.select_next_by(amount);
                } else {
                    $list.select_prev_by(amount);
                }
            };
        }
        match kind {
            ListKind::Workspaces => move_selection!(self.workspaces),
            ListKind::Repositories => move_selection!(self.repositories),
            ListKind::PullRequests => move_selection!(self.pull_requests),
            ListKind::ChangedFiles => move_selection!(self.diffstat),
            ListKind::Pipelines => move_selection!(self.pipelines),
            ListKind::PipelineSteps => move_selection!(self.pipeline_steps),
            ListKind::Branches => move_selection!(self.branches),
            ListKind::Commits => move_selection!(self.commits),
            ListKind::Source => {
                if let Some(source) = self.source.as_mut() {
                    move_selection!(source.entries);
                }
            }
            ListKind::DiffFiles => {
                if let Some(diff) = self.diff.as_mut() {
                    for _ in 0..amount {
                        if downward {
                            diff.select_file_next();
                        } else {
                            diff.select_file_prev();
                        }
                    }
                }
            }
            ListKind::LinkPalette => {
                if let Some(palette) = self.link_palette.as_mut() {
                    move_selection!(palette.links);
                }
            }
            ListKind::JumpPalette => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    move_selection!(palette.entries);
                }
            }
        }
    }

    fn click_list_row(&mut self, layout: &ListLayout, row: u16) -> Command {
        match layout.kind {
            ListKind::ChangedFiles => self.detail_focus = DetailFocus::Files,
            ListKind::DiffFiles => {
                if let Some(diff) = self.diff.as_mut() {
                    diff.focus = DiffFocus::Files;
                }
            }
            _ => {}
        }
        let position = layout
            .first_visible
            .saturating_add(usize::from(row.saturating_sub(layout.area.y)));
        if position >= self.list_len(layout.kind) {
            return Command::None;
        }
        let selected = self.list_selection(layout.kind);
        self.select_list_position(layout.kind, position);
        if selected == Some(position) {
            self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        } else {
            Command::None
        }
    }

    fn list_selection(&self, kind: ListKind) -> Option<usize> {
        match kind {
            ListKind::Workspaces => self.workspaces.state.selected(),
            ListKind::Repositories => self.repositories.state.selected(),
            ListKind::PullRequests => self.pull_requests.state.selected(),
            ListKind::ChangedFiles => self.diffstat.state.selected(),
            ListKind::Pipelines => self.pipelines.state.selected(),
            ListKind::PipelineSteps => self.pipeline_steps.state.selected(),
            ListKind::Branches => self.branches.state.selected(),
            ListKind::Commits => self.commits.state.selected(),
            ListKind::Source => self
                .source
                .as_ref()
                .and_then(|source| source.entries.state.selected()),
            ListKind::DiffFiles => self.diff.as_ref().map(|diff| diff.selected_sidebar_row()),
            ListKind::LinkPalette => self
                .link_palette
                .as_ref()
                .and_then(|palette| palette.links.state.selected()),
            ListKind::JumpPalette => self
                .jump_palette
                .as_ref()
                .and_then(|palette| palette.entries.state.selected()),
        }
    }

    fn list_len(&self, kind: ListKind) -> usize {
        match kind {
            ListKind::Workspaces => self.workspaces.matches.len(),
            ListKind::Repositories => self.repositories.matches.len(),
            ListKind::PullRequests => self.pull_requests.matches.len(),
            ListKind::ChangedFiles => self.diffstat.matches.len(),
            ListKind::Pipelines => self.pipelines.matches.len(),
            ListKind::PipelineSteps => self.pipeline_steps.matches.len(),
            ListKind::Branches => self.branches.matches.len(),
            ListKind::Commits => self.commits.matches.len(),
            ListKind::Source => self
                .source
                .as_ref()
                .map_or(0, |source| source.entries.matches.len()),
            ListKind::DiffFiles => self.diff.as_ref().map_or(0, |diff| {
                if diff.sidebar_rows.is_empty() {
                    diff.parsed.files.len()
                } else {
                    diff.sidebar_rows.len()
                }
            }),
            ListKind::LinkPalette => self
                .link_palette
                .as_ref()
                .map_or(0, |palette| palette.links.matches.len()),
            ListKind::JumpPalette => self
                .jump_palette
                .as_ref()
                .map_or(0, |palette| palette.entries.matches.len()),
        }
    }

    fn select_list_position(&mut self, kind: ListKind, position: usize) {
        match kind {
            ListKind::Workspaces => self.workspaces.select_position(position),
            ListKind::Repositories => self.repositories.select_position(position),
            ListKind::PullRequests => self.pull_requests.select_position(position),
            ListKind::ChangedFiles => self.diffstat.select_position(position),
            ListKind::Pipelines => self.pipelines.select_position(position),
            ListKind::PipelineSteps => self.pipeline_steps.select_position(position),
            ListKind::Branches => self.branches.select_position(position),
            ListKind::Commits => self.commits.select_position(position),
            ListKind::Source => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_position(position);
                }
            }
            ListKind::DiffFiles => {
                if let Some(diff) = self.diff.as_mut() {
                    // `position` は表示行インデックス。ツリー表示ではフォルダ行に当たると
                    // ファイル添字へ写せないので何もしない。空（未構築）ならフラット添字。
                    if diff.sidebar_rows.is_empty() {
                        if position < diff.parsed.files.len() {
                            diff.select_file(position);
                        }
                    } else if let Some(file_index) = diff
                        .sidebar_rows
                        .get(position)
                        .and_then(SidebarRow::file_index)
                    {
                        diff.select_file(file_index);
                    }
                }
            }
            ListKind::LinkPalette => {
                if let Some(palette) = self.link_palette.as_mut() {
                    palette.links.select_position(position);
                }
            }
            ListKind::JumpPalette => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    palette.entries.select_position(position);
                }
            }
        }
    }

    fn open_clicked_image(&mut self, url: &str) -> Command {
        let refs = self
            .current_pr
            .as_ref()
            .and_then(PullRequest::body)
            .map(imageview::extract_image_refs)
            .unwrap_or_default();
        let Some(index) = refs.iter().position(|image| image.url == url) else {
            return Command::None;
        };
        self.image_refs = refs;
        self.image_index = index;
        self.current_image = None;
        self.image_protocol = None;
        self.screen = Screen::ImageView;
        self.load_current_image()
    }

    /// テーマを次へ巡回する（`Ctrl+T`）。`config.toml` へ永続化し、Diff の着色済み行
    /// キャッシュ（[`DiffState::rendered_lines`]/[`DiffState::rendered_split`]）を無効化して
    /// 次回描画で新テーマ色を再構築させる（無効化しないと旧テーマの色のまま表示され続ける）。
    fn cycle_theme(&mut self) -> Command {
        self.theme_name = self.theme_name.next();
        self.theme = self.theme_name.theme();

        if let Some(diff) = self.diff.as_mut() {
            diff.rendered_lines = None;
            diff.rendered_split = None;
        }

        self.config.theme = Some(self.theme_name.as_str().to_string());
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない（他の config 保存箇所と同じ方針）。
            tracing::warn!(%error, "テーマ設定の保存に失敗しました");
        }
        Command::None
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
            KeyCode::Char('/') => {
                self.search_editing = true;
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
            KeyCode::Char('J') => {
                self.workspaces.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.workspaces.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Enter => self.enter_workspace(),
            _ => Command::None,
        }
    }

    /// ワークスペース決定時: 選択中のワークスペースへ入る。
    fn enter_workspace(&mut self) -> Command {
        let Some(workspace) = self.workspaces.selected() else {
            return Command::None;
        };
        let slug = workspace.slug.clone();
        self.jump_to_workspace(slug)
    }

    /// 指定ワークスペースへ入る（既定ワークスペースの保存・リポジトリ 1 ページ目の取得開始
    /// まで行う）。一覧での選択決定（[`App::enter_workspace`]）とジャンプパレットの双方から
    /// 呼ぶ共通経路。
    fn jump_to_workspace(&mut self, slug: String) -> Command {
        self.selected_workspace = Some(slug.clone());
        self.config.default_workspace = Some(slug);
        if let Err(error) = self.config.save() {
            tracing::warn!(%error, "既定ワークスペースの保存に失敗しました");
        }

        self.screen = Screen::Repositories;
        self.load_repositories_page(1)
    }

    /// リポジトリ一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::repositories_cache`]、キー = (workspace slug, ソート, ページ番号)）が
    /// あれば即座に一覧を表示しつつ、裏で `Command::LoadRepositories` を発行して最新化する
    /// （stale-while-revalidate）。キャッシュが無ければ一覧をクリアして Loading 表示を出す。
    /// [`App::jump_to_workspace`]（新規入場は 1 ページ目から）・`[`/`]`/`g`（ページ移動）・
    /// `S`（ソート変更、1 ページ目から）の共通経路。
    fn load_repositories_page(&mut self, page: u32) -> Command {
        let Some(workspace) = self.selected_workspace.clone() else {
            self.status = Status::Error("ワークスペースが未選択です".to_string());
            return Command::None;
        };
        let sort = self.repositories_sort;

        let cache_key = (workspace.clone(), sort, page);
        match self.repositories_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_repositories(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.repositories.set_items(Vec::new());
                self.repositories_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!(
                    "{workspace} のリポジトリを取得中…（{} ・{page} ページ目）",
                    sort.label()
                ));
            }
        }

        match &self.client {
            Some(client) => Command::LoadRepositories {
                client: client.clone(),
                workspace,
                sort,
                page,
            },
            None => {
                self.status = Status::Error("認証クライアントが未初期化です".to_string());
                Command::None
            }
        }
    }

    /// 取得結果を `repositories` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （ワークスペース変更・ページ変更・ソート変更）はここで選択を先頭にリセットする
    /// （キャッシュヒットによる即時表示（[`App::load_repositories_page`]）専用。バックグラウンド
    /// 再検証結果は `Msg::RepositoriesLoaded` 側で選択を保持したまま部分更新する）。
    ///
    /// 検索フィルタは（`SelectList::set_items` により）ここで必ずクリアされる。ページが変われば
    /// 表示される 40 件が入れ替わるため、前ページのフィルタを引き継がない設計判断。
    fn apply_repositories(&mut self, repos: Vec<Repository>, page_info: PageInfo) {
        self.repositories.set_items(repos);
        self.repositories_page_info = page_info;
    }

    fn on_key_repositories(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Char('/') => {
                self.search_editing = true;
                Command::None
            }
            KeyCode::Char('S') => self.cycle_repositories_sort(),
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
            KeyCode::Char('J') => {
                self.repositories.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.repositories.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Enter => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.enter_repository(repo)
            }
            KeyCode::Char('p') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.browse_return = Screen::Repositories;
                self.open_pipelines()
            }
            KeyCode::Char('b') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.browse_return = Screen::Repositories;
                self.open_branches()
            }
            KeyCode::Char('s') => {
                let Some(repo) = self.repositories.selected().cloned() else {
                    return Command::None;
                };
                self.select_repo(&repo);
                self.browse_return = Screen::Repositories;
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            _ => Command::None,
        }
    }

    /// 選択リポジトリを確定する（`ws/repo` と既定ブランチを保持する）。repo コンテキストが
    /// 実際に変わる場合は author / target branch フィルタをリセットする（どちらも
    /// リポジトリ/ワークスペース依存のため。`full_name` は workspace を含むので、この比較
    /// だけで別ワークスペースの同名 repo も区別できる。同一 repo の選び直しでは維持する）。
    fn select_repo(&mut self, repo: &Repository) {
        if self.selected_repo.as_deref() != Some(repo.full_name.as_str()) {
            self.pr_state_filter.author = None;
            self.pr_state_filter.target_branch = None;
        }
        self.selected_repo = Some(repo.full_name.clone());
        self.repo_main_branch = repo.main_branch_name().map(str::to_string);
    }

    /// リポジトリを選択して PR 一覧へ入る。一覧での決定（Enter）とジャンプパレットの
    /// 双方から呼ぶ共通経路。
    fn enter_repository(&mut self, repo: Repository) -> Command {
        self.select_repo(&repo);
        self.open_pull_requests()
    }

    /// Source ルートに使う既定ブランチ名（未取得なら `main` にフォールバック）。
    fn default_source_ref(&self) -> String {
        self.repo_main_branch
            .clone()
            .unwrap_or_else(|| "main".to_string())
    }

    /// Repositories 一覧のサーバソートを次へ巡回し、1 ページ目から再取得する
    /// （Bitbucket ブラウザ版と同じ 4 種類。[`ListSort`]）。
    fn cycle_repositories_sort(&mut self) -> Command {
        self.repositories_sort = self.repositories_sort.next();
        self.load_repositories_page(1)
    }

    /// PR 一覧画面へ遷移し、現在の state フィルタで 1 ページ目の取得を開始する。
    ///
    /// states は永続化された選択（config.toml）をそのまま使う。author はリポジトリ/
    /// ワークスペース依存のため、repo コンテキストが実際に変わる場所でリセットする
    /// （[`App::select_repo`]/[`App::jump_to_pr`]。同一 repo への再入場では維持する）。
    fn open_pull_requests(&mut self) -> Command {
        self.screen = Screen::PullRequests;
        self.current_pr = None;
        self.load_pull_requests_page(1)
    }

    /// PR 一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::pull_requests_cache`]、キー = (repo slug, state フィルタ, ソート,
    /// ページ番号)）があれば即座に一覧を表示しつつ、裏で `Command::LoadPullRequests` を
    /// 発行して最新化する（stale-while-revalidate）。キャッシュが無ければ一覧をクリアして
    /// Loading 表示を出す。リポジトリへの新規入場・フィルタ切り替え・ソート変更（いずれも
    /// 1 ページ目から）・ページ移動（`[`/`]`/`g`）・手動リロード（`r`、現在ページを再取得）の
    /// 共通経路。
    fn load_pull_requests_page(&mut self, page: u32) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        let sort = self.pull_requests_sort;
        let filter = self.pr_state_filter.clone();

        let cache_key = (repo.clone(), filter.clone(), sort, page);
        match self.pull_requests_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_pull_requests(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.pull_requests.set_items(Vec::new());
                self.pull_requests_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!(
                    "PR 一覧を取得中…（{}・{}・{page} ページ目）",
                    filter.label(),
                    sort.label()
                ));
            }
        }

        Command::LoadPullRequests {
            client,
            workspace,
            repo,
            filter,
            sort,
            page,
        }
    }

    /// 現在のフィルタ・現在ページで PR 一覧を再取得する（`r` キー）。
    fn reload_pull_requests(&mut self) -> Command {
        self.load_pull_requests_page(self.pull_requests_page_info.page)
    }

    /// 取得結果を `pull_requests` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （リポジトリ変更・フィルタ変更・ページ変更・ソート変更）はここで選択を先頭にリセット
    /// する（キャッシュヒットによる即時表示（[`App::load_pull_requests_page`]）専用。
    /// バックグラウンド再検証結果は `Msg::PullRequestsLoaded` 側で選択を保持したまま
    /// 部分更新する）。
    ///
    /// 検索フィルタは（`SelectList::set_items` により）ここで必ずクリアされる。ページが変われば
    /// 表示される 40 件が入れ替わるため、前ページのフィルタを引き継がない設計判断。
    fn apply_pull_requests(&mut self, prs: Vec<PullRequest>, page_info: PageInfo) {
        self.pull_requests.set_items(prs);
        self.pull_requests_page_info = page_info;
    }

    fn on_key_pull_requests(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Char('/') => {
                self.search_editing = true;
                Command::None
            }
            KeyCode::Char('S') => self.cycle_pull_requests_sort(),
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
            KeyCode::Char('J') => {
                self.pull_requests.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pull_requests.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
            KeyCode::Char('o') => self.set_pr_states(BTreeSet::from([PrState::Open])),
            KeyCode::Char('m') => self.set_pr_states(BTreeSet::from([PrState::Merged])),
            KeyCode::Char('d') => self.set_pr_states(BTreeSet::from([PrState::Declined])),
            KeyCode::Char('a') => self.set_pr_states(PrStateFilter::all().states),
            KeyCode::Char('f') => self.open_pr_filter_modal(),
            KeyCode::Char('r') => self.reload_pull_requests(),
            KeyCode::Char('P') => {
                self.browse_return = Screen::PullRequests;
                self.open_pipelines()
            }
            KeyCode::Char('b') => {
                self.browse_return = Screen::PullRequests;
                self.open_branches()
            }
            KeyCode::Char('s') => {
                self.browse_return = Screen::PullRequests;
                let branch = self.default_source_ref();
                self.open_source_root(branch)
            }
            KeyCode::Enter => self.open_pr_detail(),
            _ => Command::None,
        }
    }

    /// state 集合だけを差し替え、1 ページ目から読み込み直す（`o`/`m`/`d`/`a` 単発キーの
    /// 共通経路）。author / target branch フィルタは state とは独立の軸なので維持する
    /// （解除はフィルタモーダル（`f`）で All authors / All branches を選ぶ）。
    fn set_pr_states(&mut self, states: BTreeSet<PrState>) -> Command {
        self.set_pr_filter(PrStateFilter {
            states,
            author: self.pr_state_filter.author.clone(),
            target_branch: self.pr_state_filter.target_branch.clone(),
        })
    }

    /// フィルタを切り替え、states を config.toml へ永続化し、1 ページ目から読み込み直す
    /// （新しいフィルタ文脈のため）。単発キーとフィルタモーダル適用の共通経路。
    fn set_pr_filter(&mut self, filter: PrStateFilter) -> Command {
        self.pr_state_filter = filter;
        self.persist_pr_states();
        self.load_pull_requests_page(1)
    }

    /// 現在の state フィルタ選択を config.toml へ保存する（author は保存しない）。
    fn persist_pr_states(&mut self) {
        self.config.pr_states = Some(self.pr_state_filter.config_states());
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない（他の config 保存箇所と同じ方針）。
            tracing::warn!(%error, "PR state フィルタ設定の保存に失敗しました");
        }
    }

    /// PR フィルタモーダル（`f`）を開く。author 候補（この repo の PR 著者）と target branch
    /// 候補（ブランチ 1 ページ）は repo 単位のキャッシュがあれば即表示し、無ければ遅延取得を
    /// 発行する（取得完了までは「読み込み中」表示）。
    fn open_pr_filter_modal(&mut self) -> Command {
        let repo_full_name = self.selected_repo.clone();
        let current_author = self.pr_state_filter.author.clone();
        let current_target = self.pr_state_filter.target_branch.clone();

        let mut authors = repo_full_name
            .as_ref()
            .and_then(|key| self.pr_authors_cache.get(key).cloned());
        if let Some(authors) = authors.as_mut() {
            insert_current_author(authors, current_author.as_ref());
        }
        let author_cursor = author_cursor_for(authors.as_deref(), current_author.as_ref());
        let need_author_fetch = authors.is_none();
        // 検索クエリは空で開く → マッチは恒等写像（全候補）。
        let author_matches = (0..authors.as_ref().map_or(0, Vec::len)).collect();

        let mut branches = repo_full_name
            .as_ref()
            .and_then(|key| self.branch_candidates_cache.get(key).cloned());
        if let Some(branches) = branches.as_mut() {
            insert_current_target(branches, current_target.as_ref());
        }
        let need_branch_fetch = branches.is_none();
        // 部分一致（exact=false）適用中は検索クエリへ復元し、カーソルを部分一致行に置く
        // （開いてそのまま Enter しても現在のフィルタを維持できるようにする。完全一致は
        // 候補行のカーソル位置で表現する）。
        let target_query = match &current_target {
            Some(target) if !target.exact => target.text.clone(),
            _ => String::new(),
        };
        let target_matches = match branches.as_deref() {
            Some(branches) => fuzzy_match_indices(
                &target_query,
                branches,
                |name| name.clone(),
                &mut self.pr_filter_matcher,
            ),
            None => Vec::new(),
        };
        let target_cursor = if target_query.is_empty() {
            target_cursor_for(branches.as_deref(), current_target.as_ref())
        } else {
            1
        };

        self.pr_filter_modal = Some(PrFilterModal {
            section: PrFilterSection::States,
            states: self.pr_state_filter.states.clone(),
            state_cursor: 0,
            author_cursor,
            authors,
            author_query: String::new(),
            author_matches,
            target_cursor,
            branches,
            target_query,
            target_matches,
        });

        let mut commands = Vec::new();
        if (need_author_fetch || need_branch_fetch)
            && let (Some((client, workspace, repo)), Some(repo_full_name)) =
                (self.review_context(), repo_full_name)
        {
            if need_author_fetch {
                commands.push(Command::LoadPrAuthors {
                    client: client.clone(),
                    workspace: workspace.clone(),
                    repo: repo.clone(),
                    repo_full_name: repo_full_name.clone(),
                });
            }
            if need_branch_fetch {
                commands.push(Command::LoadFilterBranches {
                    client,
                    workspace,
                    repo,
                    repo_full_name,
                });
            }
        }
        batch_or_none(commands)
    }

    fn on_key_pr_filter_modal(&mut self, key: KeyEvent) -> Command {
        let Some(modal) = self.pr_filter_modal.as_mut() else {
            return Command::None;
        };
        match key.code {
            KeyCode::Esc => {
                // 取消: 作業コピー（検索クエリ含む）を破棄し、現在のフィルタは変更しない。
                self.pr_filter_modal = None;
                Command::None
            }
            KeyCode::Tab => {
                // セクション移動では検索クエリを保持する（破棄は閉じたときのみ）。
                modal.section = modal.section.next();
                Command::None
            }
            KeyCode::BackTab => {
                modal.section = modal.section.previous();
                Command::None
            }
            KeyCode::Enter => self.apply_pr_filter_modal(),
            KeyCode::Down => {
                match modal.section {
                    PrFilterSection::States => {
                        modal.state_cursor = (modal.state_cursor + 1).min(PrState::ALL.len() - 1);
                    }
                    PrFilterSection::Author => {
                        let last = modal.author_row_count().saturating_sub(1);
                        modal.author_cursor = (modal.author_cursor + 1).min(last);
                    }
                    PrFilterSection::Target => {
                        let last = modal.target_row_count().saturating_sub(1);
                        modal.target_cursor = (modal.target_cursor + 1).min(last);
                    }
                }
                Command::None
            }
            KeyCode::Up => {
                match modal.section {
                    PrFilterSection::States => {
                        modal.state_cursor = modal.state_cursor.saturating_sub(1);
                    }
                    PrFilterSection::Author => {
                        modal.author_cursor = modal.author_cursor.saturating_sub(1);
                    }
                    PrFilterSection::Target => {
                        modal.target_cursor = modal.target_cursor.saturating_sub(1);
                    }
                }
                Command::None
            }
            KeyCode::Backspace => {
                // Author / Target セクションの検索クエリを 1 文字削除する（States では
                // 何もしない）。
                self.edit_pr_filter_query(|query| query.pop().is_some());
                Command::None
            }
            KeyCode::Char(c) => match modal.section {
                // State セクションの操作は従来どおり（`jk` 移動・`Space` トグル）。
                PrFilterSection::States => {
                    match c {
                        'j' => {
                            modal.state_cursor =
                                (modal.state_cursor + 1).min(PrState::ALL.len() - 1);
                        }
                        'k' => {
                            modal.state_cursor = modal.state_cursor.saturating_sub(1);
                        }
                        ' ' => {
                            if let Some(state) = PrState::ALL.get(modal.state_cursor).copied()
                                && !modal.states.remove(&state)
                            {
                                modal.states.insert(state);
                            }
                        }
                        _ => {}
                    }
                    Command::None
                }
                // Author / Target セクションでは印字文字がそのまま検索クエリ（`j`/`k`/空白も
                // 文字。候補の移動は `↑↓`）。Ctrl/Alt 付きは入力として扱わない（Ctrl+K 等の
                // 誤爆防止）。
                PrFilterSection::Author | PrFilterSection::Target => {
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    {
                        self.edit_pr_filter_query(|query| {
                            query.push(c);
                            true
                        });
                    }
                    Command::None
                }
            },
            _ => Command::None,
        }
    }

    /// フォーカス中セクション（Author/Target）の検索クエリを `edit` で編集し、変化したら
    /// 候補を再絞り込みする（`Backspace` の削除と印字文字の追記の共通経路。States
    /// セクションでは何もしない）。
    fn edit_pr_filter_query(&mut self, edit: impl FnOnce(&mut String) -> bool) {
        let Some(modal) = self.pr_filter_modal.as_mut() else {
            return;
        };
        let section = modal.section;
        let changed = match section {
            PrFilterSection::States => false,
            PrFilterSection::Author => edit(&mut modal.author_query),
            PrFilterSection::Target => edit(&mut modal.target_query),
        };
        if !changed {
            return;
        }
        match section {
            PrFilterSection::States => {}
            PrFilterSection::Author => self.refilter_pr_filter_authors(),
            PrFilterSection::Target => self.refilter_pr_filter_targets(),
        }
    }

    /// Author 検索クエリで候補を再絞り込みする。カーソルは、クエリ非空なら先頭マッチ、空へ
    /// 戻ったら現用フィルタの author の行（無ければ All authors。[`author_cursor_for`]）へ
    /// 戻す（入力→全消去で選択が黙って All authors へ落ちないようにする）。fuzzy は
    /// [`SelectList`] と同じ nucleo 経路（[`fuzzy_match_indices`]）を流用する。
    fn refilter_pr_filter_authors(&mut self) {
        let current = self.pr_state_filter.author.clone();
        let Some(modal) = self.pr_filter_modal.as_mut() else {
            return;
        };
        let authors: &[PrAuthor] = modal.authors.as_deref().unwrap_or(&[]);
        modal.author_matches = fuzzy_match_indices(
            &modal.author_query,
            authors,
            |author| author.display_name.clone(),
            &mut self.pr_filter_matcher,
        );
        modal.author_cursor = if modal.author_query.is_empty() {
            author_cursor_for(modal.authors.as_deref(), current.as_ref())
        } else {
            0
        };
    }

    /// Target 検索クエリで候補を再絞り込みする。カーソルは、クエリ非空なら部分一致行
    /// （テキスト入力の主目的）、空へ戻ったら現用フィルタの target の行（無ければ
    /// All branches。[`target_cursor_for`]）へ戻す（入力→全消去で選択が黙って All branches
    /// へ落ちないようにする）。fuzzy は Author と同じ nucleo 経路
    /// （[`fuzzy_match_indices`]）を流用する。
    fn refilter_pr_filter_targets(&mut self) {
        let current = self.pr_state_filter.target_branch.clone();
        let Some(modal) = self.pr_filter_modal.as_mut() else {
            return;
        };
        let branches: &[String] = modal.branches.as_deref().unwrap_or(&[]);
        modal.target_matches = fuzzy_match_indices(
            &modal.target_query,
            branches,
            |name| name.clone(),
            &mut self.pr_filter_matcher,
        );
        modal.target_cursor = if modal.target_query.is_empty() {
            target_cursor_for(modal.branches.as_deref(), current.as_ref())
        } else {
            1
        };
    }

    /// フィルタモーダルの内容を適用する（`Enter`）。states が空なら Status エラーを出して
    /// 適用しない（モーダルは開いたまま）。カーソル行 → 選択の写像は描画と共有する
    /// [`PrFilterModal::author_row`] / [`PrFilterModal::target_row`] に従う（選択を指さない行
    /// （読み込み中・該当なし・範囲外）は現用フィルタを維持し、黙って解除しない）。
    fn apply_pr_filter_modal(&mut self) -> Command {
        let Some(modal) = self.pr_filter_modal.as_ref() else {
            return Command::None;
        };
        if modal.states.is_empty() {
            self.status = Status::Error("state を 1 つ以上選択してください".to_string());
            return Command::None;
        }
        let author = match modal.author_row(modal.author_cursor) {
            PrFilterRow::All => None,
            PrFilterRow::Candidate(author) => Some(author.clone()),
            // Partial は Target 専用で Author 行には現れない（防御的に維持と同じ扱い）。
            PrFilterRow::Partial | PrFilterRow::Missing => self.pr_state_filter.author.clone(),
        };
        let target_branch = match modal.target_row(modal.target_cursor) {
            PrFilterRow::All => None,
            PrFilterRow::Partial => Some(TargetBranch {
                text: modal.target_query.clone(),
                exact: false,
            }),
            PrFilterRow::Candidate(name) => Some(TargetBranch {
                text: name.to_string(),
                exact: true,
            }),
            PrFilterRow::Missing => self.pr_state_filter.target_branch.clone(),
        };
        let filter = PrStateFilter {
            states: modal.states.clone(),
            author,
            target_branch,
        };
        self.pr_filter_modal = None;
        self.set_pr_filter(filter)
    }

    /// author 候補の取得結果（またはフォールバック）をフィルタモーダルへ反映する。
    /// 現在適用中の author が候補に無ければ挿入する（[`insert_current_author`]。モーダルの
    /// 作業コピーのみで、[`App::pr_authors_cache`] には影響しない）。
    ///
    /// 取得中に別リポジトリへ移動していた場合と、既に候補が入っている場合（キャッシュ
    /// 命中で開き直した後に古い取得結果が届いた場合）は反映しない。
    ///
    /// 読み込み中の Author セクションは選択可能な行を出さない（カーソルは実質動かせない）
    /// ため、ここでのカーソル設定がユーザーの選択を上書きすることはない（Target 側の
    /// [`App::fill_pr_filter_modal_branches`] は部分一致行が選べるため選択の意味を保持する）。
    fn fill_pr_filter_modal_authors(&mut self, repo_full_name: &str, mut authors: Vec<PrAuthor>) {
        if self.selected_repo.as_deref() != Some(repo_full_name) {
            return;
        }
        let current = self.pr_state_filter.author.clone();
        let mut refilter = false;
        if let Some(modal) = self.pr_filter_modal.as_mut()
            && modal.authors.is_none()
        {
            insert_current_author(&mut authors, current.as_ref());
            modal.author_cursor = author_cursor_for(Some(&authors), current.as_ref());
            modal.author_matches = (0..authors.len()).collect();
            modal.authors = Some(authors);
            // 読み込み中に検索クエリが入力済みなら、届いた候補で絞り込みを再計算する
            // （クエリ空なら上の恒等写像 + 現用 author カーソルのまま）。
            refilter = !modal.author_query.is_empty();
        }
        if refilter {
            self.refilter_pr_filter_authors();
        }
    }

    /// target branch 候補の取得結果（取得失敗時は空候補）をフィルタモーダルへ反映する。
    /// 現在適用中の完全一致 target が候補に無ければ挿入する（[`insert_current_target`]。
    /// モーダルの作業コピーのみで、[`App::branch_candidates_cache`] には影響しない）。
    ///
    /// 取得中に別リポジトリへ移動していた場合と、既に候補が入っている場合は反映しない
    /// （[`App::fill_pr_filter_modal_authors`] と同じガード）。
    fn fill_pr_filter_modal_branches(&mut self, repo_full_name: &str, mut branches: Vec<String>) {
        if self.selected_repo.as_deref() != Some(repo_full_name) {
            return;
        }
        let current = self.pr_state_filter.target_branch.clone();
        let mut preserved_cursor = None;
        if let Some(modal) = self.pr_filter_modal.as_mut()
            && modal.branches.is_none()
        {
            insert_current_target(&mut branches, current.as_ref());
            if modal.target_query.is_empty() {
                // クエリ空の読み込み中は選択可能な行が無い（ユーザーは行を選べていない）ため、
                // 現用フィルタの行へカーソルを置く。
                modal.target_cursor = target_cursor_for(Some(&branches), current.as_ref());
            } else {
                // 読み込み中に検索クエリが入力済み（部分一致フィルタからの復元を含む）なら、
                // 届いた候補で絞り込みを再計算する。行 0（All branches）/ 行 1（部分一致）は
                // 到着後も同じ位置にあるため、ユーザーが選択中の行の意味を保って復元する
                // （無条件に部分一致行へ戻さない）。
                preserved_cursor = Some(modal.target_cursor);
            }
            modal.target_matches = (0..branches.len()).collect();
            modal.branches = Some(branches);
        }
        if let Some(cursor) = preserved_cursor {
            self.refilter_pr_filter_targets();
            if let Some(modal) = self.pr_filter_modal.as_mut() {
                modal.target_cursor = cursor.min(modal.target_row_count().saturating_sub(1));
            }
        }
    }

    /// 読み込み済み PR の author から候補を作る（author 候補ソース（PR 集約 API）が
    /// 使えないときのフォールバック。uuid 無しの author は除外し、uuid で重複排除する）。
    ///
    /// 現在表示中の一覧だけでは author フィルタ適用済みの著者しか拾えず、フィルタ適用中に
    /// モーダルを開き直すと候補が現用 author だけへ痩せてしまう。そのため現在 repo の
    /// PR 一覧キャッシュ（[`App::pull_requests_cache`]、全フィルタ・全ソート・全ページ）を
    /// 集約し、表示中の一覧（キャッシュ無効化直後の取りこぼし防止）も合わせて候補にする。
    fn loaded_pr_authors(&self) -> Vec<PrAuthor> {
        let repo = self.repo_slug();
        let users: Vec<User> = self
            .pull_requests_cache
            .iter()
            .filter(|((cached_repo, _, _, _), _)| Some(cached_repo.as_str()) == repo.as_deref())
            .flat_map(|(_, (prs, _))| prs.iter())
            .chain(self.pull_requests.items.iter())
            .filter_map(|pr| pr.author.clone())
            .collect();
        users_to_authors(users)
    }

    /// PullRequests 一覧のサーバソートを次へ巡回し、1 ページ目から再取得する。
    fn cycle_pull_requests_sort(&mut self) -> Command {
        self.pull_requests_sort = self.pull_requests_sort.next();
        self.load_pull_requests_page(1)
    }

    // ---- サーバサイド・ページネーション（Workspaces/Repositories/PullRequests 共通） ----

    /// ワークスペース一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::workspaces_cache`]、キー = ページ番号）があれば即座に一覧を表示し
    /// つつ、裏で `Command::LoadWorkspaces` を発行して最新化する（stale-while-revalidate）。
    /// キャッシュが無ければ一覧をクリアして Loading 表示を出す。起動時（1 ページ目）と
    /// `[`/`]`/`g`（ページ移動）の双方から呼ぶ共通経路。
    fn load_workspaces_page(&mut self, page: u32) -> Command {
        match self.workspaces_cache.get(&page).cloned() {
            Some((cached, info)) => {
                self.apply_workspaces(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.workspaces.set_items(Vec::new());
                self.workspaces_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status =
                    Status::Loading(format!("ワークスペースを取得中…（{page} ページ目）"));
            }
        }

        match &self.client {
            Some(client) => Command::LoadWorkspaces {
                client: client.clone(),
                page,
            },
            None => {
                self.status = Status::Error("認証クライアントが未初期化です".to_string());
                Command::None
            }
        }
    }

    /// 取得結果を `workspaces` へ反映する（ページ状態の更新を含む）。新規取得
    /// （`Msg::WorkspacesLoaded`）とキャッシュからの即時表示（[`App::load_workspaces_page`]）の
    /// 両方から呼ぶ共通経路。検索フィルタは（`SelectList::set_items` により）ここで必ずクリア
    /// される（ページが変われば表示される 40 件が入れ替わるため）。
    fn apply_workspaces(&mut self, workspaces: Vec<Workspace>, page_info: PageInfo) {
        self.workspaces.set_items(workspaces);
        self.workspaces_page_info = page_info;
    }

    /// 現在の画面がページング対象（Workspaces/Repositories/PullRequests/Pipelines/Branches）
    /// なら、そのページ状態を返す。それ以外の画面では `None`（ページ移動キーは何もしない）。
    fn page_info(&self) -> Option<PageInfo> {
        match self.screen {
            Screen::Workspaces => Some(self.workspaces_page_info),
            Screen::Repositories => Some(self.repositories_page_info),
            Screen::PullRequests => Some(self.pull_requests_page_info),
            Screen::Pipelines => Some(self.pipelines_page_info),
            Screen::Branches => Some(self.branches_page_info),
            _ => None,
        }
    }

    /// 指定ページ番号のデータを、現在の画面に応じて読み込む。
    fn load_page(&mut self, page: u32) -> Command {
        match self.screen {
            Screen::Workspaces => self.load_workspaces_page(page),
            Screen::Repositories => self.load_repositories_page(page),
            Screen::PullRequests => self.load_pull_requests_page(page),
            Screen::Pipelines => self.load_pipelines_page(page),
            Screen::Branches => self.load_branches_page(page),
            _ => Command::None,
        }
    }

    /// 前ページへ（`[`）。1 ページ目では何もしない（パニックせずクランプ）。
    fn prev_page(&mut self) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        if info.page <= 1 {
            return Command::None;
        }
        self.load_page(info.page - 1)
    }

    /// 次ページへ（`]`）。`has_next`（次ページ無し）が `false` なら何もしない。
    fn next_page(&mut self) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        if !info.has_next {
            return Command::None;
        }
        self.load_page(info.page + 1)
    }

    /// 指定ページ番号へジャンプする（ページ番号ジャンプの入力確定時）。既知の総ページ数が
    /// あればその範囲へクランプし、無ければ 1 未満のみクランプする（範囲外はサーバ応答
    /// （`has_next=false` 等）に委ねる。パニックしない）。
    fn goto_page(&mut self, page: u32) -> Command {
        let Some(info) = self.page_info() else {
            return Command::None;
        };
        let target = match info.total_pages {
            Some(total) if total > 0 => page.clamp(1, total),
            _ => page.max(1),
        };
        self.load_page(target)
    }

    /// ページ番号ジャンプの入力プロンプトを開く（`g`）。ページング対象外の画面では何もしない。
    fn open_page_jump(&mut self) -> Command {
        if self.page_info().is_some() {
            self.page_jump = Some(PageJumpModal::default());
        }
        Command::None
    }

    /// ページ番号ジャンプの入力プロンプトのキー処理。数字のみ受け付け、`Enter` で確定
    /// （[`App::goto_page`]）、`Esc`/`Backspace` で取消/1 文字削除する。
    fn on_key_page_jump(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.page_jump = None;
                Command::None
            }
            KeyCode::Enter => {
                let Some(modal) = self.page_jump.take() else {
                    return Command::None;
                };
                match modal.input.parse::<u32>() {
                    Ok(page) if page >= 1 => self.goto_page(page),
                    _ => {
                        self.status =
                            Status::Error("1 以上のページ番号を入力してください".to_string());
                        Command::None
                    }
                }
            }
            KeyCode::Backspace => {
                if let Some(modal) = self.page_jump.as_mut() {
                    modal.input.pop();
                }
                Command::None
            }
            KeyCode::Char(ch) if ch.is_ascii_digit() => {
                // 極端な桁数の入力を防ぐ安全上限（6 桁あれば実用上十分）。
                if let Some(modal) = self.page_jump.as_mut()
                    && modal.input.len() < 6
                {
                    modal.input.push(ch);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// PR 詳細概要ペインの表示行数。描画後は rich document の仮想高さ、初回描画前だけは
    /// ヘッダ・参加者・本文の論理行数による近似値を返す。
    ///
    /// 本文行数は `tui_markdown` による実際の描画結果（[`App::detail_body_rendered_lines`]、
    /// `ui` が毎フレーム書き戻す）を優先して使う。まだ描画していない場合（画面遷移直後の 1 フレ
    /// ーム目やユニットテスト等）は、本文の生の行数から近似する（`tui_markdown` はソフト改行の
    /// 結合や見出し前後の空行挿入により行数が変わるため、この近似値は厳密ではないが、
    /// 「スクロール上限が無い」バグを防ぐには十分）。
    fn detail_body_line_count(&self) -> usize {
        let Some(pr) = self.current_pr.as_ref() else {
            return 0;
        };
        self.detail_body_rendered_lines.unwrap_or_else(|| {
            PR_DETAIL_HEADER_LINES
                + participant_panel_line_count(pr)
                + pr.body().map_or(1, |body| body.lines().count().max(1))
        })
    }

    /// `detail_scroll` の上限（本文が直近描画のビューポートに収まる位置）。
    fn detail_max_scroll(&self) -> u16 {
        let total = self.detail_body_line_count();
        let viewport = self.detail_viewport.max(1);
        total.saturating_sub(viewport).min(u16::MAX as usize) as u16
    }

    /// `detail_scroll` をビューポートに合わせてクランプする（`DiffState::max_scroll` /
    /// `LogView::clamp_scroll` と同じパターン）。`ui` が毎フレーム再クランプに使うため `pub`。
    pub fn clamp_detail_scroll(&mut self) {
        let max = self.detail_max_scroll();
        if self.detail_scroll > max {
            self.detail_scroll = max;
        }
    }

    fn comments_max_scroll(&self) -> u16 {
        let total = self.comments_rendered_lines.unwrap_or_else(|| {
            self.comments
                .iter()
                .map(|comment| comment.raw().lines().count().max(1) + 2)
                .sum::<usize>()
                .max(1)
        });
        total
            .saturating_sub(self.comments_viewport.max(1))
            .min(u16::MAX as usize) as u16
    }

    pub fn clamp_comments_scroll(&mut self) {
        self.comments_scroll = self.comments_scroll.min(self.comments_max_scroll());
    }

    /// 選択中の PR の詳細画面へ遷移し、詳細/diffstat/コメントの取得を開始する。
    fn open_pr_detail(&mut self) -> Command {
        let Some(pr) = self.pull_requests.selected().cloned() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.open_pr_detail_with(client, workspace, repo, pr)
    }

    /// 他のワークスペース/リポジトリの PR へ直接ジャンプする（ジャンプパレット用）。
    /// `selected_workspace`/`selected_repo` を明示的に切り替えてから通常の詳細取得経路
    /// （[`App::open_pr_detail_with`]）に合流する。
    fn jump_to_pr(
        &mut self,
        workspace: String,
        repo_full_name: String,
        pr: PullRequest,
    ) -> Command {
        // repo コンテキストが実際に変わる場合は author / target branch フィルタをリセット
        // する（どちらもリポジトリ/ワークスペース依存で、別リポジトリの uuid・ブランチ名が
        // 残留すると PR 一覧が誤った条件で絞られたままになる）。同一 repo へのジャンプでは
        // 維持する（[`App::select_repo`] と同じ方針）。
        if self.selected_workspace.as_deref() != Some(workspace.as_str())
            || self.selected_repo.as_deref() != Some(repo_full_name.as_str())
        {
            self.pr_state_filter.author = None;
            self.pr_state_filter.target_branch = None;
        }
        self.selected_workspace = Some(workspace);
        self.selected_repo = Some(repo_full_name);
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.open_pr_detail_with(client, workspace, repo, pr)
    }

    /// PR 詳細画面へ遷移し、詳細/diffstat/コメントの取得を発行する共通経路。
    /// 一覧での決定（[`App::open_pr_detail`]）とジャンプパレット（[`App::jump_to_pr`]）の
    /// 双方から呼ぶ。
    ///
    /// キャッシュ（[`App::pr_detail_cache`]、キー = (repo full_name, PR id)）があれば
    /// 詳細/diffstat/コメントを即座に表示しつつ、裏で 3 つの `Command::Load*` を発行して
    /// 最新化する（stale-while-revalidate）。承認状態やコメント数はレビュー中に変わり得る
    /// ため、キャッシュ命中時も必ず裏で再取得する。
    fn open_pr_detail_with(
        &mut self,
        client: BitbucketClient,
        workspace: String,
        repo: String,
        pr: PullRequest,
    ) -> Command {
        let id = pr.id;
        self.current_pr = Some(pr.clone());
        self.diff = None;
        self.detail_scroll = 0;
        self.comments_scroll = 0;
        self.detail_focus = DetailFocus::Overview;
        // 新しい PR の本文行数はまだ描画していない（前の PR の実測値を持ち越さない）。次回描画
        // で `ui::render_pr_meta_body` が実測して書き戻すまでは近似値にフォールバックする。
        self.detail_body_rendered_lines = None;
        self.overview_link_positions.clear();
        self.comments_rendered_lines = None;
        self.screen = Screen::PullRequestDetail;
        self.record_recent_pr(pr);

        let cache_key = self.selected_repo.clone().map(|full_name| (full_name, id));
        match cache_key.and_then(|key| self.pr_detail_cache.get(&key).cloned()) {
            Some(cached) => {
                self.current_pr = Some(cached.pr);
                self.diffstat.set_items(cached.diffstat);
                self.comments = cached.comments;
                self.status = Status::Idle;
            }
            None => {
                self.diffstat.set_items(Vec::new());
                self.comments = Vec::new();
                self.status = Status::Loading(format!("PR #{id} を取得中…"));
            }
        }

        let mut commands = vec![
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
                client: client.clone(),
                workspace,
                repo,
                id,
            },
        ];
        commands.extend(self.queue_overview_images(&client));
        Command::Batch(commands)
    }

    /// 現在の PR 本文にある全画像について、未キャッシュ・未取得中のものだけ取得を発行する。
    /// ImageView と同じ `Command::LoadImage` / `image_cache` 経路を使い、表示中の Status は
    /// 画像ロード用に上書きしない。
    fn queue_overview_images(&mut self, client: &BitbucketClient) -> Vec<Command> {
        let refs = self
            .current_pr
            .as_ref()
            .and_then(PullRequest::body)
            .map(imageview::extract_image_refs)
            .unwrap_or_default();
        let mut seen = HashSet::new();
        let mut commands = Vec::new();
        for image in refs {
            if !seen.insert(image.url.clone())
                || self.image_cache.get(&image.url).is_some()
                || !self.overview_images_loading.insert(image.url.clone())
            {
                continue;
            }
            commands.push(Command::LoadImage {
                client: client.clone(),
                url: image.url,
            });
        }
        commands
    }

    /// 「直近開いた PR」リストへ記録する（先頭に追加・同一 PR の重複は除去・上限 20 件）。
    /// ジャンプパレットの候補に使う。`selected_workspace`/`selected_repo` が未確定なら
    /// 何もしない（レビュー系操作ができない状態と同じ扱い）。
    fn record_recent_pr(&mut self, pr: PullRequest) {
        let Some(workspace) = self.selected_workspace.clone() else {
            return;
        };
        let Some(repo_full_name) = self.selected_repo.clone() else {
            return;
        };
        let id = pr.id;
        self.recent_prs
            .retain(|recent| !(recent.repo_full_name == repo_full_name && recent.pr.id == id));
        self.recent_prs.insert(
            0,
            RecentPr {
                workspace,
                repo_full_name,
                pr,
            },
        );
        const MAX_RECENT_PRS: usize = 20;
        self.recent_prs.truncate(MAX_RECENT_PRS);
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
            KeyCode::Tab => {
                self.detail_focus = self.detail_focus.next();
                Command::None
            }
            KeyCode::BackTab => {
                self.detail_focus = self.detail_focus.previous();
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_detail_focus(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_detail_focus(-1),
            KeyCode::PageDown => self.page_detail_focus(false),
            KeyCode::PageUp => self.page_detail_focus(true),
            KeyCode::Char('J') => self.move_detail_focus(10),
            KeyCode::Char('K') => self.move_detail_focus(-10),
            KeyCode::Char('g') => self.goto_detail_focus(false),
            KeyCode::Char('G') => self.goto_detail_focus(true),
            KeyCode::Char('d') => self.open_diff(),
            KeyCode::Char('c') => {
                self.comment_editor = Some(CommentEditor::default());
                Command::None
            }
            KeyCode::Char('a') => self.toggle_approve(),
            KeyCode::Char('x') => self.toggle_request_changes(),
            KeyCode::Char('M') => self.open_merge_modal(),
            KeyCode::Char('o') => self.open_pr_in_browser(),
            KeyCode::Char('i') => self.open_image_view(),
            KeyCode::Char('L') => self.open_link_palette(),
            _ => Command::None,
        }
    }

    fn move_detail_focus(&mut self, amount: i32) -> Command {
        match self.detail_focus {
            DetailFocus::Overview => {
                self.detail_scroll = add_signed(self.detail_scroll, amount);
                self.clamp_detail_scroll();
            }
            DetailFocus::Files => {
                if amount >= 0 {
                    self.diffstat.select_next_by(amount as usize);
                } else {
                    self.diffstat.select_prev_by(amount.unsigned_abs() as usize);
                }
            }
            DetailFocus::Comments => {
                self.comments_scroll = add_signed(self.comments_scroll, amount);
                self.clamp_comments_scroll();
            }
        }
        Command::None
    }

    fn page_detail_focus(&mut self, upward: bool) -> Command {
        let amount = match self.detail_focus {
            DetailFocus::Overview => self.detail_viewport.max(1),
            DetailFocus::Comments => self.comments_viewport.max(1),
            DetailFocus::Files => return Command::None,
        };
        self.move_detail_focus(if upward {
            -(amount as i32)
        } else {
            amount as i32
        })
    }

    fn goto_detail_focus(&mut self, end: bool) -> Command {
        match self.detail_focus {
            DetailFocus::Overview => {
                self.detail_scroll = if end { self.detail_max_scroll() } else { 0 };
            }
            DetailFocus::Files => {
                let selected = if self.diffstat.matches.is_empty() {
                    None
                } else if end {
                    Some(self.diffstat.matches.len() - 1)
                } else {
                    Some(0)
                };
                self.diffstat.state.select(selected);
            }
            DetailFocus::Comments => {
                self.comments_scroll = if end { self.comments_max_scroll() } else { 0 };
            }
        }
        Command::None
    }

    fn open_link_palette(&mut self) -> Command {
        let mut links = Vec::new();
        if let Some(body) = self.current_pr.as_ref().and_then(PullRequest::body) {
            extract_links(body, &mut links);
        }
        for comment in &self.comments {
            extract_links(comment.raw(), &mut links);
        }
        if links.is_empty() {
            self.status = Status::Error("本文にリンクがありません".to_string());
            return Command::None;
        }
        let mut palette = LinkPalette::default();
        palette.links.set_items(links);
        self.link_palette = Some(palette);
        Command::None
    }

    fn on_key_link_palette(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.link_palette = None;
                Command::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(palette) = self.link_palette.as_mut() {
                    palette.links.select_next();
                }
                Command::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(palette) = self.link_palette.as_mut() {
                    palette.links.select_prev();
                }
                Command::None
            }
            KeyCode::Enter => {
                let url = self
                    .link_palette
                    .as_ref()
                    .and_then(|palette| palette.links.selected())
                    .map(|link| link.url.clone());
                self.link_palette = None;
                match url {
                    Some(url) => self.open_url_in_browser(&url),
                    None => Command::None,
                }
            }
            _ => Command::None,
        }
    }

    /// PR 本文の画像一覧から ImageView を開く（`i`）。
    ///
    /// 本文に画像が無ければ Status にエラーを出す。画像表示機能が無効
    /// （`image_picker` が `None` ＝起動時の端末検出に失敗）な環境では、その旨を案内し
    /// ImageView へは遷移しない（アプリは落ちない）。
    fn open_image_view(&mut self) -> Command {
        let Some(pr) = self.current_pr.as_ref() else {
            self.status = Status::Error("PR が選択されていません".to_string());
            return Command::None;
        };
        let refs = pr
            .body()
            .map(imageview::extract_image_refs)
            .unwrap_or_default();
        if refs.is_empty() {
            self.status = Status::Error("本文に画像がありません".to_string());
            return Command::None;
        }
        if self.image_picker.is_none() {
            self.status =
                Status::Error("この端末は画像表示に未対応です（o でブラウザ表示）".to_string());
            return Command::None;
        }
        self.image_refs = refs;
        self.image_index = 0;
        self.current_image = None;
        self.image_protocol = None;
        self.screen = Screen::ImageView;
        self.load_current_image()
    }

    /// ImageView のキー処理。`Esc` で PR 詳細へ戻る。`n`/`p`/`←→` で画像を巡回する
    /// （境界ではクランプし、循環しない）。
    fn on_key_image_view(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = Screen::PullRequestDetail;
                self.status = Status::Idle;
                Command::None
            }
            KeyCode::Right | KeyCode::Char('n') => self.next_image(),
            KeyCode::Left | KeyCode::Char('p') => self.prev_image(),
            _ => Command::None,
        }
    }

    /// 次の画像へ（末尾では何もしない＝クランプ）。
    fn next_image(&mut self) -> Command {
        if self.image_index + 1 >= self.image_refs.len() {
            return Command::None;
        }
        self.image_index += 1;
        self.load_current_image()
    }

    /// 前の画像へ（先頭では何もしない＝クランプ）。
    fn prev_image(&mut self) -> Command {
        if self.image_index == 0 {
            return Command::None;
        }
        self.image_index -= 1;
        self.load_current_image()
    }

    /// `image_index` が指す画像を表示する。キャッシュ済みなら即座に反映し、未取得なら
    /// [`Command::LoadImage`] を発行する。
    fn load_current_image(&mut self) -> Command {
        let Some(current) = self.image_refs.get(self.image_index).cloned() else {
            return Command::None;
        };
        if let Some(cached) = self.image_cache.get(&current.url).cloned() {
            self.set_current_image(cached);
            return Command::None;
        }
        let Some(client) = self.client.clone() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.current_image = None;
        self.image_protocol = None;
        self.status = Status::Loading("画像を取得中…".to_string());
        Command::LoadImage {
            client,
            url: current.url,
        }
    }

    /// 現在表示中の画像結果を `current_image`/`image_protocol`/`status` へ反映する
    /// （[`Msg::ImageLoaded`] とキャッシュ即時ヒットの双方から呼ぶ共通処理）。
    ///
    /// デコード成功（`Ok`）かつ `image_picker` が利用可能な場合のみ、
    /// `Picker::new_resize_protocol` で描画用の `StatefulProtocol` を新規生成する
    /// （生成自体は軽量で、実際のリサイズ・エンコードは描画時に遅延される）。
    fn set_current_image(&mut self, result: Result<DynamicImage, String>) {
        self.image_protocol = match (&result, self.image_picker.as_ref()) {
            (Ok(image), Some(picker)) => Some(picker.new_resize_protocol(image.clone())),
            _ => None,
        };
        self.status = match &result {
            Ok(_) => Status::Idle,
            Err(message) => Status::Error(message.clone()),
        };
        self.current_image = Some(result);
    }

    /// 概要内画像の現在のレイアウト状態を、画像キャッシュと Picker から組み立てる。
    pub fn overview_image_presentation(
        &self,
        alt: &str,
        url: &str,
        pane_width: u16,
    ) -> ImagePresentation {
        let key = url.to_string();
        let result = self.image_cache.get(&key);
        let font_size = self.image_picker.as_ref().map(Picker::font_size);
        richdoc::image_presentation(alt, result, font_size, pane_width)
    }

    /// URL ごとの概要用 `StatefulProtocol` を返す。キャッシュ済み画像に対してまだ protocol が
    /// 無い場合（たとえば別 PR で先に取得済み）は、その場で一度だけ生成する。
    pub fn overview_image_protocol_mut(&mut self, url: &str) -> Option<&mut StatefulProtocol> {
        if !self.overview_image_protocols.contains_key(url) {
            let key = url.to_string();
            let image = self
                .image_cache
                .get(&key)
                .and_then(|result| result.as_ref().ok())
                .cloned();
            if let (Some(image), Some(picker)) = (image, self.image_picker.as_ref()) {
                self.overview_image_protocols
                    .insert(key, picker.new_resize_protocol(image));
            }
        }
        self.overview_image_protocols.get_mut(url)
    }

    /// 現在の PR をデフォルトブラウザで開く（macOS の `open` コマンドを子プロセスで起動）。
    ///
    /// TUI を抜けず、子プロセスの stdout/stderr は端末を汚さないよう `Stdio::null()` へ
    /// 捨てる。起動の成否のみを `Status` に反映する（終了を待たない = ブロックしない）。
    fn open_pr_in_browser(&mut self) -> Command {
        let Some(pr) = self.current_pr.as_ref() else {
            self.status = Status::Error("PR が選択されていません".to_string());
            return Command::None;
        };
        let Some(url) = pr.html_url().map(str::to_string) else {
            self.status = Status::Error("この PR のブラウザ URL が不明です".to_string());
            return Command::None;
        };
        self.open_url_in_browser(&url)
    }

    fn open_url_in_browser(&mut self, url: &str) -> Command {
        let result = std::process::Command::new("open")
            .arg(url)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
        self.status = match result {
            Ok(_) => Status::Success("ブラウザで開きました".to_string()),
            Err(err) => Status::Error(format!("ブラウザを開けませんでした: {err}")),
        };
        Command::None
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
                    editor.insert_char('\n');
                }
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(editor) = self.comment_editor.as_mut()
                    && !editor.submitting
                {
                    editor.backspace();
                }
                Command::None
            }
            KeyCode::Left => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_left();
                }
                Command::None
            }
            KeyCode::Right => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_right();
                }
                Command::None
            }
            KeyCode::Home => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_line_home();
                }
                Command::None
            }
            KeyCode::End => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_line_end();
                }
                Command::None
            }
            KeyCode::Up => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_line_up();
                }
                Command::None
            }
            KeyCode::Down => {
                if let Some(editor) = self.comment_editor.as_mut() {
                    editor.move_line_down();
                }
                Command::None
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                if let Some(editor) = self.comment_editor.as_mut()
                    && !editor.submitting
                {
                    editor.insert_char(ch);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// コメントエディタの内容を投稿する。`editor.inline` が `Some` ならインラインコメント
    /// （`Command::CreateInlineComment`）、`None` なら一般コメント（`Command::CreateComment`）。
    fn submit_comment(&mut self) -> Command {
        let Some(editor) = self.comment_editor.as_ref() else {
            return Command::None;
        };
        if editor.submitting || !editor.is_submittable() {
            return Command::None;
        }
        let raw = editor.text.trim_end().to_string();
        let inline = editor.inline.clone();
        let reply_to = editor.reply_to;
        let editing = editor.editing;
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
        // 編集を最優先。次いで返信（`reply_to`）、インライン（新規スレッド）、
        // どれでもなければ PR 全体への一般コメント。
        if let Some(comment_id) = editing {
            return Command::EditComment {
                client,
                workspace,
                repo,
                id,
                comment_id,
                raw,
            };
        }
        if let Some(parent_id) = reply_to {
            return Command::CreateReply {
                client,
                workspace,
                repo,
                id,
                parent_id,
                raw,
            };
        }
        match inline {
            Some(anchor) => Command::CreateInlineComment {
                client,
                workspace,
                repo,
                id,
                path: anchor.path,
                side: anchor.side,
                line: anchor.line,
                raw,
            },
            None => Command::CreateComment {
                client,
                workspace,
                repo,
                id,
                raw,
            },
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
            KeyCode::Tab => {
                // サイドバーが非表示の間はファイル一覧へフォーカスを移せない（本文固定）。
                if self.diff_sidebar_visible
                    && let Some(diff) = self.diff.as_mut()
                {
                    diff.toggle_focus();
                }
                return Command::None;
            }
            KeyCode::Char('c') => return self.open_inline_comment_editor(),
            KeyCode::Char('r') => return self.open_reply_editor(),
            KeyCode::Char('e') => return self.open_edit_editor(),
            KeyCode::Char('d') => return self.request_delete_comment(),
            KeyCode::Char('R') => return self.toggle_resolve_comment(),
            KeyCode::Char('v') => return self.toggle_diff_view_mode(),
            KeyCode::Char('t') => return self.toggle_diff_sidebar(),
            _ => {}
        }

        let Some(diff) = self.diff.as_mut() else {
            return Command::None;
        };

        // ファイル一覧サイドバーにフォーカス中は ↑↓/jk がファイル選択（本文が追従）。
        // `n`/`N` は境界ジャンプとして本文フォーカス時と同じ挙動（サイドバー選択も同期）。
        if diff.focus == DiffFocus::Files {
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => diff.select_file_next(),
                KeyCode::Up | KeyCode::Char('k') => diff.select_file_prev(),
                KeyCode::Char('n') => diff.next_file(),
                KeyCode::Char('N') => diff.prev_file(),
                _ => {}
            }
            return Command::None;
        }

        // 本文フォーカス中は「現在行（カーソル）」を動かす（画面のスクロールではなく行選択。
        // `DiffState::move_cursor` 等が viewport 内に収まるよう `scroll` を自動追従させる）。
        let page = diff.viewport.max(1) as i64;
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => diff.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => diff.move_cursor(-1),
            KeyCode::Char('J') => diff.move_cursor(10),
            KeyCode::Char('K') => diff.move_cursor(-10),
            KeyCode::PageDown | KeyCode::Char('f') => diff.page_cursor(page),
            KeyCode::PageUp | KeyCode::Char('b') => diff.page_cursor(-page),
            KeyCode::Char('g') | KeyCode::Home => diff.cursor_to_top(),
            KeyCode::Char('G') | KeyCode::End => diff.cursor_to_bottom(),
            KeyCode::Char('n') => diff.next_file(),
            KeyCode::Char('N') => diff.prev_file(),
            // カーソルがコメント/コラプス行に乗っているとき、スレッドの折りたたみをトグル。
            KeyCode::Enter => {
                if let Some(root) = diff.cursor_thread() {
                    diff.toggle_thread_collapse(root);
                }
            }
            _ => {}
        }
        Command::None
    }

    /// Diff 画面の派生データ（サイドバーのツリー行列・インラインコメント配置）を、現在の
    /// `parsed` と `comments` から作り直す。コメントスレッドは PR 差分
    /// （`diff_return == Screen::PullRequestDetail`）のときだけ配置する（コミット差分には
    /// PR コメントが紐づかないため空にする）。
    fn rebuild_diff_derived(&mut self) {
        let is_pr_diff = self.diff_return == Screen::PullRequestDetail;
        let Some(diff) = self.diff.as_mut() else {
            return;
        };
        // コメント配置が変わると表示行列がずれ、カーソルが指す位置も変わってしまう。変更前に
        // 見ていたコメント（あれば）と unified 行を覚えておき、作り直したあと同じコメント／行へ
        // 戻す（返信/編集の直後や非同期のコメント取得でカーソルが飛ばないように）。
        let prev_comment = diff.cursor_comment();
        let anchor = diff.cursor_unified_line();
        // 解決状態の変化を検知するため、再構築前のスレッド別 resolved を控える。
        let old_resolved: HashMap<u64, bool> = diff
            .comment_layout
            .threads_by_line
            .values()
            .flatten()
            .map(|thread| (thread.root_id, thread.resolved))
            .collect();
        diff.sidebar_rows = build_sidebar_rows(&diff.parsed.files);
        diff.comment_layout = if is_pr_diff {
            build_comment_layout(&diff.parsed, &self.comments, &self.me)
        } else {
            CommentLayout::default()
        };
        // 解決状態が変わったスレッドは手動の折りたたみ上書きを破棄し、既定
        // （解決済み=折りたたみ / 未解決=展開）へ戻す（再解決したのに展開が残り続けるのを防ぐ）。
        let new_resolved: HashMap<u64, bool> = diff
            .comment_layout
            .threads_by_line
            .values()
            .flatten()
            .map(|thread| (thread.root_id, thread.resolved))
            .collect();
        diff.thread_collapse.retain(|root, _| {
            matches!((old_resolved.get(root), new_resolved.get(root)),
                (Some(old), Some(new)) if old == new)
        });
        diff.rebuild_display_rows();
        diff.reanchor_cursor(prev_comment, anchor);
        diff.ensure_cursor_visible();
    }

    /// Diff 画面の現在行にインラインコメントエディタを開く（`c`）。
    ///
    /// PR 差分（`diff_return == Screen::PullRequestDetail` かつ `current_pr` あり）でのみ有効。
    /// コミット差分では投稿先の PR が無いため、その旨を `Status` に出して何もしない。
    /// 現在行がメタ/ヘッダ/ハンク行でコメント不可の場合もエラーを表示するのみ。
    fn open_inline_comment_editor(&mut self) -> Command {
        if self.diff_return != Screen::PullRequestDetail || self.current_pr.is_none() {
            self.status = Status::Error("コミット差分にはコメントできません".to_string());
            return Command::None;
        }
        let Some(diff) = self.diff.as_ref() else {
            return Command::None;
        };
        let Some(anchor) = diff.current_comment_anchor() else {
            self.status = Status::Error("この行にはコメントできません".to_string());
            return Command::None;
        };
        self.comment_editor = Some(CommentEditor::inline(anchor));
        Command::None
    }

    /// カーソルで選択中のコメントへ返信エディタを開く（`r`）。PR 差分でのみ有効。
    /// コメントを選択していない（diff 行にいる）場合は案内を出す。
    fn open_reply_editor(&mut self) -> Command {
        let Some((_, comment_id)) = self.diff.as_ref().and_then(DiffState::cursor_comment) else {
            if self.is_pr_diff_writable() {
                self.status = Status::Error("返信するコメントを ↑↓ で選択してください".to_string());
            }
            return Command::None;
        };
        self.reply_to_comment(comment_id)
    }

    /// 指定コメントへ返信エディタを開く（キー/クリック共通。`parent` = そのコメント）。
    fn reply_to_comment(&mut self, comment_id: u64) -> Command {
        if !self.is_pr_diff_writable() {
            return Command::None;
        }
        self.comment_editor = Some(CommentEditor::reply(comment_id));
        Command::None
    }

    /// カーソルで選択中の自分のコメントを編集するエディタを開く（`e`）。
    fn open_edit_editor(&mut self) -> Command {
        let Some((_, comment_id)) = self.diff.as_ref().and_then(DiffState::cursor_comment) else {
            if self.is_pr_diff_writable() {
                self.status = Status::Error("編集するコメントを ↑↓ で選択してください".to_string());
            }
            return Command::None;
        };
        self.edit_comment_by_id(comment_id)
    }

    /// 指定コメントの編集エディタを開く（キー/クリック共通。自分のコメントのみ。本文プリフィル）。
    fn edit_comment_by_id(&mut self, comment_id: u64) -> Command {
        if !self.is_pr_diff_writable() {
            return Command::None;
        }
        if !self.comment_is_mine(comment_id) {
            self.status = Status::Error("自分のコメントのみ編集できます".to_string());
            return Command::None;
        }
        // 現在の本文を取得してプリフィルする（見つからなければ空で開く）。
        let text = self
            .comments
            .iter()
            .find(|comment| comment.id == comment_id)
            .map(|comment| comment.raw().to_string())
            .unwrap_or_default();
        self.comment_editor = Some(CommentEditor::edit(comment_id, text));
        Command::None
    }

    /// カーソルで選択中のコメントを削除する確認モーダルを開く（`d`）。自分のコメントのみ。
    fn request_delete_comment(&mut self) -> Command {
        let Some((_, comment_id)) = self.diff.as_ref().and_then(DiffState::cursor_comment) else {
            if self.is_pr_diff_writable() {
                self.status = Status::Error("削除するコメントを ↑↓ で選択してください".to_string());
            }
            return Command::None;
        };
        self.request_delete_by_id(comment_id)
    }

    /// 指定コメントの削除確認モーダルを開く（キー/クリック共通。自分のコメントのみ）。
    fn request_delete_by_id(&mut self, comment_id: u64) -> Command {
        if !self.is_pr_diff_writable() {
            return Command::None;
        }
        if !self.comment_is_mine(comment_id) {
            self.status = Status::Error("自分のコメントのみ削除できます".to_string());
            return Command::None;
        }
        self.delete_comment_modal = Some(DeleteCommentModal {
            comment_id,
            submitting: false,
        });
        Command::None
    }

    /// 指定コメントが自分の投稿か（`e`/`d` を自コメントに限定する）。
    fn comment_is_mine(&self, comment_id: u64) -> bool {
        self.comments
            .iter()
            .find(|comment| comment.id == comment_id)
            .map(|comment| user_is_me(comment.user.as_ref(), &self.me))
            .unwrap_or(false)
    }

    /// コメント削除確認モーダルのキー処理（Enter=確定 / Esc・n=取消）。
    fn on_key_delete_comment_modal(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') => {
                self.delete_comment_modal = None;
                Command::None
            }
            KeyCode::Enter | KeyCode::Char('y') => self.confirm_delete_comment(),
            _ => Command::None,
        }
    }

    /// 削除確認モーダルの確定。`Command::DeleteComment` を発行する。
    fn confirm_delete_comment(&mut self) -> Command {
        let Some(modal) = self.delete_comment_modal.as_ref() else {
            return Command::None;
        };
        if modal.submitting {
            return Command::None;
        }
        let comment_id = modal.comment_id;
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        if let Some(modal) = self.delete_comment_modal.as_mut() {
            modal.submitting = true;
        }
        self.status = Status::Loading("コメントを削除中…".to_string());
        Command::DeleteComment {
            client,
            workspace,
            repo,
            id,
            comment_id,
        }
    }

    /// カーソルで選択中のスレッドの解決/再オープンをトグルする（`R`）。
    fn toggle_resolve_comment(&mut self) -> Command {
        let Some(thread_root) = self.diff.as_ref().and_then(DiffState::cursor_thread) else {
            if self.is_pr_diff_writable() {
                self.status =
                    Status::Error("解決するスレッドのコメントを ↑↓ で選択してください".to_string());
            }
            return Command::None;
        };
        self.resolve_thread(thread_root)
    }

    /// 指定スレッドの解決/再オープンをトグルする（キー/クリック共通）。
    fn resolve_thread(&mut self, thread_root: u64) -> Command {
        if !self.is_pr_diff_writable() {
            return Command::None;
        }
        let resolved = self
            .diff
            .as_ref()
            .is_some_and(|diff| diff.thread_resolved(thread_root));
        let Some(id) = self.current_pr_id() else {
            return Command::None;
        };
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.status = Status::Loading(if resolved {
            "スレッドを再オープン中…".to_string()
        } else {
            "スレッドを解決中…".to_string()
        });
        Command::ResolveComment {
            client,
            workspace,
            repo,
            id,
            comment_id: thread_root,
            resolve: !resolved,
        }
    }

    /// 書込み系（コメント操作）の前提を満たすか（PR 差分かつ PR あり）。満たさなければ
    /// `Status` にエラーを出して `false`。
    fn is_pr_diff_writable(&mut self) -> bool {
        if self.diff_return != Screen::PullRequestDetail || self.current_pr.is_none() {
            self.status = Status::Error("コミット差分にはコメントできません".to_string());
            return false;
        }
        true
    }

    /// Diff 画面の表示モード（unified/split）を切り替える（`v`）。`config.toml` へ永続化し、
    /// 開いている diff があれば現在行を新モードの対応する行へ変換しつつ切り替える
    /// （[`DiffState::set_view_mode`]）。着色済み行キャッシュ（`rendered_lines`/
    /// `rendered_split`）は無効化しない: モードごとに別フィールドで独立にキャッシュしており、
    /// 一度構築した側は再度そのモードへ戻った際にそのまま再利用できるため。
    fn toggle_diff_view_mode(&mut self) -> Command {
        self.diff_view_mode = self.diff_view_mode.toggled();
        if let Some(diff) = self.diff.as_mut() {
            diff.set_view_mode(self.diff_view_mode);
        }
        self.config.diff_view = Some(self.diff_view_mode.as_str().to_string());
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない（他の config 保存箇所と同じ方針）。
            tracing::warn!(%error, "Diff 表示モード設定の保存に失敗しました");
        }
        Command::None
    }

    /// Diff 画面のファイル一覧サイドバーの表示/非表示を切り替える（`t`）。非表示にする際は
    /// フォーカスを本文へ固定する（サイドバーが無い間は `Tab` でファイル一覧へ移れない。
    /// [`App::on_key_diff`] の `Tab` 分岐も同じ不変条件を守る）。`config.toml` へ即時永続化する
    /// （他のトグル系設定と同じ方針。[`App::persist_diff_sidebar`]）。
    fn toggle_diff_sidebar(&mut self) -> Command {
        self.diff_sidebar_visible = !self.diff_sidebar_visible;
        if !self.diff_sidebar_visible
            && let Some(diff) = self.diff.as_mut()
        {
            diff.focus = DiffFocus::Body;
        }
        self.persist_diff_sidebar();
        Command::None
    }

    /// Diff サイドバーの表示状態・幅を `config.toml` へ保存する（`t` トグル
    /// [`App::toggle_diff_sidebar`]・ドラッグ確定 [`App::on_mouse_up`] の双方から呼ぶ）。
    fn persist_diff_sidebar(&mut self) {
        self.config.diff_sidebar_visible = Some(self.diff_sidebar_visible);
        self.config.diff_sidebar_width = self.diff_sidebar_width;
        if let Err(error) = self.config.save() {
            // 設定保存の失敗は致命ではない（他の config 保存箇所と同じ方針）。
            tracing::warn!(%error, "Diff サイドバー設定の保存に失敗しました");
        }
    }

    /// [`App::diff_sidebar_width`] から、幅 `total` の Diff 画面で実際に描画すべきサイドバー幅
    /// （セル数）を求める（[`resolve_diff_sidebar_width`] 参照）。`ui::render_diff` から使う。
    pub fn diff_sidebar_render_width(&self, total: u16) -> u16 {
        resolve_diff_sidebar_width(total, self.diff_sidebar_width)
    }

    // ---- パイプライン監視（M2） ----

    /// Pipelines 一覧画面へ遷移し、1 ページ目の取得を開始する。
    fn open_pipelines(&mut self) -> Command {
        self.screen = Screen::Pipelines;
        self.current_pipeline = None;
        self.load_pipelines_page(1)
    }

    /// パイプライン一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::pipelines_cache`]、キー = (repo slug, ページ番号)）があれば即座に
    /// 一覧を表示しつつ、裏で `Command::LoadPipelines` を発行して最新化する
    /// （stale-while-revalidate）。キャッシュが無ければ一覧をクリアして Loading 表示を出す。
    /// [`App::open_pipelines`]（新規入場は 1 ページ目から）・`[`/`]`/`g`（ページ移動）・
    /// `r`（手動リロード、現在ページを再取得）の共通経路。
    fn load_pipelines_page(&mut self, page: u32) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };

        let cache_key = (repo.clone(), page);
        match self.pipelines_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_pipelines(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.pipelines.set_items(Vec::new());
                self.pipelines_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status =
                    Status::Loading(format!("パイプライン一覧を取得中…（{page} ページ目）"));
            }
        }

        Command::LoadPipelines {
            client,
            workspace,
            repo,
            page,
        }
    }

    /// 取得結果を `pipelines` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （リポジトリ変更・ページ変更）はここで選択を先頭にリセットする（キャッシュヒットに
    /// よる即時表示（[`App::load_pipelines_page`]）専用。バックグラウンド再検証結果は
    /// `Msg::PipelinesLoaded` 側で選択を保持したまま部分更新する）。
    fn apply_pipelines(&mut self, pipelines: Vec<Pipeline>, page_info: PageInfo) {
        self.pipelines.set_items(pipelines);
        self.pipelines_page_info = page_info;
    }

    /// 現在ページでパイプライン一覧を再取得する（`r` キー）。
    fn reload_pipelines(&mut self) -> Command {
        self.load_pipelines_page(self.pipelines_page_info.page)
    }

    /// 現在ページのパイプライン一覧を静かに再取得する（自動ポーリング・stop/re-run 後用・
    /// Loading 表示なし）。ページ移動中に古いページの tick 結果で上書きしないよう、常に
    /// `self.pipelines_page_info.page`（＝現在表示中のページ）を対象にする。
    fn refresh_pipelines_silent(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            return Command::None;
        };
        Command::LoadPipelines {
            client,
            workspace,
            repo,
            page: self.pipelines_page_info.page,
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
                self.screen = self.browse_return;
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
            KeyCode::Char('J') => {
                self.pipelines.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pipelines.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
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
            KeyCode::Char('J') => {
                self.pipeline_steps.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.pipeline_steps.select_prev_by(10);
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
            KeyCode::Char('J') => log.scroll_down(10),
            KeyCode::Char('K') => log.scroll_up(10),
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

    /// Branches 一覧画面へ遷移し、1 ページ目の取得を開始する。
    fn open_branches(&mut self) -> Command {
        self.screen = Screen::Branches;
        self.load_branches_page(1)
    }

    /// ブランチ一覧の指定ページを読み込む。
    ///
    /// キャッシュ（[`App::branches_cache`]、キー = (repo slug, ページ番号)）があれば即座に
    /// 一覧を表示しつつ、裏で `Command::LoadBranches` を発行して最新化する
    /// （stale-while-revalidate）。キャッシュが無ければ一覧をクリアして Loading 表示を出す。
    /// [`App::open_branches`]（新規入場は 1 ページ目から）・`[`/`]`/`g`（ページ移動）・
    /// `r`（手動リロード、現在ページを再取得）の共通経路。
    fn load_branches_page(&mut self, page: u32) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };

        let cache_key = (repo.clone(), page);
        match self.branches_cache.get(&cache_key).cloned() {
            Some((cached, info)) => {
                self.apply_branches(cached, info);
                self.status = Status::Idle;
            }
            None => {
                self.branches.set_items(Vec::new());
                self.branches_page_info = PageInfo {
                    page,
                    total_pages: None,
                    has_next: false,
                };
                self.status = Status::Loading(format!("ブランチ一覧を取得中…（{page} ページ目）"));
            }
        }

        Command::LoadBranches {
            client,
            workspace,
            repo,
            page,
        }
    }

    /// 現在ページでブランチ一覧を再取得する（`r` キー）。
    fn reload_branches(&mut self) -> Command {
        self.load_branches_page(self.branches_page_info.page)
    }

    /// 取得結果を `branches` へ反映する（ページ状態の更新を含む）。新規ナビゲーション
    /// （リポジトリ変更・ページ変更）はここで選択を先頭にリセットする（キャッシュヒットに
    /// よる即時表示（[`App::load_branches_page`]）専用。バックグラウンド再検証結果は
    /// `Msg::BranchesLoaded` 側で選択を保持したまま部分更新する）。
    fn apply_branches(&mut self, branches: Vec<Branch>, page_info: PageInfo) {
        self.branches.set_items(branches);
        self.branches_page_info = page_info;
    }

    fn on_key_branches(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Char('q') => Command::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Command::None
            }
            KeyCode::Esc => {
                self.screen = self.browse_return;
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
            KeyCode::Char('J') => {
                self.branches.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.branches.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.prev_page(),
            KeyCode::Char(']') => self.next_page(),
            KeyCode::Char('g') => self.open_page_jump(),
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
        self.current_commit = None;
        self.commits_page = 1;
        self.commits_next_url = None;
        self.commits_page_cursors = vec![None];
        self.load_commits_current()
    }

    /// 現在ページ（cursor スタック末尾）のコミット履歴取得コマンドを組み立てる。
    ///
    /// [`Self::open_commits`]（先頭ページ）・`[`/`]`（前後移動）・`r`（現在ページ再取得）の
    /// 共通経路。commits は `page` 番号ジャンプ非対応のため、branches のような番号キャッシュは
    /// 持たず、末尾 cursor で毎回 `next` を辿って取得する。
    fn load_commits_current(&mut self) -> Command {
        let Some((client, workspace, repo)) = self.review_context() else {
            self.status = Status::Error("認証クライアントが未初期化です".to_string());
            return Command::None;
        };
        self.commits.set_items(Vec::new());
        let revision = self.commits_revision.clone();
        let cursor = self.commits_page_cursors.last().cloned().flatten();
        let page = self.commits_page;
        self.status = Status::Loading(format!(
            "コミット履歴を取得中…（{} / {page} ページ目）",
            revision.as_deref().unwrap_or("既定ブランチ")
        ));
        Command::LoadCommits {
            client,
            workspace,
            repo,
            revision,
            cursor,
            page,
        }
    }

    /// 現在ページのコミット履歴を再取得する（`r` キー）。
    fn reload_commits(&mut self) -> Command {
        self.load_commits_current()
    }

    /// 次ページへ（`]`）。`next` URL が無ければ何もしない。
    fn commits_next_page(&mut self) -> Command {
        let Some(next) = self.commits_next_url.clone() else {
            return Command::None;
        };
        self.commits_page_cursors.push(Some(next));
        self.commits_page += 1;
        self.load_commits_current()
    }

    /// 前ページへ（`[`）。先頭ページでは何もしない。
    fn commits_prev_page(&mut self) -> Command {
        if self.commits_page <= 1 {
            return Command::None;
        }
        self.commits_page_cursors.pop();
        self.commits_page -= 1;
        self.load_commits_current()
    }

    /// Commits 画面のページャ表示用 [`PageInfo`]。commits は cursor ベースのため総ページ数は
    /// 常に不明（`None`）で、次ページ有無は `next` URL の有無で判定する。
    pub fn commits_page_info(&self) -> PageInfo {
        PageInfo {
            page: self.commits_page,
            total_pages: None,
            has_next: self.commits_next_url.is_some(),
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
            KeyCode::Char('J') => {
                self.commits.select_next_by(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.commits.select_prev_by(10);
                Command::None
            }
            KeyCode::Char('[') => self.commits_prev_page(),
            KeyCode::Char(']') => self.commits_next_page(),
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
            KeyCode::Char('J') => {
                self.commit_scroll = self.commit_scroll.saturating_add(10);
                Command::None
            }
            KeyCode::Char('K') => {
                self.commit_scroll = self.commit_scroll.saturating_sub(10);
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
            KeyCode::Char('J') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_next_by(10);
                }
                Command::None
            }
            KeyCode::Char('K') => {
                if let Some(source) = self.source.as_mut() {
                    source.entries.select_prev_by(10);
                }
                Command::None
            }
            KeyCode::Char('r') => self.reload_source(),
            KeyCode::Enter => self.source_enter(),
            _ => Command::None,
        }
    }

    /// 選択中エントリを開く（ディレクトリなら潜る / ファイルなら FileView）。
    ///
    /// 子パスは `entry.path_str()`（API 由来、フルパス前提だが未検証）をそのまま
    /// 使わず、`source.path`（自己追跡している現在地）+ `entry.name()`（リーフ名）
    /// から [`child_path`] で合成する。API がフルパスを返す通常ケースと同じ結果
    /// になりつつ、リーフ名しか返さないケースでも階層追跡が壊れない。
    fn source_enter(&mut self) -> Command {
        let Some(source) = self.source.as_ref() else {
            return Command::None;
        };
        let Some(entry) = source.entries.selected() else {
            return Command::None;
        };
        let reference = source.reference.clone();
        let path = child_path(&source.path, entry.name());
        if entry.is_dir() {
            self.open_source(reference, path)
        } else {
            let mimetype = entry.mimetype.clone();
            self.open_file(reference, path, mimetype)
        }
    }

    /// 親ディレクトリへ戻る（ルートなら「ブラウズの戻り先」画面へ）。
    fn source_up(&mut self) -> Command {
        let parent = self.source.as_ref().and_then(|source| {
            parent_dir(&source.path).map(|parent| (source.reference.clone(), parent))
        });
        match parent {
            Some((reference, path)) => self.open_source(reference, path),
            None => {
                self.source = None;
                self.screen = self.browse_return;
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
            KeyCode::Char('J') => view.scroll_down(10),
            KeyCode::Char('K') => view.scroll_up(10),
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

    /// PR 詳細キャッシュ（[`App::pr_detail_cache`]）の該当エントリを取得-or-作成して
    /// `update` で部分更新する。キーは (`selected_repo`, `id`)。エントリが無い場合は
    /// `self.current_pr`（呼び出し元のガードで `id` と一致していることが確認済み）を
    /// 土台にして新規作成する。`selected_repo`/`current_pr` のどちらかが無ければ何もしない
    /// （レビュー系操作ができない状態と同じ扱い）。
    fn update_pr_detail_cache<F>(&mut self, id: u64, update: F)
    where
        F: FnOnce(&mut PrDetailCache),
    {
        let Some(repo_full_name) = self.selected_repo.clone() else {
            return;
        };
        let key = (repo_full_name, id);
        if let Some(entry) = self.pr_detail_cache.get_mut(&key) {
            update(entry);
            return;
        }
        let Some(current_pr) = self.current_pr.clone() else {
            return;
        };
        let mut entry = PrDetailCache {
            pr: current_pr,
            diffstat: Vec::new(),
            comments: Vec::new(),
        };
        update(&mut entry);
        self.pr_detail_cache.insert(key, entry);
    }

    /// 現在のリポジトリに紐づく PR 一覧キャッシュを全フィルタ分まとめて無効化する。
    ///
    /// 承認/変更要求/コメント/マージ成功後に呼ぶ。フィルタ別に承認バッジや PR の
    /// 掲載フィルタ（マージで OPEN→MERGED 等）を厳密に差分更新するより、対象 repo の
    /// キャッシュをまとめて削除して次回表示を必ず再取得（キャッシュミス）させる方が単純で
    /// 確実なため、この方式を採る。
    /// 現在の repo のキャッシュを、フィルタ・ソート・ページ番号を問わずすべて無効化する
    /// （承認/変更要求/コメント投稿/マージ後、一覧のバッジ・掲載フィルタが変わり得るため）。
    fn invalidate_pull_requests_cache_for_current_repo(&mut self) {
        let Some(repo) = self.repo_slug() else {
            return;
        };
        self.pull_requests_cache
            .retain(|(cached_repo, _filter, _sort, _page)| cached_repo != &repo);
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

    // ---- インクリメンタル検索（`/`） ----

    /// 現在の画面がインクリメンタル検索に対応するリストを持つ場合、そのフィルタ文字列を返す。
    fn current_filter_text(&self) -> Option<String> {
        match self.screen {
            Screen::Workspaces => Some(self.workspaces.filter.clone()),
            Screen::Repositories => Some(self.repositories.filter.clone()),
            Screen::PullRequests => Some(self.pull_requests.filter.clone()),
            _ => None,
        }
    }

    /// 現在の画面のリストへフィルタ文字列を適用する（検索対象文字列は型ごとに決める）。
    fn set_current_list_filter(&mut self, filter: String) {
        match self.screen {
            Screen::Workspaces => {
                self.workspaces.set_filter(filter, |workspace: &Workspace| {
                    format!("{} {}", workspace.display_name(), workspace.slug)
                });
            }
            Screen::Repositories => {
                self.repositories.set_filter(filter, |repo: &Repository| {
                    format!("{} {}", repo.full_name, repo.name)
                });
            }
            Screen::PullRequests => {
                self.pull_requests.set_filter(filter, |pr: &PullRequest| {
                    format!("{} #{}", pr.title_str(), pr.id)
                });
            }
            _ => {}
        }
    }

    fn on_key_search_editing(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.search_editing = false;
                self.set_current_list_filter(String::new());
                Command::None
            }
            KeyCode::Enter => {
                // フィルタは維持したまま、通常のリスト操作へ戻る。
                self.search_editing = false;
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(mut filter) = self.current_filter_text() {
                    filter.pop();
                    self.set_current_list_filter(filter);
                }
                Command::None
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(mut filter) = self.current_filter_text() {
                    filter.push(ch);
                    self.set_current_list_filter(filter);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    // ---- ジャンプパレット（`Ctrl+K`） ----

    /// ジャンプパレットを開く（保持済みデータからの候補一覧を組み立てる）。
    fn open_jump_palette(&mut self) -> Command {
        let mut entries = SelectList::default();
        entries.set_items(self.build_jump_entries());
        self.jump_palette = Some(JumpPaletteState { entries });
        Command::None
    }

    /// ジャンプパレットの候補を、保持済みデータ（ワークスペース/リポジトリ/直近開いた PR）と
    /// 画面ジャンプから組み立てる。ワークスペース/リポジトリは現在保持している順（画面の
    /// 一覧と同じ順序）、PR は開いた順（新しい順）で並べる。
    fn build_jump_entries(&self) -> Vec<JumpEntry> {
        let mut entries = Vec::new();

        entries.push(JumpEntry {
            label: "ワークスペース一覧へ".to_string(),
            search_key: "workspaces ワークスペース一覧".to_string(),
            action: JumpAction::Screen(Screen::Workspaces),
        });
        if self.selected_workspace.is_some() {
            entries.push(JumpEntry {
                label: "リポジトリ一覧へ（現在のワークスペース）".to_string(),
                search_key: "repositories リポジトリ一覧".to_string(),
                action: JumpAction::Screen(Screen::Repositories),
            });
        }
        if self.selected_repo.is_some() {
            entries.push(JumpEntry {
                label: "PR 一覧へ（現在のリポジトリ）".to_string(),
                search_key: "pull requests PR プルリクエスト一覧".to_string(),
                action: JumpAction::Screen(Screen::PullRequests),
            });
        }

        for workspace in self.workspaces.items.iter().cloned() {
            entries.push(JumpEntry {
                label: format!("WS: {} ({})", workspace.display_name(), workspace.slug),
                search_key: format!("{} {}", workspace.display_name(), workspace.slug),
                action: JumpAction::Workspace(workspace.slug.clone()),
            });
        }

        for repo in self.repositories.items.iter().cloned() {
            entries.push(JumpEntry {
                label: format!("Repo: {}", repo.full_name),
                search_key: format!("{} {}", repo.full_name, repo.name),
                action: JumpAction::Repository(Box::new(repo)),
            });
        }

        for recent in &self.recent_prs {
            entries.push(JumpEntry {
                label: format!(
                    "PR #{} {} ({})",
                    recent.pr.id,
                    recent.pr.title_str(),
                    recent.repo_full_name
                ),
                search_key: format!(
                    "{} #{} {}",
                    recent.pr.title_str(),
                    recent.pr.id,
                    recent.repo_full_name
                ),
                action: JumpAction::RecentPr(Box::new(recent.clone())),
            });
        }

        entries
    }

    fn on_key_jump_palette(&mut self, key: KeyEvent) -> Command {
        match key.code {
            KeyCode::Esc => {
                self.jump_palette = None;
                Command::None
            }
            KeyCode::Down => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    palette.entries.select_next();
                }
                Command::None
            }
            KeyCode::Up => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    palette.entries.select_prev();
                }
                Command::None
            }
            KeyCode::Backspace => {
                if let Some(palette) = self.jump_palette.as_mut() {
                    let mut filter = palette.entries.filter.clone();
                    filter.pop();
                    palette.entries.set_filter(filter, jump_entry_key);
                }
                Command::None
            }
            KeyCode::Enter => self.confirm_jump_palette(),
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(palette) = self.jump_palette.as_mut() {
                    let mut filter = palette.entries.filter.clone();
                    filter.push(ch);
                    palette.entries.set_filter(filter, jump_entry_key);
                }
                Command::None
            }
            _ => Command::None,
        }
    }

    /// 選択中の候補で遷移を実行する。
    fn confirm_jump_palette(&mut self) -> Command {
        let Some(palette) = self.jump_palette.as_ref() else {
            return Command::None;
        };
        let Some(entry) = palette.entries.selected().cloned() else {
            return Command::None;
        };
        self.jump_palette = None;
        match entry.action {
            JumpAction::Screen(screen) => {
                self.screen = screen;
                Command::None
            }
            JumpAction::Workspace(slug) => self.jump_to_workspace(slug),
            JumpAction::Repository(repo) => self.enter_repository(*repo),
            JumpAction::RecentPr(recent) => {
                let recent = *recent;
                self.jump_to_pr(recent.workspace, recent.repo_full_name, recent.pr)
            }
        }
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

/// 現在のディレクトリ `parent`（TUI 自身が追跡する信頼できる状態）と、選択エントリの
/// リーフ名 `leaf`（[`SrcEntry::name`]）から子パスを合成する。
///
/// `SrcEntry::path`（[`SrcEntry::path_str`]）はリポジトリルートからのフルパスで
/// 返る前提だが、これは実 API で未検証の仮定（`docs/LEDGER.md` 参照）。この関数を
/// 使うことで、API がフルパスを正しく返す場合はもちろん、リーフ名しか返さない
/// 場合でも `source_up`/`parent_dir` が前提とする「`source.path` は常にルートから
/// のフルパス」を壊さずに階層移動できる（[`App::source_enter`] 参照）。
fn child_path(parent: &str, leaf: &str) -> String {
    if parent.is_empty() {
        leaf.to_string()
    } else {
        format!("{parent}/{leaf}")
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

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn app() -> App {
        App::new(Config::default(), None)
    }

    fn client() -> BitbucketClient {
        BitbucketClient::new("me@example.com".to_string(), "token".to_string())
            .expect("client builds")
    }

    /// テスト用の `PageInfo`（総ページ数・次ページ有無を明示指定）。
    fn page_info(page: u32, total_pages: Option<u32>, has_next: bool) -> PageInfo {
        PageInfo {
            page,
            total_pages,
            has_next,
        }
    }

    /// 1 ページに収まる小さな一覧を想定した `PageInfo`（1 ページ目・総 1 ページ・次ページ無し）。
    fn single_page() -> PageInfo {
        page_info(1, Some(1), false)
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

    fn make_diffstat_entry(path: &str) -> DiffStatEntry {
        let json = format!(
            r#"{{ "status": "modified", "lines_added": 1, "lines_removed": 0,
                  "new": {{ "path": "{path}" }} }}"#
        );
        serde_json::from_str(&json).expect("valid diffstat entry json")
    }

    fn make_comment(id: u64, raw: &str) -> Comment {
        let json = format!(
            r#"{{ "id": {id}, "content": {{ "raw": "{raw}" }},
                  "user": {{ "display_name": "Alice" }}, "deleted": false }}"#
        );
        serde_json::from_str(&json).expect("valid comment json")
    }

    /// inline アンカー（新側 `to`）付きのコメント。`parent` を渡すと返信になる。
    fn make_inline_comment(
        id: u64,
        raw: &str,
        path: &str,
        to: u64,
        created: &str,
        parent: Option<u64>,
    ) -> Comment {
        let parent_json = match parent {
            Some(pid) => format!(r#", "parent": {{ "id": {pid} }}"#),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "id": {id}, "content": {{ "raw": "{raw}" }},
                  "user": {{ "display_name": "Alice" }}, "deleted": false,
                  "created_on": "{created}",
                  "inline": {{ "path": "{path}", "to": {to} }}{parent_json} }}"#
        );
        serde_json::from_str(&json).expect("valid inline comment json")
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
    fn ctrl_t_cycles_theme_and_persists_name() {
        let mut app = app();
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha);
        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha.next());
        assert_eq!(app.theme, app.theme_name.theme());
        assert_eq!(app.config.theme.as_deref(), Some(app.theme_name.as_str()));
    }

    #[test]
    fn ctrl_t_cycles_theme_globally_even_while_help_is_shown() {
        let mut app = app();
        app.show_help = true;
        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));
        assert_eq!(app.theme_name, ThemeName::CatppuccinMocha.next());
        // グローバルキーとして扱うため、ヘルプの開閉状態には影響しない。
        assert!(app.show_help);
    }

    #[test]
    fn ctrl_t_invalidates_diff_rendered_line_cache() {
        let mut app = app();
        app.diff = Some(DiffState {
            parsed: parse_diff(" context\n"),
            scroll: 0,
            viewport: 0,
            title: "#1".to_string(),
            rendered_lines: Some(Vec::new()),
            rendered_split: Some(Vec::new()),
            file_index: 0,
            cursor: 0,
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        });

        app.update(Msg::Key(ctrl(KeyCode::Char('t'))));

        let diff = app.diff.as_ref().expect("diff は保持されたまま");
        assert!(
            diff.rendered_lines.is_none(),
            "テーマ切替後は着色済み行キャッシュを無効化するべき"
        );
        assert!(
            diff.rendered_split.is_none(),
            "テーマ切替後は split 表示の着色済み行キャッシュも無効化するべき"
        );
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
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
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
            ],
            page_info: single_page(),
        });
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
            sort: ListSort::RecentlyUpdated,
            workspace: "stale".to_string(),
            repos: vec![make_repo("x/y", None)],
            page_info: single_page(),
        });
        assert!(app.repositories.items.is_empty());
    }

    #[test]
    fn selecting_repository_loads_pull_requests() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/widget", None)],
            page_info: single_page(),
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
                assert_eq!(filter, PrStateFilter::only(PrState::Open));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_loaded_sets_items_when_fresh() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 2);
        assert_eq!(app.pull_requests.state.selected(), Some(0));
    }

    #[test]
    fn pull_requests_loaded_ignored_for_stale_filter() {
        let mut app = review_app();
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Merged),
            prs: vec![make_pr(1, "MERGED")],
            page_info: single_page(),
        });
        assert!(app.pull_requests.items.is_empty());
    }

    #[test]
    fn filter_key_reloads_with_new_state() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert_eq!(app.pr_state_filter, PrStateFilter::only(PrState::Merged));
        match cmd {
            Command::LoadPullRequests { filter, .. } => {
                assert_eq!(filter, PrStateFilter::only(PrState::Merged))
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    // ---- PR フィルタ（M7 Phase 2） ----

    fn author(name: &str, uuid: &str) -> PrAuthor {
        PrAuthor {
            uuid: uuid.to_string(),
            display_name: name.to_string(),
        }
    }

    /// author（表示名 + 任意の uuid）付きの PR。
    fn make_pr_with_author(id: u64, state: &str, name: &str, uuid: Option<&str>) -> PullRequest {
        let uuid_json = match uuid {
            Some(uuid) => format!(r#", "uuid": "{uuid}""#),
            None => String::new(),
        };
        let json = format!(
            r#"{{ "id": {id}, "title": "PR {id}", "state": "{state}",
                  "author": {{ "display_name": "{name}"{uuid_json} }},
                  "participants": [] }}"#
        );
        serde_json::from_str(&json).expect("valid pr json")
    }

    #[test]
    fn single_state_keys_map_to_state_sets_and_preserve_author() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));

        app.update(Msg::Key(key(KeyCode::Char('o'))));
        assert_eq!(app.pr_state_filter.states, BTreeSet::from([PrState::Open]));
        app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Merged])
        );
        app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Declined])
        );
        let cmd = app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert_eq!(app.pr_state_filter.states, BTreeSet::from(PrState::ALL));
        // author は state キーとは独立の軸なので維持される（解除はフィルタモーダルで行う）。
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        // 既存の体感どおり 1 ページ目から再取得する。
        match cmd {
            Command::LoadPullRequests { page, filter, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.states, BTreeSet::from(PrState::ALL));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn state_filter_change_persists_states_to_config_but_not_author() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert_eq!(app.config.pr_states, Some(vec!["MERGED".to_string()]));
    }

    #[test]
    fn pr_state_filter_from_config_ignores_invalid_values_and_defaults_to_open() {
        assert_eq!(PrStateFilter::from_config(None), PrStateFilter::default());

        let mixed = vec![
            "MERGED".to_string(),
            "bogus".to_string(),
            "DECLINED".to_string(),
        ];
        let filter = PrStateFilter::from_config(Some(&mixed));
        assert_eq!(
            filter.states,
            BTreeSet::from([PrState::Merged, PrState::Declined])
        );
        assert!(filter.author.is_none());

        let invalid = vec!["bogus".to_string()];
        assert_eq!(
            PrStateFilter::from_config(Some(&invalid)),
            PrStateFilter::default()
        );
    }

    #[test]
    fn app_new_restores_pr_states_from_config() {
        let config = Config {
            pr_states: Some(vec!["MERGED".to_string(), "bogus".to_string()]),
            ..Config::default()
        };
        let app = App::new(config, None);
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Merged])
        );
        assert!(app.pr_state_filter.author.is_none());
    }

    /// target branch フィルタのテストヘルパ。
    fn target(text: &str, exact: bool) -> TargetBranch {
        TargetBranch {
            text: text.to_string(),
            exact,
        }
    }

    /// states + author のみの `PrStateFilter`（target 無し）のテストヘルパ。
    fn filter_with_author(states: BTreeSet<PrState>, author: PrAuthor) -> PrStateFilter {
        PrStateFilter {
            states,
            author: Some(author),
            target_branch: None,
        }
    }

    /// モーダルを開いたときのコマンド（`Batch`）を展開するテストヘルパ。
    fn unbatch(cmd: Command) -> Vec<Command> {
        match cmd {
            Command::Batch(cmds) => cmds,
            Command::None => Vec::new(),
            other => vec![other],
        }
    }

    #[test]
    fn pr_state_filter_label_formats_states_author_and_target() {
        assert_eq!(PrStateFilter::only(PrState::Open).label(), "OPEN");
        assert_eq!(PrStateFilter::all().label(), "ALL");
        let filter = filter_with_author(
            BTreeSet::from([PrState::Open, PrState::Merged]),
            author("Alice", "{u-1}"),
        );
        assert_eq!(filter.label(), "OPEN+MERGED, author: Alice");
        // 部分一致は `target~"..."`、完全一致は `target: ...`。
        let partial = PrStateFilter {
            target_branch: Some(target("release", false)),
            ..PrStateFilter::only(PrState::Open)
        };
        assert_eq!(partial.label(), r#"OPEN, target~"release""#);
        let exact = PrStateFilter {
            author: Some(author("Alice", "{u-1}")),
            target_branch: Some(target("main", true)),
            ..PrStateFilter::only(PrState::Open)
        };
        assert_eq!(exact.label(), "OPEN, author: Alice, target: main");
    }

    #[test]
    fn f_opens_pr_filter_modal_and_caches_authors_and_branches_per_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;

        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        let modal = app.pr_filter_modal.as_ref().expect("modal opens");
        assert!(modal.authors.is_none(), "取得完了までは読み込み中");
        assert!(modal.branches.is_none(), "取得完了までは読み込み中");
        // author 候補（PR 集約）とブランチ候補の両方を遅延取得する。
        let cmds = unbatch(cmd);
        assert_eq!(cmds.len(), 2);
        match &cmds[0] {
            Command::LoadPrAuthors {
                workspace,
                repo,
                repo_full_name,
                ..
            } => {
                assert_eq!(workspace, "acme");
                assert_eq!(repo, "widget");
                assert_eq!(repo_full_name, "acme/widget");
            }
            other => panic!("expected LoadPrAuthors, got {other:?}"),
        }
        match &cmds[1] {
            Command::LoadFilterBranches {
                workspace,
                repo,
                repo_full_name,
                ..
            } => {
                assert_eq!(workspace, "acme");
                assert_eq!(repo, "widget");
                assert_eq!(repo_full_name, "acme/widget");
            }
            other => panic!("expected LoadFilterBranches, got {other:?}"),
        }

        // author 取得成功 → PR 集約が uuid 重複排除・表示名順（大文字小文字は無視）で候補に
        // なり、repo 単位でキャッシュ。
        app.update(Msg::PrAuthorsLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Ok(vec![
                make_pr_with_author(1, "OPEN", "bob", Some("{u-2}")),
                make_pr_with_author(2, "MERGED", "Alice", Some("{u-1}")),
                make_pr_with_author(3, "OPEN", "bob", Some("{u-2}")), // 重複 uuid → 1 件に。
                make_pr_with_author(4, "OPEN", "Ghost", None),        // uuid 無し → 除外。
            ]),
        });
        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(
            modal.authors,
            Some(vec![author("Alice", "{u-1}"), author("bob", "{u-2}")])
        );

        // ブランチ取得成功 → 名前一覧が候補になり、repo 単位でキャッシュ。
        app.update(Msg::FilterBranchesLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Ok(vec![
                make_branch("main", "abc123"),
                make_branch("develop", "def456"),
            ]),
        });
        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(
            modal.branches,
            Some(vec!["main".to_string(), "develop".to_string()])
        );

        // 閉じて開き直すとキャッシュ命中で即表示（再取得コマンドを発行しない）。
        app.update(Msg::Key(key(KeyCode::Esc)));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(matches!(cmd, Command::None));
        let modal = app.pr_filter_modal.as_ref().expect("modal reopens");
        assert!(modal.authors.is_some());
        assert!(modal.branches.is_some());
    }

    #[test]
    fn pr_filter_modal_space_toggles_and_rejects_empty_states_on_enter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('f'))));

        // カーソル先頭（OPEN）をトグルして空集合にする → Enter は拒否（Status エラー）。
        app.update(Msg::Key(key(KeyCode::Char(' '))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(matches!(cmd, Command::None));
        assert!(matches!(app.status, Status::Error(_)));
        assert!(
            app.pr_filter_modal.is_some(),
            "空集合では適用されずモーダルは開いたまま"
        );
        assert_eq!(app.pr_state_filter, PrStateFilter::only(PrState::Open));

        // OPEN を戻し、MERGED も足して適用 → 1 ページ目から再取得。
        app.update(Msg::Key(key(KeyCode::Char(' '))));
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        app.update(Msg::Key(key(KeyCode::Char(' '))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Open, PrState::Merged])
        );
        assert_eq!(
            app.config.pr_states,
            Some(vec!["OPEN".to_string(), "MERGED".to_string()])
        );
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(
                    filter.states,
                    BTreeSet::from([PrState::Open, PrState::Merged])
                );
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pr_filter_modal_tab_moves_to_author_section_and_enter_applies_author() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![author("Alice", "{u-1}"), author("Bob", "{u-2}")],
        );
        app.branch_candidates_cache
            .insert("acme/widget".to_string(), vec!["main".to_string()]);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(
            matches!(cmd, Command::None),
            "キャッシュ命中時は再取得しない"
        );

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(
            app.pr_filter_modal.as_ref().map(|modal| modal.section),
            Some(PrFilterSection::Author)
        );
        // Author セクションでは `j` は検索文字（8.4）なので、移動は ↓ で行う。
        app.update(Msg::Key(key(KeyCode::Down))); // All authors → Alice
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        assert_eq!(app.pr_state_filter.states, BTreeSet::from([PrState::Open]));
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.author_uuid(), Some("{u-1}"));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
        // author は config へ保存しない（states のみ）。
        assert_eq!(app.config.pr_states, Some(vec!["OPEN".to_string()]));
    }

    #[test]
    fn pr_filter_modal_esc_cancels_without_applying() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Char(' ')))); // OPEN を外す。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        app.update(Msg::Key(key(KeyCode::Char(' ')))); // MERGED を付ける。
        let cmd = app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(matches!(cmd, Command::None));
        assert!(app.pr_filter_modal.is_none());
        // 作業コピーは破棄され、現在のフィルタは変わらない。
        assert_eq!(app.pr_state_filter, PrStateFilter::only(PrState::Open));
    }

    #[test]
    fn pr_authors_fetch_failure_falls_back_to_loaded_pr_authors_and_does_not_cache() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![
            make_pr_with_author(1, "OPEN", "Alice", Some("{u-1}")),
            make_pr_with_author(2, "OPEN", "Alice", Some("{u-1}")), // 重複 uuid → 1 件に。
            make_pr_with_author(3, "OPEN", "Ghost", None),          // uuid 無し → 除外。
        ]);
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::PrAuthorsLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Err(ApiError::Auth),
        });

        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(modal.authors, Some(vec![author("Alice", "{u-1}")]));

        // 失敗はキャッシュしない → 開き直すと再取得を試みる。
        app.update(Msg::Key(key(KeyCode::Esc)));
        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(
            unbatch(cmd)
                .iter()
                .any(|cmd| matches!(cmd, Command::LoadPrAuthors { .. }))
        );
    }

    #[test]
    fn branch_fetch_failure_leaves_no_candidates_but_partial_match_still_works() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::FilterBranchesLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Err(ApiError::Auth),
        });

        // 失敗 → 候補なし（空の Some）で確定し、キャッシュには入れない。
        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(modal.branches, Some(Vec::new()));
        assert!(
            app.branch_candidates_cache
                .get(&"acme/widget".to_string())
                .is_none()
        );

        // 候補が無くても自由入力の部分一致は適用できる。
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        for c in "rel".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("rel", false))
        );

        // 失敗はキャッシュしない → 開き直すとブランチ候補の再取得を試みる。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(
            unbatch(cmd)
                .iter()
                .any(|cmd| matches!(cmd, Command::LoadFilterBranches { .. }))
        );
    }

    #[test]
    fn pr_filter_modal_enter_while_authors_loading_keeps_current_author() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));

        let cmd = app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert!(
            unbatch(cmd)
                .iter()
                .any(|cmd| matches!(cmd, Command::LoadPrAuthors { .. }))
        );
        // 候補未着（読み込み中）のまま MERGED を足して Enter。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        app.update(Msg::Key(key(KeyCode::Char(' '))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        // author は黙って All authors へ落とさず維持し、states だけを適用する。
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Open, PrState::Merged])
        );
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.author_uuid(), Some("{u-1}"));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    /// 8.4: Author セクションでは印字文字（`j` 含む）が検索クエリになり、fuzzy（nucleo）で
    /// 候補を絞り込む。カーソルは先頭マッチへ戻り、Enter はマッチ行を適用する。
    #[test]
    fn pr_filter_modal_author_query_filters_candidates_and_treats_j_as_input() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![
                author("Alice", "{u-1}"),
                author("Bob", "{u-2}"),
                author("Jane", "{u-3}"),
            ],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));

        // `j` は移動ではなく検索文字として扱われる。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        {
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.author_query, "j");
            assert_eq!(modal.author_matches, vec![2], "Jane だけがマッチ");
            assert_eq!(modal.author_cursor, 0, "カーソルは先頭マッチへ");
        }

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(app.pr_state_filter.author, Some(author("Jane", "{u-3}")));
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.author_uuid(), Some("{u-3}"));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    /// 8.4: クエリはセクション移動（Tab）では保持し、Backspace で 1 文字ずつ削除できる。
    /// モーダルを閉じたら破棄される（開き直すと空）。
    #[test]
    fn pr_filter_modal_author_query_survives_tab_and_is_discarded_on_close() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![
                author("Alice", "{u-1}"),
                author("Bob", "{u-2}"),
                author("Jane", "{u-3}"),
            ],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));

        // Tab でセクションを一巡（Author → Target → States → Author）してもクエリは
        // 保持される。
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.author_query.as_str()),
            Some("ja")
        );

        // Backspace で削除。空になったら恒等写像（All authors + 全候補）へ戻る。
        app.update(Msg::Key(key(KeyCode::Backspace)));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        {
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.author_query, "");
            assert_eq!(modal.author_matches, vec![0, 1, 2]);
            assert_eq!(modal.author_cursor, 0, "先頭行（All authors）へ戻る");
        }

        // 閉じたら破棄（開き直すと空クエリ）。
        app.update(Msg::Key(key(KeyCode::Esc)));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.author_query.as_str()),
            Some("")
        );
    }

    /// 8.4: 候補 0 件（該当なし）で Enter しても現用 author を維持する（All authors へ
    /// 黙って落とさない。読み込み中 Enter と同じ方針）。
    #[test]
    fn pr_filter_modal_author_query_no_match_enter_keeps_current_author() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![author("Alice", "{u-1}"), author("Bob", "{u-2}")],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.author_matches.len()),
            Some(0)
        );

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.author_uuid(), Some("{u-1}"));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    /// 8.4: クエリ絞り込み中の候補移動は `↑↓`（マッチ行の範囲でクランプ）。Enter は
    /// カーソル位置のマッチ行を適用する。
    #[test]
    fn pr_filter_modal_author_query_arrows_move_within_matches() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![
                author("Alice", "{u-1}"),
                author("Bob", "{u-2}"),
                author("Bobby", "{u-3}"),
            ],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        for c in "bob".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        // Bob と Bobby の 2 件（スコア順は実装依存のため、期待値はモーダル状態から導く）。
        let expected = {
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.author_matches.len(), 2);
            let authors = modal.authors.as_ref().expect("authors loaded");
            authors[modal.author_matches[1]].clone()
        };

        app.update(Msg::Key(key(KeyCode::Down)));
        // 末尾でクランプ（さらに ↓ しても動かない）。
        app.update(Msg::Key(key(KeyCode::Down)));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.author_cursor),
            Some(1)
        );
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.pr_state_filter.author, Some(expected));
    }

    #[test]
    fn current_author_missing_from_members_is_inserted_with_cursor_on_it() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::PrAuthorsLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Ok(vec![
                make_pr_with_author(1, "OPEN", "bob", Some("{u-2}")),
                make_pr_with_author(2, "OPEN", "Zoe", Some("{u-3}")),
            ]),
        });

        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        // 直近 PR に登場しない等で候補に無い現用 author は表示名順（大文字小文字無視）の
        // 位置へ挿入される。
        assert_eq!(
            modal.authors,
            Some(vec![
                author("Alice", "{u-1}"),
                author("bob", "{u-2}"),
                author("Zoe", "{u-3}"),
            ])
        );
        // カーソルは現用 author を指す（0 = All authors、1 = Alice）。
        assert_eq!(modal.author_cursor, 1);
        // 挿入はモーダルの作業コピーのみで、repo 単位キャッシュへは波及しない。
        assert_eq!(
            app.pr_authors_cache.get(&"acme/widget".to_string()),
            Some(&vec![author("bob", "{u-2}"), author("Zoe", "{u-3}")])
        );
    }

    #[test]
    fn current_author_missing_from_cached_members_is_inserted_on_open() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache
            .insert("acme/widget".to_string(), vec![author("Bob", "{u-2}")]);
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));

        app.update(Msg::Key(key(KeyCode::Char('f'))));
        let modal = app.pr_filter_modal.as_ref().expect("modal opens");
        assert_eq!(
            modal.authors,
            Some(vec![author("Alice", "{u-1}"), author("Bob", "{u-2}")])
        );
        assert_eq!(modal.author_cursor, 1);
    }

    #[test]
    fn members_fallback_aggregates_authors_across_cached_filters_and_pages() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        // author フィルタ適用中: 表示中の一覧は Alice の PR だけに痩せている。
        let with_author =
            filter_with_author(BTreeSet::from([PrState::Open]), author("Alice", "{u-1}"));
        app.pr_state_filter = with_author.clone();
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: with_author,
            prs: vec![make_pr_with_author(1, "OPEN", "Alice", Some("{u-1}"))],
            page_info: single_page(),
        });
        // 現在 repo の別フィルタ・別ページのキャッシュには別著者の PR が残っている。
        app.pull_requests_cache.insert(
            (
                "widget".to_string(),
                PrStateFilter::only(PrState::Merged),
                ListSort::RecentlyUpdated,
                2,
            ),
            (
                vec![make_pr_with_author(2, "MERGED", "bob", Some("{u-2}"))],
                page_info(2, Some(2), false),
            ),
        );
        // 他 repo のキャッシュは候補に混ぜない。
        app.pull_requests_cache.insert(
            (
                "gadget".to_string(),
                PrStateFilter::only(PrState::Open),
                ListSort::RecentlyUpdated,
                1,
            ),
            (
                vec![make_pr_with_author(3, "OPEN", "Carol", Some("{u-3}"))],
                single_page(),
            ),
        );

        // author 候補ソース取得失敗 → フォールバック候補にキャッシュ上の他著者も出る。
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::PrAuthorsLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Err(ApiError::Auth),
        });
        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(
            modal.authors,
            Some(vec![author("Alice", "{u-1}"), author("bob", "{u-2}")])
        );
    }

    #[test]
    fn pull_requests_cache_is_keyed_by_author_and_does_not_leak_across_authors() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 1);

        // author 付きフィルタへ切替: author 無しのキャッシュに命中しないこと。
        let with_author =
            filter_with_author(BTreeSet::from([PrState::Open]), author("Alice", "{u-1}"));
        app.pr_state_filter = with_author.clone();
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: with_author,
            prs: vec![make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        // author 無しへ戻すと元のキャッシュに命中する（別エントリとして共存している）。
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(app.status, Status::Idle);
        assert_eq!(app.pull_requests.items[0].id, 1);
    }

    // ---- Target branch フィルタ（M8 8.6） ----

    #[test]
    fn pull_requests_cache_is_keyed_by_target_branch_and_does_not_leak_across_targets() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 1);

        // target 付きフィルタへ切替: target 無しのキャッシュに命中しないこと。
        let with_target = PrStateFilter {
            target_branch: Some(target("release", false)),
            ..PrStateFilter::only(PrState::Open)
        };
        app.pr_state_filter = with_target.clone();
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: with_target,
            prs: vec![make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        // exact 違い（部分一致 ⇔ 完全一致）も別エントリ。
        app.pr_state_filter = PrStateFilter {
            target_branch: Some(target("release", true)),
            ..PrStateFilter::only(PrState::Open)
        };
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        // target 無しへ戻すと元のキャッシュに命中する（別エントリとして共存している）。
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(app.status, Status::Idle);
        assert_eq!(app.pull_requests.items[0].id, 1);
    }

    #[test]
    fn pr_filter_modal_tab_cycles_three_sections_and_backtab_reverses() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        let section = |app: &App| app.pr_filter_modal.as_ref().map(|modal| modal.section);

        assert_eq!(section(&app), Some(PrFilterSection::States));
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(section(&app), Some(PrFilterSection::Author));
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(section(&app), Some(PrFilterSection::Target));
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(section(&app), Some(PrFilterSection::States));
        app.update(Msg::Key(key(KeyCode::BackTab)));
        assert_eq!(section(&app), Some(PrFilterSection::Target));
        app.update(Msg::Key(key(KeyCode::BackTab)));
        assert_eq!(section(&app), Some(PrFilterSection::Author));
    }

    #[test]
    fn pr_filter_modal_target_partial_row_applies_non_exact_match() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache
            .insert("acme/widget".to_string(), Vec::new());
        app.branch_candidates_cache.insert(
            "acme/widget".to_string(),
            vec!["main".to_string(), "release/1.0".to_string()],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));

        // `j` も検索文字として扱う（Author セクションと同じ操作系）。
        for c in "rel".chars() {
            app.update(Msg::Key(key(KeyCode::Char(c))));
        }
        {
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.target_query, "rel");
            assert_eq!(modal.target_matches, vec![1], "release/1.0 だけがマッチ");
            assert_eq!(modal.target_cursor, 1, "カーソルは部分一致行へ");
        }

        // カーソル位置（部分一致行）のまま Enter → exact=false で適用。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("rel", false))
        );
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.target_branch, Some(target("rel", false)));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pr_filter_modal_target_candidate_selection_applies_exact_match() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_authors_cache
            .insert("acme/widget".to_string(), Vec::new());
        app.branch_candidates_cache.insert(
            "acme/widget".to_string(),
            vec!["main".to_string(), "develop".to_string()],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));

        // クエリ空: 行 0 = All branches、行 1 = main、行 2 = develop。
        app.update(Msg::Key(key(KeyCode::Down)));
        app.update(Msg::Key(key(KeyCode::Down)));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("develop", true))
        );
        match cmd {
            Command::LoadPullRequests { filter, page, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter.target_branch, Some(target("develop", true)));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pr_filter_modal_target_all_branches_clears_filter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.target_branch = Some(target("main", true));
        app.pr_authors_cache
            .insert("acme/widget".to_string(), Vec::new());
        app.branch_candidates_cache.insert(
            "acme/widget".to_string(),
            vec!["main".to_string(), "develop".to_string()],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        {
            // 開いた時点でカーソルは現用の完全一致 target（行 1 = main）を指す。
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.target_cursor, 1);
        }
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Up))); // main → All branches
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert!(app.pr_state_filter.target_branch.is_none());
    }

    #[test]
    fn pr_filter_modal_enter_while_branches_loading_keeps_current_target() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.target_branch = Some(target("main", true));

        // 候補未着（読み込み中）のまま Enter → target を黙って解除しない。
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("main", true))
        );
        match cmd {
            Command::LoadPullRequests { filter, .. } => {
                assert_eq!(filter.target_branch, Some(target("main", true)));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pr_filter_modal_restores_partial_target_into_query_and_keeps_it_on_enter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.target_branch = Some(target("rel", false));
        app.pr_authors_cache
            .insert("acme/widget".to_string(), Vec::new());
        app.branch_candidates_cache
            .insert("acme/widget".to_string(), vec!["release/1.0".to_string()]);

        app.update(Msg::Key(key(KeyCode::Char('f'))));
        {
            // 部分一致フィルタは検索クエリへ復元され、カーソルは部分一致行を指す。
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert_eq!(modal.target_query, "rel");
            assert_eq!(modal.target_cursor, 1);
        }
        // 未操作のまま Enter → 同じ部分一致フィルタを維持する。
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("rel", false))
        );
    }

    /// M8 レビュー修正 F2: 検索クエリを入力して全消去したとき、カーソルは All 行ではなく
    /// 現用フィルタ値の行へ戻る（1 文字入力 → Backspace → Enter で適用中の author / target が
    /// 黙って解除されない）。
    #[test]
    fn pr_filter_modal_clearing_query_returns_cursor_to_current_filter_row() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.pr_state_filter.target_branch = Some(target("main", true));
        app.pr_authors_cache.insert(
            "acme/widget".to_string(),
            vec![author("Alice", "{u-1}"), author("Bob", "{u-2}")],
        );
        app.branch_candidates_cache.insert(
            "acme/widget".to_string(),
            vec!["main".to_string(), "develop".to_string()],
        );
        app.update(Msg::Key(key(KeyCode::Char('f'))));

        // Author: 1 文字入力 → 全消去でカーソルは現用 author（行 1 = Alice）へ戻る。
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.author_cursor),
            Some(1)
        );

        // Target: 同様に 1 文字入力 → 全消去で現用 target（行 1 = main）へ戻る。
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Char('d'))));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(
            app.pr_filter_modal
                .as_ref()
                .map(|modal| modal.target_cursor),
            Some(1)
        );

        // そのまま Enter しても現在のフィルタが維持される。
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_filter_modal.is_none());
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("main", true))
        );
    }

    /// M8 レビュー修正 F5: ブランチ候補の応答到着がユーザーのカーソル選択を上書きしない
    /// （読み込み中に All branches を選んでいたら到着後も All のまま。無条件に部分一致行へ
    /// 戻さない）。
    #[test]
    fn branch_arrival_preserves_cursor_selection_made_while_loading() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        // 部分一致フィルタ適用中に開く → クエリ復元・カーソルは部分一致行（1）。候補は未着。
        app.pr_state_filter.target_branch = Some(target("rel", false));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        app.update(Msg::Key(key(KeyCode::Tab)));
        {
            let modal = app.pr_filter_modal.as_ref().expect("modal open");
            assert!(modal.branches.is_none(), "候補は読み込み中");
            assert_eq!(modal.target_cursor, 1);
        }
        // 読み込み中に ↑ で All branches（行 0）を選ぶ。
        app.update(Msg::Key(key(KeyCode::Up)));

        // 応答到着 → 絞り込みは再計算されるが、カーソルは All のまま。
        app.update(Msg::FilterBranchesLoaded {
            repo_full_name: "acme/widget".to_string(),
            result: Ok(vec![make_branch("release/1.0", "abc123")]),
        });
        let modal = app.pr_filter_modal.as_ref().expect("modal stays open");
        assert_eq!(
            modal.target_matches,
            vec![0],
            "候補の絞り込みは再計算される"
        );
        assert_eq!(modal.target_cursor, 0, "All branches の選択が維持される");

        // Enter → 見えている選択のとおり All branches（解除）が適用される。
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.pr_state_filter.target_branch.is_none());
    }

    #[test]
    fn current_exact_target_missing_from_candidates_is_inserted_on_open() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter.target_branch = Some(target("hotfix", true));
        app.pr_authors_cache
            .insert("acme/widget".to_string(), Vec::new());
        app.branch_candidates_cache
            .insert("acme/widget".to_string(), vec!["main".to_string()]);

        app.update(Msg::Key(key(KeyCode::Char('f'))));
        let modal = app.pr_filter_modal.as_ref().expect("modal open");
        // 候補（1 ページ目）から漏れた現用ブランチは先頭へ挿入され、カーソルが指す。
        assert_eq!(
            modal.branches,
            Some(vec!["hotfix".to_string(), "main".to_string()])
        );
        assert_eq!(modal.target_cursor, 1);
        // 挿入はモーダルの作業コピーのみで、repo 単位キャッシュへは波及しない。
        assert_eq!(
            app.branch_candidates_cache.get(&"acme/widget".to_string()),
            Some(&vec!["main".to_string()])
        );
    }

    #[test]
    fn entering_repository_resets_author_and_target_filters_but_keeps_states() {
        // 別 repo からの入場: author / target はリポジトリ依存なので両方リセット、states は
        // 維持する。
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.selected_repo = Some("acme/gadget".to_string());
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        app.pr_state_filter = PrStateFilter {
            states: BTreeSet::from([PrState::Merged]),
            author: Some(author("Alice", "{u-1}")),
            target_branch: Some(target("main", true)),
        };
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(
            app.pr_state_filter.states,
            BTreeSet::from([PrState::Merged])
        );
        assert!(app.pr_state_filter.author.is_none());
        assert!(app.pr_state_filter.target_branch.is_none());

        // 同一 repo への再入場: repo コンテキストは変わらないので author / target を維持する。
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.pr_state_filter.target_branch = Some(target("main", true));
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("main", true))
        );
    }

    #[test]
    fn ctrl_k_does_not_open_jump_palette_while_pr_filter_modal_is_open() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(app.jump_palette.is_none());
        assert!(app.pr_filter_modal.is_some());
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
    fn entering_detail_queues_every_body_image_through_existing_load_command() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr_with_images(8)]);

        let command = app.update(Msg::Key(key(KeyCode::Enter)));

        let Command::Batch(commands) = command else {
            panic!("expected Batch");
        };
        let image_urls = commands
            .iter()
            .filter_map(|command| match command {
                Command::LoadImage { url, .. } => Some(url.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            image_urls,
            vec!["https://example.com/a.png", "https://example.com/b.png"]
        );
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
    fn detail_focus_cycles_both_directions_and_keeps_global_keybindings() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(9, "OPEN"));
        assert_eq!(app.detail_focus, DetailFocus::Overview);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.detail_focus, DetailFocus::Files);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.detail_focus, DetailFocus::Comments);
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.detail_focus, DetailFocus::Overview);
        app.update(Msg::Key(key(KeyCode::BackTab)));
        assert_eq!(app.detail_focus, DetailFocus::Comments);

        app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert!(app.comment_editor.is_some());
    }

    #[test]
    fn comments_focus_scrolls_and_clamps_using_rendered_height() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.detail_focus = DetailFocus::Comments;
        app.comments_viewport = 3;
        app.comments_rendered_lines = Some(8);

        app.update(Msg::Key(key(KeyCode::Char('G'))));
        assert_eq!(app.comments_scroll, 5);
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.comments_scroll, 5);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert_eq!(app.comments_scroll, 0);
    }

    #[test]
    fn link_extraction_excludes_fences_and_images_deduplicates_and_keeps_bare_urls() {
        let mut links = Vec::new();
        extract_links(
            "[docs](https://example.com/docs) https://example.com/bare\n\
             ![image](https://example.com/image.png)\n\
             ```\nhttps://example.com/code\n```\n\
             https://example.com/docs",
            &mut links,
        );

        assert_eq!(
            links,
            vec![
                DetailLink {
                    label: "docs".to_string(),
                    url: "https://example.com/docs".to_string(),
                },
                DetailLink {
                    label: "https://example.com/bare".to_string(),
                    url: "https://example.com/bare".to_string(),
                },
            ]
        );
    }

    #[test]
    fn detail_o_without_html_url_reports_error_and_stays_on_screen() {
        // html_url が無い PR では実プロセスを起動せず Status::Error にするだけ
        // （`open` コマンドは呼ばれない分岐を検証する。実起動の分岐は
        // `PullRequest::html_url` 側の単体テストで代替する）。
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(20, "OPEN"));
        assert!(app.current_pr.as_ref().expect("pr").html_url().is_none());
        let cmd = app.update(Msg::Key(key(KeyCode::Char('o'))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn detail_o_without_current_pr_reports_error() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = None;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('o'))));
        assert!(matches!(cmd, Command::None));
        assert!(matches!(app.status, Status::Error(_)));
    }

    // ---- ImageView（`i`） ----

    /// 本文に画像 2 枚を含む PR の JSON（`make_pr` は `description` を持たないため専用に組む）。
    fn make_pr_with_images(id: u64) -> PullRequest {
        let json = format!(
            r#"{{ "id": {id}, "state": "OPEN",
                  "description": "見て: ![alt1](https://example.com/a.png) と ![alt2](https://example.com/b.png)",
                  "participants": [] }}"#
        );
        serde_json::from_str(&json).expect("valid pr json")
    }

    #[test]
    fn detail_i_with_images_and_supported_terminal_opens_image_view_and_loads_first() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr_with_images(1));
        app.image_picker = Some(Picker::from_fontsize((10, 20)));

        let cmd = app.update(Msg::Key(key(KeyCode::Char('i'))));

        assert_eq!(app.screen, Screen::ImageView);
        assert_eq!(app.image_index, 0);
        assert_eq!(app.image_refs.len(), 2);
        match cmd {
            Command::LoadImage { url, .. } => assert_eq!(url, "https://example.com/a.png"),
            other => panic!("expected LoadImage, got {other:?}"),
        }
    }

    #[test]
    fn detail_i_without_images_reports_error_and_stays_on_detail() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(30, "OPEN"));
        app.image_picker = Some(Picker::from_fontsize((10, 20)));

        let cmd = app.update(Msg::Key(key(KeyCode::Char('i'))));

        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(cmd, Command::None));
        match &app.status {
            Status::Error(message) => assert!(message.contains("画像がありません")),
            other => panic!("expected Status::Error, got {other:?}"),
        }
    }

    #[test]
    fn detail_i_with_images_but_unsupported_terminal_reports_guidance() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr_with_images(2));
        app.image_picker = None; // 端末検出失敗（Picker 無し）を模す。

        let cmd = app.update(Msg::Key(key(KeyCode::Char('i'))));

        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(cmd, Command::None));
        match &app.status {
            Status::Error(message) => assert!(message.contains("未対応")),
            other => panic!("expected Status::Error, got {other:?}"),
        }
    }

    #[test]
    fn image_view_n_and_right_advance_and_clamp_at_last() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![
            ImageRef {
                alt: "a".to_string(),
                url: "https://example.com/a.png".to_string(),
            },
            ImageRef {
                alt: "b".to_string(),
                url: "https://example.com/b.png".to_string(),
            },
        ];
        app.image_index = 0;

        let cmd = app.update(Msg::Key(key(KeyCode::Char('n'))));
        assert_eq!(app.image_index, 1);
        assert!(matches!(cmd, Command::LoadImage { .. }));

        // 末尾でさらに次へ進もうとしても何も起きない（境界クランプ）。
        let cmd = app.update(Msg::Key(key(KeyCode::Right)));
        assert_eq!(app.image_index, 1);
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn image_view_p_and_left_go_back_and_clamp_at_first() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![
            ImageRef {
                alt: "a".to_string(),
                url: "https://example.com/a.png".to_string(),
            },
            ImageRef {
                alt: "b".to_string(),
                url: "https://example.com/b.png".to_string(),
            },
        ];
        app.image_index = 1;

        let cmd = app.update(Msg::Key(key(KeyCode::Char('p'))));
        assert_eq!(app.image_index, 0);
        assert!(matches!(cmd, Command::LoadImage { .. }));

        // 先頭でさらに前へ戻ろうとしても何も起きない（境界クランプ）。
        let cmd = app.update(Msg::Key(key(KeyCode::Left)));
        assert_eq!(app.image_index, 0);
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn image_view_esc_returns_to_pull_request_detail() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        let cmd = app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(cmd, Command::None));
    }

    /// テスト用の最小限の PNG バイト列（1x1px 不透明赤）。
    fn tiny_png_bytes() -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::from(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .expect("PNG エンコードに成功すること");
        buffer.into_inner()
    }

    #[test]
    fn image_loaded_with_valid_bytes_decodes_and_updates_current_image() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![ImageRef {
            alt: "a".to_string(),
            url: "https://example.com/a.png".to_string(),
        }];
        app.image_index = 0;
        app.image_picker = Some(Picker::from_fontsize((10, 20)));

        app.update(Msg::ImageLoaded {
            url: "https://example.com/a.png".to_string(),
            result: Ok(tiny_png_bytes()),
        });

        let image = app
            .current_image
            .as_ref()
            .expect("current_image が Some であること")
            .as_ref()
            .expect("デコード成功");
        assert_eq!((image.width(), image.height()), (1, 1));
        assert_eq!(app.status, Status::Idle);
        // デコード成功 + Picker あり → 描画用 StatefulProtocol が生成される。
        assert!(app.image_protocol.is_some());
    }

    #[test]
    fn image_loaded_without_picker_decodes_but_leaves_protocol_none() {
        // 端末検出に失敗した環境（`image_picker == None`）でも `open_image_view` 側でガードして
        // いるため通常は到達しないが、念のため防御的に protocol 生成をスキップすることを確認する。
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![ImageRef {
            alt: "a".to_string(),
            url: "https://example.com/a.png".to_string(),
        }];
        app.image_index = 0;
        app.image_picker = None;

        app.update(Msg::ImageLoaded {
            url: "https://example.com/a.png".to_string(),
            result: Ok(tiny_png_bytes()),
        });

        assert!(app.current_image.as_ref().expect("Some").is_ok());
        assert!(app.image_protocol.is_none());
    }

    #[test]
    fn image_loaded_with_corrupt_bytes_sets_error_without_panicking() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![ImageRef {
            alt: "a".to_string(),
            url: "https://example.com/a.png".to_string(),
        }];
        app.image_index = 0;
        app.image_picker = Some(Picker::from_fontsize((10, 20)));

        app.update(Msg::ImageLoaded {
            url: "https://example.com/a.png".to_string(),
            result: Ok(b"not a real image".to_vec()),
        });

        assert!(app.current_image.as_ref().expect("Some").is_err());
        assert!(matches!(app.status, Status::Error(_)));
        // デコード失敗時は Picker があっても protocol は作らない。
        assert!(app.image_protocol.is_none());
    }

    #[test]
    fn image_loaded_caches_a_distinct_overview_protocol_by_url() {
        let mut app = review_app();
        app.image_picker = Some(Picker::from_fontsize((10, 20)));
        let url = "https://example.com/inline.png";

        app.update(Msg::ImageLoaded {
            url: url.to_string(),
            result: Ok(tiny_png_bytes()),
        });

        assert!(app.overview_image_protocols.contains_key(url));
    }

    #[test]
    fn image_loaded_with_bitbucket_attachment_error_surfaces_exact_guidance() {
        const MESSAGE: &str = "この画像（Bitbucket 添付）は API token では取得できません。o でブラウザ表示してください";
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![ImageRef {
            alt: "attachment".to_string(),
            url: "https://bitbucket.org/workspace/repo/images/file.png".to_string(),
        }];

        app.update(Msg::ImageLoaded {
            url: "https://bitbucket.org/workspace/repo/images/file.png".to_string(),
            result: Err(MESSAGE.to_string()),
        });

        assert_eq!(app.status, Status::Error(MESSAGE.to_string()));
        assert_eq!(
            app.current_image,
            Some(Err(MESSAGE.to_string())),
            "raw reqwest error must not replace the dedicated guidance"
        );
    }

    #[test]
    fn image_loaded_for_stale_url_is_ignored_but_cache_still_updates() {
        let mut app = review_app();
        app.screen = Screen::ImageView;
        app.image_refs = vec![
            ImageRef {
                alt: "a".to_string(),
                url: "https://example.com/a.png".to_string(),
            },
            ImageRef {
                alt: "b".to_string(),
                url: "https://example.com/b.png".to_string(),
            },
        ];
        // ユーザーは既に 2 枚目へ進んでいるが、1 枚目（古い URL）の応答が遅れて届く想定。
        app.image_index = 1;
        app.current_image = None;
        app.image_picker = Some(Picker::from_fontsize((10, 20)));

        app.update(Msg::ImageLoaded {
            url: "https://example.com/a.png".to_string(),
            result: Ok(tiny_png_bytes()),
        });

        // 現在表示中（2 枚目）の状態は上書きされない。
        assert!(app.current_image.is_none());
        assert!(app.image_protocol.is_none());

        // ただしキャッシュには反映されるため、1 枚目へ戻れば再取得せず即表示できる。
        app.image_index = 0;
        let cmd = app.load_current_image();
        assert!(matches!(cmd, Command::None));
        assert!(app.current_image.as_ref().expect("Some").is_ok());
        // キャッシュ即時反映（`load_current_image`）でも protocol は再生成される。
        assert!(app.image_protocol.is_some());
    }

    #[test]
    fn detail_scroll_page_down_clamps_to_body_end() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        // 本文なし PR: ヘッダ 4 行 + プレースホルダ 1 行 = 5 行。viewport 2 → 上限 3。
        app.current_pr = Some(make_pr(11, "OPEN"));
        app.detail_viewport = 2;

        for _ in 0..10 {
            app.update(Msg::Key(key(KeyCode::PageDown)));
        }
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn detail_scroll_page_up_stops_at_zero() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(12, "OPEN"));
        app.detail_viewport = 2;

        app.update(Msg::Key(key(KeyCode::PageUp)));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn clamp_detail_scroll_limits_to_body_line_count() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(14, "OPEN"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // 5 行（ヘッダ 4 + プレースホルダ 1） - viewport 2 = 上限 3。
        assert_eq!(app.detail_scroll, 3);
    }

    #[test]
    fn clamp_detail_scroll_accounts_for_multiline_body() {
        let mut app = review_app();
        let json = r#"{ "id": 15, "state": "OPEN", "description": "line1\nline2\nline3",
            "participants": [] }"#;
        app.current_pr = Some(serde_json::from_str(json).expect("valid pr json"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // ヘッダ 4 行 + 本文 3 行 = 7 行 - viewport 2 = 上限 5。
        assert_eq!(app.detail_scroll, 5);
    }

    #[test]
    fn clamp_detail_scroll_accounts_for_participant_panel() {
        let mut app = review_app();
        let json = r#"{ "id": 16, "state": "OPEN", "description": "line1\nline2",
            "participants": [
                { "user": { "display_name": "Bob" }, "approved": true, "state": "approved" },
                { "user": { "display_name": "Carol" }, "approved": false, "state": "changes_requested" }
            ] }"#;
        app.current_pr = Some(serde_json::from_str(json).expect("valid pr json"));
        app.detail_viewport = 2;
        app.detail_scroll = 999;

        app.clamp_detail_scroll();

        // ヘッダ 4 行 + 承認/変更要求パネル 2 行 + 本文 2 行 = 8 行 - viewport 2 = 上限 6。
        assert_eq!(app.detail_scroll, 6);
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
        // 既定フォーカスは本文なので、サイドバー導入前と同じく ↑↓/jk が直接スクロールする。
        assert_eq!(diff.focus, DiffFocus::Body);
        // ビューポート未設定でも 1 行スクロールできる。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 1);
    }

    /// 2 ファイル分の diff テキスト（各ファイル 3 行のコンテキスト行）。サイドバー関連の
    /// テストで使う。
    fn multi_file_diff_text() -> String {
        "diff --git a/one.txt b/one.txt\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,3 +1,3 @@\n\
 one line 0\n\
 one line 1\n\
 one line 2\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -1,3 +1,3 @@\n\
 two line 0\n\
 two line 1\n\
 two line 2\n"
            .to_string()
    }

    #[test]
    fn diff_tab_toggles_focus_between_files_and_body() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Files);

        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);
    }

    #[test]
    fn diff_sidebar_selection_moves_scroll_to_file_start() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        let second_file_start = app.diff.as_ref().expect("diff").parsed.files[1].start;

        app.update(Msg::Key(key(KeyCode::Tab))); // フォーカスをファイル一覧へ。
        app.update(Msg::Key(key(KeyCode::Char('j')))); // 1 つ下（2 番目のファイル）を選択。

        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 1);
        assert_eq!(diff.scroll, second_file_start);
    }

    #[test]
    fn diff_sidebar_selection_stays_within_bounds() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });
        app.update(Msg::Key(key(KeyCode::Tab)));

        // 先頭で上へ: 変化しない。
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.diff.as_ref().expect("diff").file_index, 0);

        // 末尾を超えて下へ連打しても最後のファイルで止まる（2 ファイルのみ）。
        for _ in 0..5 {
            app.update(Msg::Key(key(KeyCode::Down)));
        }
        assert_eq!(app.diff.as_ref().expect("diff").file_index, 1);
    }

    #[test]
    fn diff_body_focus_ignores_sidebar_navigation_and_scrolls_instead() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });

        // 本文フォーカスのまま（Tab を押していない）↓ を押すと本文が 1 行スクロールし、
        // サイドバー選択（file_index）は動かない。
        app.update(Msg::Key(key(KeyCode::Down)));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.scroll, 1);
        assert_eq!(diff.file_index, 0);
    }

    #[test]
    fn diff_next_file_key_syncs_sidebar_file_index_regardless_of_focus() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: multi_file_diff_text(),
        });

        // 本文フォーカスのまま `n` で次ファイルへジャンプしても file_index が同期する。
        // 現在行（cursor）もそのファイルの先頭へ移動する。
        app.update(Msg::Key(key(KeyCode::Char('n'))));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 1);
        assert_eq!(diff.scroll, diff.parsed.files[1].start);
        assert_eq!(diff.cursor, diff.parsed.files[1].start);

        app.update(Msg::Key(key(KeyCode::Char('N'))));
        let diff = app.diff.as_ref().expect("diff present");
        assert_eq!(diff.file_index, 0);
        assert_eq!(diff.scroll, diff.parsed.files[0].start);
        assert_eq!(diff.cursor, diff.parsed.files[0].start);
    }

    // ---- 現在行カーソル（Diff 画面） ----

    /// 十分な行数を持つ diff（1 ファイル・全て文脈行）から、指定 viewport の Diff 画面を作る。
    fn diff_app_with_lines(line_count: usize, viewport: usize) -> App {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        let mut text = "diff --git a/x b/x\n--- a/x\n+++ b/x\n".to_string();
        text.push_str(&format!("@@ -1,{line_count} +1,{line_count} @@\n"));
        for index in 0..line_count {
            text.push_str(&format!(" context {index}\n"));
        }
        app.update(Msg::DiffLoaded { id: 9, text });
        if let Some(diff) = app.diff.as_mut() {
            diff.viewport = viewport;
        }
        app
    }

    #[test]
    fn diff_cursor_moves_one_line_with_j_k() {
        let mut app = diff_app_with_lines(20, 10);
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 1);
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 0);
    }

    #[test]
    fn diff_cursor_clamps_at_top_and_bottom_boundaries() {
        let mut app = diff_app_with_lines(5, 10);
        // 先頭で上へ: 変化しない（パニックもしない）。
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 0);

        let last = app.diff.as_ref().expect("diff").parsed.len() - 1;
        app.update(Msg::Key(key(KeyCode::Char('G'))));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, last);
        // 末尾を超えて下へ連打しても最後の行で止まる。
        for _ in 0..5 {
            app.update(Msg::Key(key(KeyCode::Down)));
        }
        assert_eq!(app.diff.as_ref().expect("diff").cursor, last);
    }

    #[test]
    fn diff_cursor_page_up_down_moves_by_viewport_and_auto_scrolls() {
        let mut app = diff_app_with_lines(100, 10);
        app.update(Msg::Key(key(KeyCode::PageDown)));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.cursor, 10);
        // 現在行が viewport 内に収まるよう自動スクロールする。
        assert!(diff.cursor >= diff.scroll && diff.cursor < diff.scroll + diff.viewport);

        app.update(Msg::Key(key(KeyCode::PageUp)));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.cursor, 0);
        assert_eq!(diff.scroll, 0);
    }

    #[test]
    fn diff_cursor_g_and_shift_g_jump_to_top_and_bottom() {
        let mut app = diff_app_with_lines(50, 10);
        app.update(Msg::Key(key(KeyCode::Char('G'))));
        let diff = app.diff.as_ref().expect("diff");
        let last = diff.parsed.len() - 1;
        assert_eq!(diff.cursor, last);
        assert!(diff.cursor < diff.scroll + diff.viewport);

        app.update(Msg::Key(key(KeyCode::Char('g'))));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.cursor, 0);
        assert_eq!(diff.scroll, 0);
    }

    // ---- split 表示（`v`。#Diff-split-view） ----

    #[test]
    fn diff_v_key_toggles_view_mode_between_unified_and_split() {
        let mut app = diff_app_with_lines(10, 10);
        assert_eq!(
            app.diff.as_ref().expect("diff").view_mode,
            DiffViewMode::Unified
        );

        app.update(Msg::Key(key(KeyCode::Char('v'))));
        assert_eq!(
            app.diff.as_ref().expect("diff").view_mode,
            DiffViewMode::Split
        );
        assert_eq!(app.diff_view_mode, DiffViewMode::Split);

        app.update(Msg::Key(key(KeyCode::Char('v'))));
        assert_eq!(
            app.diff.as_ref().expect("diff").view_mode,
            DiffViewMode::Unified
        );
        assert_eq!(app.diff_view_mode, DiffViewMode::Unified);
    }

    #[test]
    fn diff_v_key_persists_view_mode_to_config() {
        let mut app = diff_app_with_lines(10, 10);
        app.update(Msg::Key(key(KeyCode::Char('v'))));
        assert_eq!(app.config.diff_view.as_deref(), Some("split"));

        app.update(Msg::Key(key(KeyCode::Char('v'))));
        assert_eq!(app.config.diff_view.as_deref(), Some("unified"));
    }

    #[test]
    fn app_new_initializes_diff_view_mode_from_config() {
        let config = Config {
            diff_view: Some("split".to_string()),
            ..Config::default()
        };
        let app = App::new(config, None);
        assert_eq!(app.diff_view_mode, DiffViewMode::Split);
    }

    // ---- Diff サイドバーの表示/非表示・幅調整（`t`・境界ドラッグ） ----

    #[test]
    fn resolve_diff_sidebar_width_uses_default_percent_when_none() {
        assert_eq!(resolve_diff_sidebar_width(60, None), 18); // 60 の 30%。
    }

    #[test]
    fn resolve_diff_sidebar_width_clamps_desired_up_to_min_width() {
        assert_eq!(
            resolve_diff_sidebar_width(60, Some(1)),
            DIFF_SIDEBAR_MIN_WIDTH
        );
    }

    #[test]
    fn resolve_diff_sidebar_width_clamps_desired_down_to_max_percent() {
        // 全体 60 の 70% = 42 が上限。
        assert_eq!(resolve_diff_sidebar_width(60, Some(1000)), 42);
    }

    #[test]
    fn resolve_diff_sidebar_width_never_panics_when_total_narrower_than_min() {
        // 全体幅が極端に狭い場合は下限より上限を優先し、パニックしない。
        assert_eq!(resolve_diff_sidebar_width(5, None), 3);
        assert_eq!(resolve_diff_sidebar_width(0, None), 0);
    }

    #[test]
    fn app_new_initializes_diff_sidebar_from_config() {
        let config = Config {
            diff_sidebar_visible: Some(false),
            diff_sidebar_width: Some(24),
            ..Config::default()
        };
        let app = App::new(config, None);
        assert!(!app.diff_sidebar_visible);
        assert_eq!(app.diff_sidebar_width, Some(24));
    }

    #[test]
    fn app_new_defaults_diff_sidebar_visible_when_config_unset() {
        let app = App::new(Config::default(), None);
        assert!(app.diff_sidebar_visible, "既定は表示");
        assert_eq!(app.diff_sidebar_width, None);
    }

    #[test]
    fn diff_t_key_toggles_sidebar_visibility_and_persists_immediately() {
        let mut app = diff_app_with_lines(10, 10);
        assert!(app.diff_sidebar_visible);

        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert!(!app.diff_sidebar_visible);
        assert_eq!(app.config.diff_sidebar_visible, Some(false));

        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert!(app.diff_sidebar_visible);
        assert_eq!(app.config.diff_sidebar_visible, Some(true));
    }

    #[test]
    fn diff_t_key_hidden_forces_focus_to_body_and_blocks_tab_until_reshown() {
        let mut app = diff_app_with_lines(10, 10);
        app.update(Msg::Key(key(KeyCode::Tab))); // フォーカスをファイル一覧へ。
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Files);

        app.update(Msg::Key(key(KeyCode::Char('t'))));
        assert!(!app.diff_sidebar_visible);
        assert_eq!(
            app.diff.as_ref().expect("diff").focus,
            DiffFocus::Body,
            "非表示にした時点で本文へ固定する"
        );

        // 非表示中は Tab を押してもファイル一覧へは戻らない。
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);

        // カーソル移動キーは通常どおり本文を動かす。
        app.update(Msg::Key(key(KeyCode::Char('j'))));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 1);

        // 再表示すれば Tab でファイル一覧へ戻れる。
        app.update(Msg::Key(key(KeyCode::Char('t'))));
        app.update(Msg::Key(key(KeyCode::Tab)));
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Files);
    }

    #[test]
    fn diff_t_key_restores_previous_width_on_reshow() {
        let mut app = diff_app_with_lines(10, 10);
        app.diff_sidebar_width = Some(20);

        app.update(Msg::Key(key(KeyCode::Char('t')))); // 非表示。
        assert_eq!(
            app.diff_sidebar_width,
            Some(20),
            "幅は非表示にしても変わらない"
        );

        app.update(Msg::Key(key(KeyCode::Char('t')))); // 再表示。
        assert_eq!(
            app.diff_sidebar_width,
            Some(20),
            "再表示時に直前の幅が保たれている"
        );
    }

    #[test]
    fn diff_sidebar_drag_from_boundary_resizes_width_and_persists_on_up() {
        let mut app = diff_app_with_lines(10, 10);
        app.layout
            .panes
            .push((PaneKind::DiffFiles, Rect::new(0, 0, 18, 10)));
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(18, 0, 42, 10)));

        // 境界列（body_rect.x = 18）で Down するとドラッグが始まる。
        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            18,
            5,
        )));
        assert!(app.diff_sidebar_dragging);

        // 列 25 までドラッグすると幅がその場で追従する（保存はまだされない）。
        app.update(Msg::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            25,
            5,
        )));
        assert_eq!(app.diff_sidebar_width, Some(25));
        assert_eq!(
            app.config.diff_sidebar_width, None,
            "Up するまでは config へ保存されない"
        );

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            25,
            5,
        )));
        assert!(!app.diff_sidebar_dragging);
        assert_eq!(app.config.diff_sidebar_width, Some(25));
        assert_eq!(app.config.diff_sidebar_visible, Some(true));
    }

    #[test]
    fn diff_sidebar_drag_clamps_width_to_max_percent() {
        let mut app = diff_app_with_lines(10, 10);
        app.layout
            .panes
            .push((PaneKind::DiffFiles, Rect::new(0, 0, 18, 10)));
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(18, 0, 42, 10)));

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            18,
            5,
        )));
        app.update(Msg::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            59,
            5,
        )));
        // 全体 60 の 70% = 42 が上限。
        assert_eq!(app.diff_sidebar_width, Some(42));
    }

    #[test]
    fn diff_sidebar_drag_below_min_hides_sidebar_and_keeps_previous_width() {
        let mut app = diff_app_with_lines(10, 10);
        app.diff_sidebar_width = Some(20);
        app.layout
            .panes
            .push((PaneKind::DiffFiles, Rect::new(0, 0, 20, 10)));
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(20, 0, 40, 10)));

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            20,
            5,
        )));
        assert!(app.diff_sidebar_dragging);

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Drag(MouseButton::Left),
            5,
            5,
        )));
        assert!(
            !app.diff_sidebar_visible,
            "MIN 未満まで縮めたら非表示になる"
        );
        assert_eq!(
            app.diff_sidebar_width,
            Some(20),
            "直前の幅は変えずに保つ（再表示のため）"
        );
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            5,
            5,
        )));
        assert_eq!(app.config.diff_sidebar_visible, Some(false));
        assert_eq!(app.config.diff_sidebar_width, Some(20));
    }

    #[test]
    fn diff_sidebar_boundary_down_does_not_trigger_pane_focus_switch() {
        let mut app = diff_app_with_lines(10, 10);
        app.layout
            .panes
            .push((PaneKind::DiffFiles, Rect::new(0, 0, 18, 10)));
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(18, 0, 42, 10)));
        app.diff.as_mut().expect("diff").focus = DiffFocus::Body;

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            18,
            5,
        )));
        // 境界ドラッグ扱いになり、通常のペインクリック（フォーカス切替）は発火しない。
        assert_eq!(app.diff.as_ref().expect("diff").focus, DiffFocus::Body);
        assert!(app.diff_sidebar_dragging);
    }

    #[test]
    fn diff_sidebar_drag_does_nothing_when_sidebar_hidden() {
        let mut app = diff_app_with_lines(10, 10);
        app.diff_sidebar_visible = false;
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(0, 0, 60, 10)));

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            0,
            5,
        )));
        assert!(!app.diff_sidebar_dragging);
    }

    #[test]
    fn diff_mouse_up_without_drag_is_inert() {
        let mut app = diff_app_with_lines(10, 10);
        let before = app.config.diff_sidebar_width;
        app.update(Msg::Mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            0,
            0,
        )));
        assert!(!app.diff_sidebar_dragging);
        assert_eq!(app.config.diff_sidebar_width, before);
    }

    #[test]
    fn diff_loaded_uses_persisted_diff_view_mode() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_view_mode = DiffViewMode::Split;

        app.update(Msg::DiffLoaded {
            id: 9,
            text: " context\n".to_string(),
        });

        assert_eq!(
            app.diff.as_ref().expect("diff").view_mode,
            DiffViewMode::Split
        );
    }

    #[test]
    fn diff_v_key_maps_cursor_to_corresponding_split_row_and_back() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        let text = "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        {
            let diff = app.diff.as_mut().expect("diff");
            diff.viewport = 10;
            diff.cursor = 3; // unified インデックス 3 = "+new"
        }

        app.update(Msg::Key(key(KeyCode::Char('v'))));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.view_mode, DiffViewMode::Split);
        // unified: 0 diff--git / 1 @@ / 2 -old / 3 +new。
        // split_lines: [(0,0),(1,1),(2,3)]（削除・追加が同じペア行にまとまる）。
        assert_eq!(
            diff.cursor, 2,
            "unified の cursor=3 は split のペア行(2)へ変換される"
        );

        app.update(Msg::Key(key(KeyCode::Char('v'))));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.view_mode, DiffViewMode::Unified);
        assert_eq!(
            diff.cursor, 3,
            "split から戻すと元の unified 行へ復元される"
        );
    }

    #[test]
    fn diff_cursor_to_bottom_uses_split_total_lines_not_unified_when_in_split_mode() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        let text = "diff --git a/x b/x\n@@ -1,3 +1,1 @@\n-a\n-b\n-c\n+x\n".to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        {
            let diff = app.diff.as_mut().expect("diff");
            diff.viewport = 10;
            diff.view_mode = DiffViewMode::Split;
            // 表示行列はモードに依存するので組み直す（実運用では `v` の set_view_mode 経由）。
            diff.rebuild_display_rows();
        }

        app.update(Msg::Key(key(KeyCode::Char('G'))));
        let diff = app.diff.as_ref().expect("diff");
        // 3 つの削除と 1 つの追加が 3 行にペアリングされるため、split の総行数は unified
        // より少ない（5 行 vs 6 行）。`G` は split の総行数基準で末尾へ移動するべき。
        assert_eq!(diff.parsed.lines.len(), 6);
        assert_eq!(diff.parsed.split_lines.len(), 5);
        assert_eq!(diff.cursor, 4);
    }

    #[test]
    fn diff_next_file_key_uses_split_file_boundaries_in_split_mode() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        let text = "diff --git a/one.txt b/one.txt\n\
--- a/one.txt\n\
+++ b/one.txt\n\
@@ -1,1 +1,1 @@\n\
-old one\n\
+new one\n\
diff --git a/two.txt b/two.txt\n\
--- a/two.txt\n\
+++ b/two.txt\n\
@@ -1,1 +1,1 @@\n\
-old two\n\
+new two\n"
            .to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        // viewport は既定の 0 のまま（`max_scroll` を大きく保ち、ファイルジャンプ先の scroll
        // がクランプされないようにする。既存の unified 版テストと同じ流儀）。
        if let Some(diff) = app.diff.as_mut() {
            diff.view_mode = DiffViewMode::Split;
        }

        app.update(Msg::Key(key(KeyCode::Char('n'))));
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.file_index, 1);
        let expected_start = diff.parsed.split_file_starts[1];
        assert_eq!(diff.scroll, expected_start);
        assert_eq!(diff.cursor, expected_start);
    }

    #[test]
    fn diff_c_key_in_split_mode_uses_new_side_when_row_has_added_line() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        let text = "diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n".to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        {
            let diff = app.diff.as_mut().expect("diff");
            diff.viewport = 10;
            diff.view_mode = DiffViewMode::Split;
            diff.cursor = 2; // split 行(2,3) = "-old"/"+new" のペア行
        }

        app.update(Msg::Key(key(KeyCode::Char('c'))));
        let editor = app.comment_editor.as_ref().expect("inline editor opens");
        let anchor = editor.inline.as_ref().expect("anchor present");
        assert_eq!(anchor.side, CommentSide::To);
        assert_eq!(anchor.line, 1);
    }

    #[test]
    fn diff_c_key_in_split_mode_uses_old_side_when_row_is_removed_only() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        let text = "diff --git a/x b/x\n@@ -1,1 +0,0 @@\n-removed only\n".to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        {
            let diff = app.diff.as_mut().expect("diff");
            diff.viewport = 10;
            diff.view_mode = DiffViewMode::Split;
            diff.cursor = 2; // split 行(Some(2), None) = 追加ブロックを伴わない削除のみ
        }

        app.update(Msg::Key(key(KeyCode::Char('c'))));
        let editor = app.comment_editor.as_ref().expect("inline editor opens");
        let anchor = editor.inline.as_ref().expect("anchor present");
        assert_eq!(anchor.side, CommentSide::From);
        assert_eq!(anchor.line, 1);
    }

    #[test]
    fn diff_mouse_wheel_moves_cursor_in_split_mode_bounded_by_split_total_lines() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        let text = "diff --git a/x b/x\n@@ -1,3 +1,1 @@\n-a\n-b\n-c\n+x\n".to_string();
        app.update(Msg::DiffLoaded { id: 9, text });
        {
            let diff = app.diff.as_mut().expect("diff");
            diff.viewport = 10;
            diff.view_mode = DiffViewMode::Split;
        }
        app.layout
            .panes
            .push((PaneKind::DiffBody, Rect::new(0, 0, 40, 10)));

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown, 5, 5)));
        let diff = app.diff.as_ref().expect("diff");
        // split_lines は 5 行（3 削除 + 1 追加が 3 行にペアリングされる）。3 行分下へ、
        // かつフォーカスも本文へ切り替わる（既存の unified と同じ挙動）。
        assert_eq!(diff.cursor, 3);
        assert_eq!(diff.focus, DiffFocus::Body);
    }

    // ---- インラインコメント（`c`。#Diff-inline-comment） ----

    #[test]
    fn diff_c_key_is_rejected_for_commit_diff() {
        let mut app = review_app();
        app.screen = Screen::Diff;
        app.diff_return = Screen::CommitDetail;
        app.current_pr = None;
        app.diff = Some(DiffState {
            parsed: parse_diff("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n"),
            scroll: 0,
            viewport: 10,
            title: "abc1234".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 3, // "+new"（追加行）
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.comment_editor.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn diff_c_key_opens_inline_editor_on_pr_diff_at_addable_line() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.diff = Some(DiffState {
            parsed: parse_diff("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n"),
            scroll: 0,
            viewport: 10,
            title: "#9".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 3, // "+new"（追加行、コメント可能）
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        });

        app.update(Msg::Key(key(KeyCode::Char('c'))));
        let editor = app.comment_editor.as_ref().expect("inline editor opens");
        let anchor = editor.inline.as_ref().expect("anchor present");
        assert_eq!(anchor.path, "x");
        assert_eq!(anchor.side, CommentSide::To);
        assert_eq!(anchor.line, 1);
    }

    #[test]
    fn diff_c_key_on_uncommentable_line_shows_error_and_does_not_open_editor() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.diff = Some(DiffState {
            parsed: parse_diff("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n"),
            scroll: 0,
            viewport: 10,
            title: "#9".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 1, // "@@ -1 +1 @@"（ハンクヘッダ、コメント不可）
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.comment_editor.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn diff_inline_comment_submit_dispatches_create_inline_comment_with_anchor() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.diff = Some(DiffState {
            parsed: parse_diff("diff --git a/x b/x\n@@ -1 +1 @@\n-old\n+new\n"),
            scroll: 0,
            viewport: 10,
            title: "#9".to_string(),
            rendered_lines: None,
            rendered_split: None,
            file_index: 0,
            cursor: 2, // "-old"（削除行）
            focus: DiffFocus::Body,
            view_mode: DiffViewMode::Unified,
            comment_layout: CommentLayout::default(),
            sidebar_rows: Vec::new(),
            display_rows: Vec::new(),
            thread_collapse: HashMap::new(),
        });

        app.update(Msg::Key(key(KeyCode::Char('c'))));
        for ch in "なぜ削除？".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(ctrl(KeyCode::Char('s'))));
        match cmd {
            Command::CreateInlineComment {
                id,
                path,
                side,
                line,
                raw,
                ..
            } => {
                assert_eq!(id, 9);
                assert_eq!(path, "x");
                assert_eq!(side, CommentSide::From);
                assert_eq!(line, 1);
                assert_eq!(raw, "なぜ削除？");
            }
            other => panic!("expected CreateInlineComment, got {other:?}"),
        }
    }

    #[test]
    fn build_comment_layout_places_thread_at_matching_line_with_count() {
        // 行: 0=FileHeader 1=Hunk 2=Context(new1) 3=Added(new2) 4=Context(new3)
        let parsed =
            parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,2 +1,3 @@\n ctx1\n+added\n ctx2\n");
        let comment = make_inline_comment(7, "LGTM", "x.rs", 2, "2026-01-01T00:00:00Z", None);
        let layout = build_comment_layout(&parsed, &[comment], &Me::default());
        // 新側 2 行目（追加行）は unified 行インデックス 3。
        let threads = layout.threads_by_line.get(&3).expect("thread at line 3");
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].root_id, 7);
        assert!(!threads[0].resolved);
        assert_eq!(threads[0].comments.len(), 1);
        assert_eq!(layout.file_comment_counts, vec![1]);
    }

    #[test]
    fn build_comment_layout_aggregates_reply_under_root_in_time_order() {
        let parsed =
            parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,2 +1,3 @@\n ctx1\n+added\n ctx2\n");
        let root = make_inline_comment(1, "root", "x.rs", 2, "2026-01-01T00:00:00Z", None);
        let reply = make_inline_comment(2, "reply", "x.rs", 2, "2026-01-02T00:00:00Z", Some(1));
        // 入力順を逆にしても created_on 昇順（ルート→返信）で並ぶ。
        let layout = build_comment_layout(&parsed, &[reply, root], &Me::default());
        let threads = layout.threads_by_line.get(&3).expect("thread at line 3");
        let replies: Vec<bool> = threads[0].comments.iter().map(|c| c.reply).collect();
        assert_eq!(replies, vec![false, true]);
        assert_eq!(threads[0].root_id, 1);
        assert_eq!(layout.file_comment_counts, vec![2]);
    }

    #[test]
    fn build_comment_layout_maps_removed_line_via_from_old_no() {
        // 行: 0=FileHeader 1=Hunk 2=Removed(-old, old_no=1) 3=Added(+new)
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n");
        // 旧側 1 行目（削除行）へのコメントは from=1 → old_no==1 の行インデックス 2 に対応する。
        let json = r#"{ "id": 7, "content": { "raw": "why removed?" },
                        "user": { "display_name": "Alice" }, "deleted": false,
                        "created_on": "2026-01-01T00:00:00Z",
                        "inline": { "path": "x.rs", "from": 1 } }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid inline comment json");
        let layout = build_comment_layout(&parsed, &[comment], &Me::default());
        let threads = layout.threads_by_line.get(&2).expect("thread at line 2");
        assert_eq!(threads[0].root_id, 7);
    }

    #[test]
    fn build_comment_layout_falls_back_to_file_header_when_line_missing() {
        let parsed =
            parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,2 +1,3 @@\n ctx1\n+added\n ctx2\n");
        // 存在しない新側 999 行 → ファイル先頭行（インデックス 0）へフォールバック。
        let comment = make_inline_comment(3, "stale", "x.rs", 999, "2026-01-01T00:00:00Z", None);
        let layout = build_comment_layout(&parsed, &[comment], &Me::default());
        assert!(layout.threads_by_line.contains_key(&0));
        assert_eq!(layout.file_comment_counts, vec![1]);
    }

    #[test]
    fn max_scroll_without_comments_is_total_minus_viewport() {
        let parsed = parse_diff(&"a\n".repeat(30));
        let diff = DiffState {
            parsed,
            viewport: 10,
            view_mode: DiffViewMode::Unified,
            ..Default::default()
        };
        assert_eq!(diff.max_scroll(), 20);
    }

    /// アンカー（新側 2 行目 = unified 行 3）にコメント 1 件のスレッドを持つ diff を組む。
    /// 表示行列: Diff0..Diff3, [Top, Header, Body, Actions, Bottom, Spacer], Diff4（計 11 行）。
    fn diff_with_thread(view_mode: DiffViewMode) -> DiffState {
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n");
        let comment = make_inline_comment(1, "hi", "x.rs", 2, "2026-01-01T00:00:00Z", None);
        let comment_layout = build_comment_layout(&parsed, &[comment], &Me::default());
        let mut diff = DiffState {
            parsed,
            viewport: 20,
            view_mode,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        diff
    }

    /// スレッドのヘッダ行（カーソル停止対象）の表示行インデックス。
    fn thread_header_index(diff: &DiffState) -> usize {
        diff.display_rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    DisplayRow::Comment(CommentRow {
                        kind: CommentRowKind::Header { .. },
                        ..
                    })
                )
            })
            .expect("header row present")
    }

    #[test]
    fn display_rows_interleave_thread_after_anchor_line() {
        let diff = diff_with_thread(DiffViewMode::Unified);
        // Diff0..Diff3 の直後（index 4）が枠上端、その後にヘッダ・本文・枠下端が続く。
        assert!(matches!(
            diff.display_rows.get(4),
            Some(DisplayRow::Comment(CommentRow {
                kind: CommentRowKind::Top,
                ..
            }))
        ));
        let header = thread_header_index(&diff);
        assert!(diff.is_focusable(header));
        // 枠上端はカーソル停止対象でない。
        assert!(!diff.is_focusable(4));
    }

    #[test]
    fn move_cursor_skips_box_border_and_lands_on_comment_header() {
        let mut diff = diff_with_thread(DiffViewMode::Unified);
        // アンカー diff 行（表示行 3）から下へ。枠上端(4)を飛ばしてヘッダ(5)へ。
        diff.cursor = 3;
        diff.move_cursor(1);
        assert_eq!(diff.cursor, thread_header_index(&diff));
        assert_eq!(diff.cursor_comment(), Some((1, 1)));
    }

    #[test]
    fn max_scroll_counts_display_rows_including_comments() {
        let diff = diff_with_thread(DiffViewMode::Unified);
        // 表示行 = 5 diff + 6 comment(Top/Header/Body/Actions/Bottom/Spacer) = 11。
        // viewport 20 なので上限 0。
        assert_eq!(diff.display_rows.len(), 11);
        assert_eq!(diff.max_scroll(), 0);
    }

    #[test]
    fn cursor_comment_is_none_on_diff_line() {
        let diff = diff_with_thread(DiffViewMode::Unified);
        // 先頭（diff 行）ではコメント選択なし。
        assert_eq!(diff.cursor_comment(), None);
    }

    #[test]
    fn format_when_uses_relative_labels_within_24h() {
        // 2026-01-01T00:00:00Z = 1767225600
        let t0 = "2026-01-01T00:00:00Z";
        let base = 1_767_225_600i64;
        assert_eq!(format_when(t0, base + 30), "just now");
        assert_eq!(format_when(t0, base + 27 * 60), "27m ago");
        assert_eq!(format_when(t0, base + 2 * 3600), "2h ago");
        assert_eq!(format_when(t0, base + 25 * 3600), "2026-01-01");
        // タイムゾーンオフセット: +09:00 は UTC より 9 時間前の絶対時刻。
        assert_eq!(
            format_when("2026-01-01T09:00:00+09:00", base + 120),
            "2m ago"
        );
        // パース不能は日付フォールバック。
        assert_eq!(format_when("invalid-date", base), "invalid-da");
    }

    #[test]
    fn comment_action_labels_gate_resolve_and_own_actions() {
        let root_mine: Vec<&str> = comment_action_labels(true, true, false)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(root_mine, vec!["Reply", "Resolve", "Edit", "Delete"]);
        let root_resolved: Vec<&str> = comment_action_labels(true, false, true)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(root_resolved, vec!["Reply", "Reopen"]);
        let reply_other: Vec<&str> = comment_action_labels(false, false, false)
            .into_iter()
            .map(|(_, label)| label)
            .collect();
        assert_eq!(reply_other, vec!["Reply"]);
    }

    #[test]
    fn display_rows_include_actions_row_per_comment() {
        let diff = diff_with_thread(DiffViewMode::Unified);
        let actions: Vec<bool> = diff
            .display_rows
            .iter()
            .filter_map(|row| match row {
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Actions { root, .. },
                    ..
                }) => Some(*root),
                _ => None,
            })
            .collect();
        // コメント 1 件（ルート）に 1 本のアクション行（root=true）。
        assert_eq!(actions, vec![true]);
    }

    #[test]
    fn display_rows_append_spacer_after_thread_block() {
        let diff = diff_with_thread(DiffViewMode::Unified);
        // Diff0..Diff3, Top(4), Header(5), Body(6), Actions(7), Bottom(8), Spacer(9), Diff4(10)。
        assert!(matches!(
            diff.display_rows.get(9),
            Some(DisplayRow::Comment(CommentRow {
                kind: CommentRowKind::Spacer,
                ..
            }))
        ));
        assert!(!diff.is_focusable(9), "Spacer はカーソル停止対象でない");
        assert!(matches!(
            diff.display_rows.get(10),
            Some(DisplayRow::Diff(4))
        ));
    }

    #[test]
    fn move_cursor_skips_spacer_and_lands_on_next_diff_line() {
        let mut diff = diff_with_thread(DiffViewMode::Unified);
        diff.cursor = thread_header_index(&diff);
        // ヘッダから下へ 1 ステップ: Body/Actions/Bottom/Spacer を飛ばして Diff4 へ。
        diff.move_cursor(1);
        assert_eq!(diff.row_diff_index(diff.cursor), Some(4));
    }

    #[test]
    fn click_on_spacer_row_is_noop() {
        let mut diff = diff_with_thread(DiffViewMode::Unified);
        assert_eq!(diff.cursor, 0);
        // Spacer は表示行 9。枠線 1 行を挟むのでクリック座標 y = 1 + 9（scroll=0）。
        diff.click_body_row(Rect::new(0, 0, 40, 20), (5, 10));
        assert_eq!(
            diff.cursor, 0,
            "Spacer クリックはスレッド先頭選択に落とさず何もしない"
        );
    }

    /// 解決済みスレッド（コメント 1 件）を持つ diff。
    fn diff_with_resolved_thread() -> DiffState {
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n");
        let json = r#"{ "id": 1, "content": { "raw": "done" },
                        "user": { "display_name": "Alice" }, "deleted": false,
                        "created_on": "2026-01-01T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 2 },
                        "resolution": {} }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid resolved comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Me::default());
        let mut diff = DiffState {
            parsed,
            viewport: 20,
            view_mode: DiffViewMode::Unified,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        diff
    }

    #[test]
    fn resolved_thread_auto_collapses_to_single_row() {
        let diff = diff_with_resolved_thread();
        let collapsed_rows: Vec<&DisplayRow> = diff
            .display_rows
            .iter()
            .filter(|row| {
                matches!(
                    row,
                    DisplayRow::Comment(CommentRow {
                        kind: CommentRowKind::Collapsed { .. },
                        ..
                    })
                )
            })
            .collect();
        assert_eq!(collapsed_rows.len(), 1, "解決済みは 1 行に折りたたむ");
        // ヘッダ/本文行は出ない。
        assert!(!diff.display_rows.iter().any(|row| matches!(
            row,
            DisplayRow::Comment(CommentRow {
                kind: CommentRowKind::Header { .. },
                ..
            })
        )));
    }

    #[test]
    fn display_rows_append_spacer_after_collapsed_row() {
        let diff = diff_with_resolved_thread();
        let collapsed = diff
            .display_rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    DisplayRow::Comment(CommentRow {
                        kind: CommentRowKind::Collapsed { .. },
                        ..
                    })
                )
            })
            .expect("collapsed row present");
        assert!(matches!(
            diff.display_rows.get(collapsed + 1),
            Some(DisplayRow::Comment(CommentRow {
                kind: CommentRowKind::Spacer,
                ..
            }))
        ));
        assert!(!diff.is_focusable(collapsed + 1));
    }

    #[test]
    fn toggle_thread_collapse_expands_resolved_thread_and_back() {
        let mut diff = diff_with_resolved_thread();
        diff.toggle_thread_collapse(1);
        assert!(
            diff.display_rows.iter().any(|row| matches!(
                row,
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Header { .. },
                    ..
                })
            )),
            "展開でヘッダが現れる"
        );
        // カーソルは展開したスレッドのヘッダへ。
        assert_eq!(diff.cursor_comment(), Some((1, 1)));
        diff.toggle_thread_collapse(1);
        assert!(diff.display_rows.iter().any(|row| matches!(
            row,
            DisplayRow::Comment(CommentRow {
                kind: CommentRowKind::Collapsed { .. },
                ..
            })
        )));
    }

    #[test]
    fn enter_key_collapses_unresolved_thread_at_cursor() {
        let mut app = review_app_with_thread(); // カーソルはコメントヘッダ
        app.update(Msg::Key(key(KeyCode::Enter)));
        let diff = app.diff.as_ref().expect("diff");
        assert!(
            diff.display_rows.iter().any(|row| matches!(
                row,
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Collapsed { .. },
                    ..
                })
            )),
            "Enter で未解決スレッドも折りたためる"
        );
    }

    #[test]
    fn click_action_hit_opens_reply_editor() {
        let mut app = review_app_with_thread();
        app.layout.comment_actions.push(CommentActionHit {
            area: Rect::new(10, 5, 5, 1),
            action: CommentAction::Reply,
            comment_id: 1,
            thread_root: 1,
        });
        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            12,
            5,
        )));
        assert_eq!(
            app.comment_editor.as_ref().and_then(|e| e.reply_to),
            Some(1),
            "アクションリンクのクリックで返信エディタが開く"
        );
    }

    #[test]
    fn click_action_hit_resolve_issues_command() {
        let mut app = review_app_with_thread();
        app.layout.comment_actions.push(CommentActionHit {
            area: Rect::new(10, 5, 7, 1),
            action: CommentAction::Resolve,
            comment_id: 1,
            thread_root: 1,
        });
        match app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            11,
            5,
        ))) {
            Command::ResolveComment {
                comment_id,
                resolve,
                ..
            } => {
                assert_eq!(comment_id, 1);
                assert!(resolve);
            }
            other => panic!("expected ResolveComment, got {other:?}"),
        }
    }

    #[test]
    fn click_body_row_on_collapsed_row_expands() {
        let mut diff = diff_with_resolved_thread();
        let collapsed_index = diff
            .display_rows
            .iter()
            .position(|row| {
                matches!(
                    row,
                    DisplayRow::Comment(CommentRow {
                        kind: CommentRowKind::Collapsed { .. },
                        ..
                    })
                )
            })
            .expect("collapsed row");
        // ペイン枠 (0,0,60,20)・内側先頭行 y=1。collapsed_index 行をクリック。
        let area = Rect::new(0, 0, 60, 20);
        diff.click_body_row(area, (5, 1 + collapsed_index as u16));
        assert!(
            diff.display_rows.iter().any(|row| matches!(
                row,
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Header { .. },
                    ..
                })
            )),
            "コラプス行のクリックで展開"
        );
    }

    #[test]
    fn action_click_is_blocked_while_delete_modal_open() {
        let mut app = review_app_with_thread();
        // 削除確認モーダルを開いた状態（レイアウトにもモーダル登録）。
        app.delete_comment_modal = Some(DeleteCommentModal {
            comment_id: 1,
            submitting: false,
        });
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::DeleteCommentConfirm,
            area: Rect::new(20, 8, 20, 5),
        });
        // モーダル外にあるアクションリンクをクリックしても発火しない（Esc で閉じるだけ）。
        app.layout.comment_actions.push(CommentActionHit {
            area: Rect::new(2, 2, 5, 1),
            action: CommentAction::Reply,
            comment_id: 1,
            thread_root: 1,
        });
        let cmd = app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            3,
            2,
        )));
        assert!(
            app.comment_editor.is_none(),
            "モーダル表示中はアクションリンクが発火しない"
        );
        assert!(
            app.delete_comment_modal.is_none(),
            "モーダル外クリックは Esc 扱いで閉じる"
        );
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn collapse_override_resets_when_resolution_changes() {
        // 解決済み（自動コラプス）→ 手動展開 → 再オープン（resolved=false で再取得）→
        // 再解決（resolved=true で再取得）で、展開の上書きが破棄され再び自動コラプスする。
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: "diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n".to_string(),
        });
        if let Some(diff) = app.diff.as_mut() {
            diff.viewport = 20;
        }
        let resolved_json = r#"{ "id": 1, "content": { "raw": "done" },
                        "user": { "display_name": "Alice" }, "deleted": false,
                        "created_on": "2026-01-01T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 2 },
                        "resolution": {} }"#;
        let resolved: Comment = serde_json::from_str(resolved_json).expect("valid json");
        let reopened = make_inline_comment(1, "done", "x.rs", 2, "2026-01-01T00:00:00Z", None);

        // 解決済みで取得 → 自動コラプス → 手動展開（override=false が入る）。
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: vec![resolved.clone()],
        });
        if let Some(diff) = app.diff.as_mut() {
            diff.toggle_thread_collapse(1);
            assert_eq!(diff.thread_collapse.get(&1), Some(&false));
        }
        // 再オープン（resolved が変化）→ override 破棄。
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: vec![reopened],
        });
        assert!(
            app.diff
                .as_ref()
                .is_some_and(|diff| !diff.thread_collapse.contains_key(&1)),
            "解決状態の変化で上書きが破棄される"
        );
        // 再解決 → 既定（自動コラプス）に戻る。
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: vec![resolved],
        });
        let diff = app.diff.as_ref().expect("diff");
        assert!(
            diff.display_rows.iter().any(|row| matches!(
                row,
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Collapsed { .. },
                    ..
                })
            )),
            "再解決で自動コラプスへ戻る"
        );
    }

    #[test]
    fn format_when_far_future_falls_back_to_date() {
        // 5 分を超える未来（時計ずれ超過）は「just now」でなく日付。
        let t0 = "2026-01-01T00:00:00Z";
        let base = 1_767_225_600i64;
        assert_eq!(format_when(t0, base - 3600), "2026-01-01");
        // 5 分以内の未来は just now に丸める。
        assert_eq!(format_when(t0, base - 60), "just now");
    }

    #[test]
    fn reanchor_lands_on_collapsed_row_after_resolving_from_reply() {
        // 返信ヘッダにカーソルがある状態で解決（自動コラプス）→ カーソルはコラプス行へ。
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: "diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n".to_string(),
        });
        if let Some(diff) = app.diff.as_mut() {
            diff.viewport = 30;
        }
        let root = make_inline_comment(1, "root", "x.rs", 2, "2026-01-01T00:00:00Z", None);
        let reply = make_inline_comment(2, "reply", "x.rs", 2, "2026-01-02T00:00:00Z", Some(1));
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: vec![root, reply],
        });
        // カーソルを返信（id=2）のヘッダへ。
        if let Some(diff) = app.diff.as_mut() {
            let reply_header = diff
                .display_rows
                .iter()
                .position(|row| {
                    matches!(
                        row,
                        DisplayRow::Comment(CommentRow {
                            comment_id: Some(2),
                            kind: CommentRowKind::Header { .. },
                            ..
                        })
                    )
                })
                .expect("reply header");
            diff.cursor = reply_header;
        }
        // 解決済みとして再取得（自動コラプス。返信は Collapsed 行に畳まれる）。
        let resolved_root_json = r#"{ "id": 1, "content": { "raw": "root" },
                        "user": { "display_name": "Alice" }, "deleted": false,
                        "created_on": "2026-01-01T00:00:00Z",
                        "inline": { "path": "x.rs", "to": 2 },
                        "resolution": {} }"#;
        let resolved_root: Comment = serde_json::from_str(resolved_root_json).expect("valid json");
        let reply2 = make_inline_comment(2, "reply", "x.rs", 2, "2026-01-02T00:00:00Z", Some(1));
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: vec![resolved_root, reply2],
        });
        let diff = app.diff.as_ref().expect("diff");
        // カーソルは同じスレッドのコラプス行に乗っている（diff 行へ落ちない）。
        assert_eq!(diff.cursor_thread(), Some(1), "スレッドの行に留まる");
        assert_eq!(diff.cursor_comment(), Some((1, 1)), "コラプス行を選択");
    }

    #[test]
    fn split_display_rows_include_comment_on_replaced_removed_line() {
        // 置換（-old/+new）。旧側（削除行 old_no=1）にコメント。split でも欠落しない。
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,1 +1,1 @@\n-old\n+new\n");
        let json = r#"{ "id": 7, "content": { "raw": "why" },
                        "user": { "display_name": "A" }, "deleted": false,
                        "created_on": "2026-01-01T00:00:00Z",
                        "inline": { "path": "x.rs", "from": 1 } }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid inline comment json");
        let comment_layout = build_comment_layout(&parsed, &[comment], &Me::default());
        let mut diff = DiffState {
            parsed,
            viewport: 20,
            view_mode: DiffViewMode::Split,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        let has_header = diff.display_rows.iter().any(|row| {
            matches!(
                row,
                DisplayRow::Comment(CommentRow {
                    kind: CommentRowKind::Header { .. },
                    ..
                })
            )
        });
        assert!(
            has_header,
            "split で削除側コメントが欠落: {:?}",
            diff.display_rows
        );
    }

    /// カーソルをスレッドのヘッダに合わせた PR 差分の App を組む（コメント 1 件 id=1、投稿者
    /// Alice。`me` も Alice にして自コメント扱いにする）。
    fn review_app_with_thread() -> App {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.me = Me {
            account_id: None,
            uuid: None,
            display_name: Some("Alice".to_string()),
        };
        app.comments = vec![make_inline_comment(
            1,
            "hi",
            "x.rs",
            2,
            "2026-01-01T00:00:00Z",
            None,
        )];
        let mut diff = diff_with_thread(DiffViewMode::Unified);
        diff.cursor = thread_header_index(&diff);
        app.diff = Some(diff);
        app
    }

    #[test]
    fn diff_reply_on_selected_comment_submits_create_reply() {
        let mut app = review_app_with_thread();
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(
            app.comment_editor
                .as_ref()
                .expect("reply editor open")
                .reply_to,
            Some(1)
        );
        for ch in "了解".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        match app.update(Msg::Key(ctrl(KeyCode::Char('s')))) {
            Command::CreateReply {
                id, parent_id, raw, ..
            } => {
                assert_eq!(id, 9);
                assert_eq!(parent_id, 1);
                assert_eq!(raw, "了解");
            }
            other => panic!("expected CreateReply, got {other:?}"),
        }
    }

    #[test]
    fn diff_edit_on_selected_comment_prefills_and_submits_edit() {
        let mut app = review_app_with_thread();
        app.update(Msg::Key(key(KeyCode::Char('e'))));
        let editor = app.comment_editor.as_ref().expect("edit editor open");
        assert_eq!(editor.editing, Some(1));
        assert_eq!(editor.text, "hi");
        match app.update(Msg::Key(ctrl(KeyCode::Char('s')))) {
            Command::EditComment {
                id,
                comment_id,
                raw,
                ..
            } => {
                assert_eq!(id, 9);
                assert_eq!(comment_id, 1);
                assert_eq!(raw, "hi");
            }
            other => panic!("expected EditComment, got {other:?}"),
        }
    }

    #[test]
    fn diff_delete_on_selected_comment_confirms_then_deletes() {
        let mut app = review_app_with_thread();
        app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert!(app.delete_comment_modal.is_some());
        match app.update(Msg::Key(key(KeyCode::Enter))) {
            Command::DeleteComment { id, comment_id, .. } => {
                assert_eq!(id, 9);
                assert_eq!(comment_id, 1);
            }
            other => panic!("expected DeleteComment, got {other:?}"),
        }
    }

    #[test]
    fn diff_resolve_on_selected_comment_issues_resolve() {
        let mut app = review_app_with_thread();
        match app.update(Msg::Key(key(KeyCode::Char('R')))) {
            Command::ResolveComment {
                id,
                comment_id,
                resolve,
                ..
            } => {
                assert_eq!(id, 9);
                assert_eq!(comment_id, 1);
                assert!(resolve, "未解決スレッドは解決へトグルする");
            }
            other => panic!("expected ResolveComment, got {other:?}"),
        }
    }

    #[test]
    fn diff_reply_on_diff_line_shows_error() {
        let mut app = review_app_with_thread();
        // カーソルを diff 行（先頭）へ戻すとコメント未選択。
        if let Some(diff) = app.diff.as_mut() {
            diff.cursor = 0;
        }
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(app.comment_editor.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn diff_edit_on_others_comment_is_blocked() {
        let mut app = review_app_with_thread();
        // 自分（me）を別人にすると、Alice のコメントは編集不可。
        app.me = Me {
            account_id: None,
            uuid: None,
            display_name: Some("Bob".to_string()),
        };
        app.update(Msg::Key(key(KeyCode::Char('e'))));
        assert!(app.comment_editor.is_none(), "他人のコメントは編集できない");
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn diff_delete_on_others_comment_is_blocked() {
        let mut app = review_app_with_thread();
        app.me = Me {
            account_id: None,
            uuid: None,
            display_name: Some("Bob".to_string()),
        };
        app.update(Msg::Key(key(KeyCode::Char('d'))));
        assert!(
            app.delete_comment_modal.is_none(),
            "他人のコメントは削除できない"
        );
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn cursor_stays_on_diff_line_after_comment_rebuild() {
        // 回帰: 非同期のコメント取得で表示行が増えても、カーソルは同じ diff 行に留まる。
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.update(Msg::DiffLoaded {
            id: 9,
            text: "diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n".to_string(),
        });
        if let Some(diff) = app.diff.as_mut() {
            diff.viewport = 20;
            diff.cursor = 4; // 最後の diff 行（Context c）
        }
        // アンカー行より上（新側 1 行目 = unified 行 2）にコメントが付く。
        app.comments = vec![make_inline_comment(
            1,
            "hi",
            "x.rs",
            1,
            "2026-01-01T00:00:00Z",
            None,
        )];
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: app.comments.clone(),
        });
        let diff = app.diff.as_ref().expect("diff");
        // カーソルは依然として最後の diff 行（Context c、unified 行 4）を指す。
        assert_eq!(diff.cursor_diff_line(), 4);
    }

    #[test]
    fn cursor_stays_on_same_comment_after_comment_rebuild() {
        // 返信/編集後の CommentsLoaded 相当。コメントが残っていればカーソルはそのコメントに留まる。
        let mut app = review_app_with_thread(); // カーソルはコメント id=1 のヘッダ
        app.update(Msg::CommentsLoaded {
            id: 9,
            comments: app.comments.clone(),
        });
        let diff = app.diff.as_ref().expect("diff");
        assert_eq!(diff.cursor_comment(), Some((1, 1)));
    }

    #[test]
    fn cursor_to_bottom_lands_on_last_diff_line_even_with_trailing_comment() {
        // 最終 diff 行にコメントがあっても G は最後の diff 行に着地する（c が使えるように）。
        let parsed = parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,3 +1,3 @@\n a\n b\n c\n");
        // 新側 3 行目（unified 行 4 = 最終 diff 行）にコメント。
        let comment = make_inline_comment(1, "hi", "x.rs", 3, "2026-01-01T00:00:00Z", None);
        let comment_layout = build_comment_layout(&parsed, &[comment], &Me::default());
        let mut diff = DiffState {
            parsed,
            viewport: 20,
            view_mode: DiffViewMode::Unified,
            comment_layout,
            ..Default::default()
        };
        diff.rebuild_display_rows();
        diff.cursor_to_bottom();
        // カーソルは diff 行（コメント不可の枠行ではない）。
        assert_eq!(diff.row_diff_index(diff.cursor), Some(4));
    }

    #[test]
    fn build_comment_layout_orphaned_root_is_not_marked_as_reply() {
        let parsed =
            parse_diff("diff --git a/x.rs b/x.rs\n@@ -1,2 +1,3 @@\n ctx1\n+added\n ctx2\n");
        // parent が取得済みコメント集合に無い（削除/未取得）ため、このコメントがルートに昇格する。
        let orphan = make_inline_comment(5, "body", "x.rs", 2, "2026-01-01T00:00:00Z", Some(99));
        let layout = build_comment_layout(&parsed, &[orphan], &Me::default());
        let threads = layout.threads_by_line.get(&3).expect("thread present");
        assert!(
            !threads[0].comments[0].reply,
            "昇格したルートに返信マーカーを付けない"
        );
    }

    #[test]
    fn ensure_cursor_visible_matches_legacy_formula_without_comments() {
        let parsed = parse_diff(&"a\n".repeat(50));
        let mut diff = DiffState {
            parsed,
            viewport: 10,
            view_mode: DiffViewMode::Unified,
            ..Default::default()
        };
        diff.scroll = 0;
        diff.cursor = 30;
        diff.ensure_cursor_visible();
        // コメント無しでは従来の O(1) 式 scroll = cursor + 1 - viewport = 21 と一致する。
        assert_eq!(diff.scroll, 21);
    }

    #[test]
    fn ensure_cursor_visible_keeps_cursor_within_viewport() {
        let parsed = parse_diff(&"a\n".repeat(30));
        let mut diff = DiffState {
            parsed,
            viewport: 10,
            view_mode: DiffViewMode::Unified,
            ..Default::default()
        };
        // 表示行 = diff 行と 1:1。cursor が下にはみ出したら scroll を合わせる。
        diff.cursor = 25;
        diff.scroll = 5;
        diff.ensure_cursor_visible();
        assert!(diff.scroll <= diff.cursor);
        assert!(diff.cursor < diff.scroll + diff.viewport.max(1));
    }

    #[test]
    fn diff_inline_comment_posted_clears_editor_and_refreshes_comments() {
        let mut app = review_app();
        app.current_pr = Some(make_pr(9, "OPEN"));
        app.screen = Screen::Diff;
        app.diff_return = Screen::PullRequestDetail;
        app.comment_editor = Some(CommentEditor::inline(CommentAnchor {
            path: "x".to_string(),
            side: CommentSide::To,
            line: 1,
        }));

        let cmd = app.update(Msg::CommentPosted { id: 9 });
        assert!(app.comment_editor.is_none());
        assert!(matches!(app.status, Status::Success(_)));
        assert!(matches!(cmd, Command::LoadComments { id: 9, .. }));
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
    fn comment_editor_inserts_and_deletes_at_cursor_with_multibyte() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(2, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        for ch in "あいう".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        // カーソルは char 単位の末尾。
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 3);
        // ← で「う」の前へ → Backspace は直前の「い」を消す。
        app.update(Msg::Key(key(KeyCode::Left)));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        let editor = app.comment_editor.as_ref().expect("editor");
        assert_eq!(editor.text, "あう");
        assert_eq!(editor.cursor, 1);
        // 途中挿入もカーソル位置基準。
        app.update(Msg::Key(key(KeyCode::Char('X'))));
        let editor = app.comment_editor.as_ref().expect("editor");
        assert_eq!(editor.text, "あXう");
        assert_eq!(editor.cursor, 2);
    }

    #[test]
    fn comment_editor_home_end_up_down_move_within_logical_lines() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(2, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        // 2 論理行を入力: "abc\nあい"（カーソルは行 1・列 2 の末尾）。
        for ch in "abc".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        app.update(Msg::Key(key(KeyCode::Enter)));
        for ch in "あい".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let editor = app.comment_editor.as_ref().expect("editor");
        assert_eq!(editor.text, "abc\nあい");
        assert_eq!(editor.cursor_line_col(), (1, 2));
        // Home → 行 1 の行頭（cursor = "abc\n" の 4 char）。
        app.update(Msg::Key(key(KeyCode::Home)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 4);
        // Up → 行 0 列 0。
        app.update(Msg::Key(key(KeyCode::Up)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 0);
        // End → 行 0 の行末。
        app.update(Msg::Key(key(KeyCode::End)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 3);
        // Down → 行 1 へ。列 3 は行長 2 にクランプされ末尾（cursor=6）。
        app.update(Msg::Key(key(KeyCode::Down)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 6);
        // Right は末尾でクランプ、Left で 1 文字戻る。
        app.update(Msg::Key(key(KeyCode::Right)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 6);
        app.update(Msg::Key(key(KeyCode::Left)));
        assert_eq!(app.comment_editor.as_ref().expect("editor").cursor, 5);
    }

    #[test]
    fn comment_editor_edit_prefill_puts_cursor_at_end() {
        let mut app = review_app_with_thread();
        app.update(Msg::Key(key(KeyCode::Char('e'))));
        let editor = app.comment_editor.as_ref().expect("edit editor open");
        assert_eq!(editor.text, "hi");
        assert_eq!(editor.cursor, 2, "edit プリフィルはカーソルを末尾に置く");
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
            page_info: single_page(),
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
            page_info: single_page(),
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
            page_info: single_page(),
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

    // ---- Pipelines のサーバサイド・ページネーション ----

    #[test]
    fn pipelines_next_page_dispatches_load_pipelines_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadPipelines { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    #[test]
    fn pipelines_prev_page_does_nothing_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn pipelines_page_jump_navigates_clamped_to_total_pages() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(1, Some(3), true);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        for ch in "99".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.page_jump.is_none());
        match cmd {
            // 総ページ数(3)でクランプされる。
            Command::LoadPipelines { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    #[test]
    fn pipelines_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(2, None, false);
        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![make_pipeline("{stale}", 1, "IN_PROGRESS", None)],
            page_info: page_info(1, None, true),
        });
        assert!(app.pipelines.items.is_empty());
    }

    #[test]
    fn pipelines_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![make_pipeline("{p1}", 1, "COMPLETED", Some("SUCCESSFUL"))],
            page_info: page_info(1, Some(2), true),
        });

        // 2 ページ目へ移動: 未キャッシュなので一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.pipelines.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPipelines { .. }));

        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![make_pipeline("{p2}", 2, "COMPLETED", Some("FAILED"))],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.pipelines.items[0].uuid, "{p2}");

        // 1 ページ目へ戻る: キャッシュ命中で即座に表示（Loading にならない）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.pipelines.items[0].uuid, "{p1}");
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadPipelines { .. }));
    }

    #[test]
    fn opening_pipelines_from_repositories_starts_at_page_one() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('p'))));
        assert_eq!(app.screen, Screen::Pipelines);
        match cmd {
            Command::LoadPipelines { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    #[test]
    fn reloading_pipelines_reuses_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(2, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        match cmd {
            Command::LoadPipelines { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    /// 自動ポーリングの tick は「現在表示中のページ」を対象にする（全集約には戻らない）。
    #[test]
    fn tick_refreshes_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(3, Some(5), true);
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)]);
        match app.update(Msg::Tick) {
            Command::LoadPipelines { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadPipelines, got {other:?}"),
        }
    }

    /// ページ移動中に古いページ（tick 発行時点のページ）の結果が届いても、現在表示中の
    /// 新しいページを上書きしない（文脈ガード）。
    #[test]
    fn tick_result_for_page_navigated_away_from_is_ignored() {
        let mut app = review_app();
        app.screen = Screen::Pipelines;
        app.pipelines_page_info = page_info(1, Some(2), true);
        app.pipelines
            .set_items(vec![make_pipeline("{p1}", 1, "IN_PROGRESS", None)]);

        // tick が 1 ページ目のリフレッシュを発行した直後に、ユーザーが 2 ページ目へ移動した
        // 状況を模す（tick の応答はまだ届いていない）。
        let tick_cmd = app.update(Msg::Tick);
        assert!(matches!(tick_cmd, Command::LoadPipelines { .. }));
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert_eq!(app.pipelines_page_info.page, 2);

        // 遅れて届いた 1 ページ目（tick 由来）の応答は無視され、2 ページ目の表示を上書きしない。
        app.update(Msg::PipelinesLoaded {
            repo: "widget".to_string(),
            pipelines: vec![make_pipeline("{stale}", 1, "COMPLETED", Some("SUCCESSFUL"))],
            page_info: page_info(1, Some(2), true),
        });
        assert_eq!(app.pipelines_page_info.page, 2);
        assert!(
            app.pipelines
                .items
                .iter()
                .all(|pipeline| pipeline.uuid != "{stale}")
        );
    }

    // ---- Pipelines の戻り先（入って来た画面へ Esc で戻る） ----

    #[test]
    fn pipelines_esc_returns_to_repositories_when_opened_from_repositories() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        app.update(Msg::Key(key(KeyCode::Char('p'))));
        assert_eq!(app.screen, Screen::Pipelines);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
    }

    #[test]
    fn pipelines_esc_returns_to_pull_requests_when_opened_from_pull_requests() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('P'))));
        assert_eq!(app.screen, Screen::Pipelines);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequests);
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

    // ---- ブラウズの戻り先（B: Branches/Source が「入って来た画面」へ戻る） ----

    #[test]
    fn branches_esc_returns_to_repositories_when_opened_from_repositories() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
    }

    #[test]
    fn branches_esc_returns_to_pull_requests_when_opened_from_pull_requests() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn source_root_esc_returns_to_repositories_when_opened_from_repositories() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.source.is_none());
    }

    #[test]
    fn source_root_esc_returns_to_pull_requests_when_opened_from_pull_requests() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.repo_main_branch = Some("trunk".to_string());
        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        // ルートでの Backspace も Esc と同じ経路（`source_up`）を通る。
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn source_opened_from_branches_keeps_original_browse_origin() {
        // Branches の `s` は「ブラウズの戻り先」を更新しない（最初に入って来た画面のままにする）。
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        app.branches.set_items(vec![make_branch("dev", "aaaa1111")]);

        app.update(Msg::Key(key(KeyCode::Char('s'))));
        assert_eq!(app.screen, Screen::Source);

        app.update(Msg::Key(key(KeyCode::Esc)));
        // Branches ではなく、最初にブラウズへ入った PullRequests へ戻る。
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn branches_to_commits_to_commit_detail_esc_steps_one_level_at_a_time() {
        // Branches→Commits→CommitDetail の途中段は「戻り先」を使わず、常に 1 段ずつ戻る。
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        app.branches.set_items(vec![make_branch("dev", "aaaa1111")]);

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Commits);
        app.commits.set_items(vec![make_commit("bbbb2222", "msg")]);

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::CommitDetail);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Commits);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Branches);

        // ここでようやく「ブラウズの戻り先」（PullRequests）が使われる。
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::PullRequests);
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
            page_info: single_page(),
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
            page_info: single_page(),
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
            next: Some("cursor-2".to_string()),
            page: 1,
        });
        assert_eq!(app.commits.items.len(), 2);
        assert_eq!(app.commits_next_url.as_deref(), Some("cursor-2"));
    }

    #[test]
    fn commits_loaded_ignored_for_stale_revision() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.update(Msg::CommitsLoaded {
            revision: Some("other".to_string()),
            commits: vec![make_commit("zzzz9999", "z")],
            next: None,
            page: 1,
        });
        assert!(app.commits.items.is_empty());
    }

    /// 開いた瞬間は先頭ページ（cursor なし・page 1）にリセットされる（前回の残り状態を持ち越さない）。
    #[test]
    fn opening_commits_resets_to_first_page_with_no_cursor() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches
            .set_items(vec![make_branch("main", "aaaa1111")]);
        // 前回のページ移動状態を汚しておく。
        app.commits_page = 5;
        app.commits_next_url = Some("stale".to_string());
        app.commits_page_cursors = vec![None, Some("stale".to_string())];

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Commits);
        assert_eq!(app.commits_page, 1);
        assert!(app.commits_next_url.is_none());
        assert_eq!(app.commits_page_cursors, vec![None]);
        match cmd {
            Command::LoadCommits { cursor, page, .. } => {
                assert!(cursor.is_none());
                assert_eq!(page, 1);
            }
            other => panic!("expected LoadCommits, got {other:?}"),
        }
    }

    /// `]` は現在ページの `next` URL を cursor として次ページを取得し、ページ番号を進める。
    #[test]
    fn commits_next_page_follows_next_url_and_increments_page() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("aaaa1111", "x")],
            next: Some("https://api.example/commits?ctx=abc".to_string()),
            page: 1,
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert_eq!(app.commits_page, 2);
        assert!(app.commits.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        match cmd {
            Command::LoadCommits { cursor, page, .. } => {
                assert_eq!(
                    cursor.as_deref(),
                    Some("https://api.example/commits?ctx=abc")
                );
                assert_eq!(page, 2);
            }
            other => panic!("expected LoadCommits, got {other:?}"),
        }
    }

    /// 次ページ URL が無ければ `]` は何もしない（末尾ページの空振り防止）。
    #[test]
    fn commits_next_page_does_nothing_without_next_url() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("aaaa1111", "x")],
            next: None,
            page: 1,
        });
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert_eq!(app.commits_page, 1);
        assert!(matches!(cmd, Command::None));
    }

    /// `[` は cursor スタックを 1 つ戻し、前ページを再取得する（先頭ページは cursor なし）。
    #[test]
    fn commits_prev_page_pops_cursor_and_returns_to_previous_page() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        // 1 → 2 ページへ進む。
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("aaaa1111", "x")],
            next: Some("cursor-2".to_string()),
            page: 1,
        });
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert_eq!(app.commits_page, 2);
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("bbbb2222", "y")],
            next: Some("cursor-3".to_string()),
            page: 2,
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.commits_page, 1);
        match cmd {
            Command::LoadCommits { cursor, page, .. } => {
                assert!(cursor.is_none());
                assert_eq!(page, 1);
            }
            other => panic!("expected LoadCommits, got {other:?}"),
        }
    }

    /// 先頭ページで `[` は何もしない（cursor スタックを底割れさせない）。
    #[test]
    fn commits_prev_page_does_nothing_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.commits_page, 1);
        assert!(matches!(cmd, Command::None));
    }

    /// ページ移動中に届いた別ページの応答は現在ページと一致しないため反映しない（文脈ガード）。
    #[test]
    fn commits_loaded_ignored_when_page_does_not_match_current() {
        let mut app = review_app();
        app.screen = Screen::Commits;
        app.commits_revision = Some("main".to_string());
        app.commits_page = 2;
        app.update(Msg::CommitsLoaded {
            revision: Some("main".to_string()),
            commits: vec![make_commit("stale111", "old")],
            next: None,
            page: 1,
        });
        assert!(app.commits.items.is_empty());
    }

    /// ページャ表示用 `PageInfo` は総ページ数を持たず、次ページ有無を `next` URL で判定する。
    #[test]
    fn commits_page_info_reflects_page_and_next_url() {
        let mut app = review_app();
        app.commits_page = 3;
        app.commits_next_url = Some("cursor".to_string());
        let info = app.commits_page_info();
        assert_eq!(info.page, 3);
        assert!(info.total_pages.is_none());
        assert!(info.has_next);

        app.commits_next_url = None;
        assert!(!app.commits_page_info().has_next);
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

    /// サブディレクトリを多段で潜っても Backspace/Esc が毎回「直前の親」へ戻り、
    /// 入口画面（`browse_return`）へ抜けないことを固定する回帰テスト。
    /// API が `entry.path` をルートからのフルパスで正しく返すケース（想定どおりの
    /// 実 API 応答）を模す。
    #[test]
    fn source_backspace_walks_up_multi_level_subdirectories_with_full_api_paths() {
        let mut app = review_app();
        app.screen = Screen::Source;

        // ルート → "src" へ潜る。
        let mut root = SourceState {
            reference: "main".to_string(),
            path: String::new(),
            entries: SelectList::default(),
        };
        root.entries
            .set_items(vec![make_src_entry("commit_directory", "src")]);
        app.source = Some(root);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.source.as_ref().expect("source").path, "src");

        // "src" → "src/tui" へ潜る（子エントリの path はフルパス）。
        let entries = app.source.as_mut().expect("source");
        entries
            .entries
            .set_items(vec![make_src_entry("commit_directory", "src/tui")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.source.as_ref().expect("source").path, "src/tui");

        // Backspace で "src" へ（入口画面へは抜けない）。
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.source.as_ref().expect("source").path, "src");

        // さらに Esc でルートへ。
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.source.as_ref().expect("source").path, "");

        // ルートでの Esc で初めて入口画面へ抜ける。
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.source.is_none());
    }

    /// API が子エントリの `path` にリーフ名しか返さない（フルパス前提が崩れる）
    /// ケースでも、`source.path`（自己追跡）+ リーフ名から子パスを合成するため
    /// 階層追跡が壊れず、Backspace/Esc が正しく親ディレクトリへ戻ることを固定する。
    /// （このケースが実 API で起き得るかは未検証だが、起きても壊れないことを保証する
    /// 堅牢化のテスト）
    #[test]
    fn source_backspace_walks_up_when_api_returns_leaf_names_only() {
        let mut app = review_app();
        app.screen = Screen::Source;

        // ルート → "src" へ潜る（ルートではリーフ名==フルパス）。
        let mut root = SourceState {
            reference: "main".to_string(),
            path: String::new(),
            entries: SelectList::default(),
        };
        root.entries
            .set_items(vec![make_src_entry("commit_directory", "src")]);
        app.source = Some(root);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.source.as_ref().expect("source").path, "src");

        // "src" 配下の子エントリがリーフ名のみを返す（フルパス "src/tui" ではなく "tui"）。
        let source = app.source.as_mut().expect("source");
        source
            .entries
            .set_items(vec![make_src_entry("commit_directory", "tui")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        // child_path("src", "tui") == "src/tui" になっていること（API のリーフ名だけを
        // 使わず、自己追跡している現在地と組み合わせて合成している）。
        assert_eq!(app.source.as_ref().expect("source").path, "src/tui");

        // Backspace で "src" へ戻る（"" へ飛んだり入口画面へ抜けたりしない）。
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.screen, Screen::Source);
        assert_eq!(app.source.as_ref().expect("source").path, "src");
    }

    /// ファイルを開く際の子パスも同様に `source.path` + リーフ名から合成される
    /// （API がリーフ名しか返さないケースでも `get_src_file` に正しいフルパスを渡す）。
    #[test]
    fn source_enter_builds_file_path_from_current_dir_and_leaf_name() {
        let mut app = review_app();
        app.screen = Screen::Source;
        let mut state = SourceState {
            reference: "main".to_string(),
            path: "src".to_string(),
            entries: SelectList::default(),
        };
        // API がリーフ名のみを返すケース。
        state
            .entries
            .set_items(vec![make_src_entry("commit_file", "main.rs")]);
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
    fn child_path_combines_parent_and_leaf() {
        assert_eq!(child_path("", "src"), "src");
        assert_eq!(child_path("src", "tui"), "src/tui");
        assert_eq!(child_path("src/tui", "app.rs"), "src/tui/app.rs");
    }

    /// `child_path` の結果に `parent_dir` を適用すると元の `parent` に戻る
    /// （潜る/戻るが可逆であることの回帰確認）。
    #[test]
    fn child_path_and_parent_dir_are_inverse() {
        for (parent, leaf) in [("", "src"), ("src", "tui"), ("src/tui", "app.rs")] {
            let child = child_path(parent, leaf);
            assert_eq!(parent_dir(&child), Some(parent.to_string()));
        }
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

    // ---- SelectList 検索（#1） ----

    #[test]
    fn select_list_filter_narrows_matches_case_insensitively() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["Alpha", "Beta", "Gamma"]);
        list.set_filter("PH".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        assert_eq!(list.selected(), Some(&"Alpha"));
    }

    #[test]
    fn select_list_empty_filter_restores_full_set() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a", "b", "c"]);
        list.set_filter("b".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![1]);
        list.set_filter(String::new(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 1, 2]);
    }

    #[test]
    fn select_list_selection_clamps_to_matches_range() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "a2", "b1", "a3"]);
        list.state.select(Some(3)); // "a3" を選択中。
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        // matches = [0, 1, 3]（a1, a2, a3）。選択位置はこの範囲にクランプされる。
        assert_eq!(list.matches, vec![0, 1, 3]);
        assert!(
            list.state
                .selected()
                .is_some_and(|pos| pos < list.matches.len())
        );
    }

    #[test]
    fn select_list_select_next_prev_operate_within_matches() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "b1", "a2", "b2"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 2]);
        assert_eq!(list.selected(), Some(&"a1"));
        list.select_next();
        assert_eq!(list.selected(), Some(&"a2"));
        // 末尾で停止する（"b1"/"b2" はフィルタ対象外なので飛ばされない）。
        list.select_next();
        assert_eq!(list.selected(), Some(&"a2"));
    }

    #[test]
    fn select_list_set_items_clears_filter() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a", "b"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        list.set_items(vec!["x", "y", "z"]);
        assert!(list.filter.is_empty());
        assert_eq!(list.matches, vec![0, 1, 2]);
    }

    #[test]
    fn select_list_set_items_keep_selection_preserves_identity_matches_for_unfiltered_screens() {
        // pipelines/branches/commits/source 等、検索を使わない画面の既存挙動を保証する回帰テスト。
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items(vec![1, 2, 3]);
        list.state.select(Some(2));
        list.set_items_keep_selection(vec![10, 20]);
        assert_eq!(list.matches, vec![0, 1]);
        assert_eq!(list.state.selected(), Some(1)); // 3 件→2 件でクランプ。
        assert_eq!(list.selected(), Some(&20));
    }

    // ---- fuzzy 検索（nucleo-matcher） ----

    #[test]
    fn select_list_filter_matches_non_contiguous_fuzzy_subsequence() {
        // "bktui" は連続部分文字列ではないが、"bitbucket-tui" の中に b→k→t→u→i の順で
        // （間を飛ばしつつ）部分列として出現するため fuzzy マッチではヒットする
        // （旧・単純部分一致では絶対にマッチしなかった）。
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["bitbucket-tui", "unrelated"]);
        list.set_filter("bktui".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        assert_eq!(list.selected(), Some(&"bitbucket-tui"));
    }

    #[test]
    fn select_list_filter_orders_matches_by_score_descending() {
        // 先頭一致（"apple"）は途中一致（"pineapple"）よりスコアが高く、先に並ぶ。
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["pineapple", "apple", "grape"]);
        list.set_filter("apple".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![1, 0]);
    }

    #[test]
    fn select_list_filter_does_not_panic_on_japanese_and_path_like_candidates() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec![
            "ワークスペース一覧",
            "src/tui/app.rs",
            "foo/bar/baz.rs",
            "",
        ]);
        // 日本語クエリ。
        list.set_filter("覧".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0]);
        // パス区切りを含む部分列クエリ（"app.rs" の非英数字を飛ばした部分列）。
        list.set_filter("apprs".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![1]);
        // 空文字列候補や空クエリでも panic しない。
        list.set_filter(String::new(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 1, 2, 3]);
    }

    // ---- Shift+J/K（10 件/10 行移動、#D） ----

    #[test]
    fn select_list_select_next_by_moves_ten_and_clamps_at_end() {
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items((0..25).collect());
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(10));
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(20));
        // 末尾（インデックス 24）でクランプする。
        list.select_next_by(10);
        assert_eq!(list.state.selected(), Some(24));
    }

    #[test]
    fn select_list_select_prev_by_moves_ten_and_clamps_at_start() {
        let mut list: SelectList<i32> = SelectList::default();
        list.set_items((0..25).collect());
        list.state.select(Some(15));
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), Some(5));
        // 先頭でクランプする（負にはならない）。
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), Some(0));
    }

    #[test]
    fn select_list_select_next_by_respects_filter_matches() {
        let mut list: SelectList<&str> = SelectList::default();
        list.set_items(vec!["a1", "b1", "a2", "b2", "a3"]);
        list.set_filter("a".to_string(), |s: &&str| s.to_string());
        assert_eq!(list.matches, vec![0, 2, 4]);
        list.select_next_by(10);
        // フィルタ後は 3 件しかないため末尾（"a3"）でクランプする。
        assert_eq!(list.selected(), Some(&"a3"));
    }

    #[test]
    fn select_list_select_next_prev_by_are_noop_on_empty_list() {
        let mut list: SelectList<i32> = SelectList::default();
        list.select_next_by(10);
        assert_eq!(list.state.selected(), None);
        list.select_prev_by(10);
        assert_eq!(list.state.selected(), None);
    }

    #[test]
    fn repositories_shift_j_k_move_by_ten_and_clamp() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        let repos: Vec<Repository> = (0..25)
            .map(|i| make_repo(&format!("acme/repo{i}"), None))
            .collect();
        app.repositories.set_items(repos);

        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.repositories.state.selected(), Some(10));
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.repositories.state.selected(), Some(0));
    }

    #[test]
    fn pull_request_detail_shift_j_moves_ten_and_clamps_to_body_end() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        // 本文なし PR: ヘッダ 4 行 + 本文プレースホルダ 1 行 = 5 行。viewport=1 なら上限は 4。
        app.detail_viewport = 1;
        app.current_pr = Some(make_pr(1, "OPEN"));
        app.detail_scroll = 0;

        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.detail_scroll, 4);
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn diff_shift_j_k_move_cursor_by_ten_and_auto_scroll() {
        let mut app = review_app();
        app.screen = Screen::Diff;
        let lines = (0..50)
            .map(|i| format!("+line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let text = format!("diff --git a/x b/x\n@@ -1,1 +1,50 @@\n{lines}\n");
        app.diff = Some(DiffState {
            parsed: parse_diff(&text),
            scroll: 0,
            viewport: 5,
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
        });

        // 現在行が 10 行分下がり、viewport(5) に収まるよう最小限だけ自動スクロールする
        // （cursor=10, viewport=5 → scroll は cursor が最終行になる 6 まで進む）。
        app.update(Msg::Key(key(KeyCode::Char('J'))));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 10);
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 6);
        app.update(Msg::Key(key(KeyCode::Char('K'))));
        assert_eq!(app.diff.as_ref().expect("diff").cursor, 0);
        assert_eq!(app.diff.as_ref().expect("diff").scroll, 0);
    }

    // ---- インクリメンタル検索（App 統合・#1） ----

    #[test]
    fn slash_key_enters_search_editing_and_filters_pull_requests_by_typing() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests
            .set_items(vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")]);

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        assert!(app.search_editing);

        for ch in "PR 2".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        assert_eq!(app.pull_requests.matches.len(), 1);
        assert_eq!(app.pull_requests.selected().map(|pr| pr.id), Some(2));

        app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(!app.search_editing);
        assert_eq!(app.pull_requests.filter, "PR 2");
        // Enter 確定後もフィルタは維持される。
        assert_eq!(app.pull_requests.matches.len(), 1);
    }

    #[test]
    fn esc_during_search_clears_filter_and_restores_full_list() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories.set_items(vec![
            make_repo("acme/foo", None),
            make_repo("acme/bar", None),
        ]);

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        assert_eq!(app.repositories.matches.len(), 1);

        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(!app.search_editing);
        assert!(app.repositories.filter.is_empty());
        assert_eq!(app.repositories.matches.len(), 2);
    }

    #[test]
    fn backspace_during_search_removes_last_filter_char() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![
            Workspace {
                slug: "acme".to_string(),
                name: None,
                uuid: None,
            },
            Workspace {
                slug: "other".to_string(),
                name: None,
                uuid: None,
            },
        ]);
        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        app.update(Msg::Key(key(KeyCode::Char('c'))));
        assert_eq!(app.workspaces.filter, "ac");
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.workspaces.filter, "a");
    }

    // ---- ソート（サーバサイド、#1） ----

    #[test]
    fn repositories_and_pull_requests_sort_default_to_recently_updated() {
        let app = review_app();
        assert_eq!(app.repositories_sort, ListSort::RecentlyUpdated);
        assert_eq!(app.pull_requests_sort, ListSort::RecentlyUpdated);
    }

    #[test]
    fn cycle_repositories_sort_cycles_through_all_four_and_reloads_page_one() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        // 3 ページ目にいても、ソート変更は常に 1 ページ目から取得し直す。
        app.repositories_page_info = page_info(3, Some(5), true);

        let expected = [
            ListSort::LeastRecentlyUpdated,
            ListSort::Newest,
            ListSort::Oldest,
            ListSort::RecentlyUpdated,
        ];
        for sort in expected {
            let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
            assert_eq!(app.repositories_sort, sort);
            match cmd {
                Command::LoadRepositories {
                    sort: cmd_sort,
                    page,
                    ..
                } => {
                    assert_eq!(cmd_sort, sort);
                    assert_eq!(page, 1);
                }
                other => panic!("expected LoadRepositories, got {other:?}"),
            }
        }
    }

    #[test]
    fn cycle_pull_requests_sort_reloads_page_one_preserving_filter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Merged);
        app.pull_requests_page_info = page_info(4, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        match cmd {
            Command::LoadPullRequests {
                sort, page, filter, ..
            } => {
                assert_eq!(sort, ListSort::LeastRecentlyUpdated);
                assert_eq!(page, 1);
                assert_eq!(filter, PrStateFilter::only(PrState::Merged));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn repositories_cache_is_keyed_by_sort_and_does_not_leak_across_sorts() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "zeta");

        // ソート変更: 別ソートの 1 ページ目はキャッシュ未命中 → Loading（一覧クリア）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "alpha");

        // 元のソートへ戻す（4 回巡回して一周）とキャッシュ命中で即時表示される。
        for _ in 0..3 {
            app.update(Msg::Key(key(KeyCode::Char('S'))));
        }
        assert_eq!(app.repositories_sort, ListSort::RecentlyUpdated);
        assert_eq!(app.repositories.items[0].name, "zeta");
        assert_eq!(app.status, Status::Idle);
    }

    // ---- 選択位置の保持（E: j/k 移動中に stale-while-revalidate で先頭へ戻るバグの修正） ----

    #[test]
    fn workspaces_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
                Workspace {
                    slug: "alpha".to_string(),
                    name: None,
                    uuid: None,
                },
                Workspace {
                    slug: "beta".to_string(),
                    name: None,
                    uuid: None,
                },
            ],
            page_info: single_page(),
        });
        // ユーザーが 2 番目（"beta"）を選択した状態で j/k 移動中とみなす。
        app.workspaces.state.select(Some(1));

        // 裏側の再検証（同一ページ）が同じ内容で届く。
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![
                Workspace {
                    slug: "alpha".to_string(),
                    name: None,
                    uuid: None,
                },
                Workspace {
                    slug: "beta".to_string(),
                    name: None,
                    uuid: None,
                },
            ],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、"beta" のまま維持される。
        assert_eq!(
            app.workspaces.selected().map(|w| w.slug.as_str()),
            Some("beta")
        );
    }

    #[test]
    fn repositories_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![
                make_repo("acme/alpha", None),
                make_repo("acme/beta", None),
                make_repo("acme/gamma", None),
            ],
            page_info: single_page(),
        });
        // ユーザーが 2 番目（"beta"）を選択した状態で j/k 移動中とみなす。
        app.repositories.state.select(Some(1));

        // 裏側の再検証（同一 workspace/sort/page）が同じ内容で届く。
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![
                make_repo("acme/alpha", None),
                make_repo("acme/beta", None),
                make_repo("acme/gamma", None),
            ],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、"beta" のまま維持される。
        assert_eq!(
            app.repositories.selected().map(|r| r.full_name.as_str()),
            Some("acme/beta")
        );
    }

    #[test]
    fn repositories_loaded_same_context_revalidation_follows_identity_even_if_order_changes() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None), make_repo("acme/beta", None)],
            page_info: single_page(),
        });
        app.repositories.state.select(Some(1)); // "beta" を選択中。

        // 再検証結果で並び順が入れ替わっても（サーバソート下で他者の更新により順序が変動する
        // ことを想定）、識別子（full_name）で "beta" を追従する。
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/beta", None), make_repo("acme/alpha", None)],
            page_info: single_page(),
        });

        assert_eq!(
            app.repositories.selected().map(|r| r.full_name.as_str()),
            Some("acme/beta")
        );
        assert_eq!(app.repositories.state.selected(), Some(0)); // "beta" は新しい並びで先頭。
    }

    #[test]
    fn repositories_context_change_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None), make_repo("acme/beta", None)],
            page_info: page_info(1, Some(2), true),
        });
        app.repositories.state.select(Some(1));

        // 別ページへ移動（新しい文脈）: 先頭へリセットされる。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/gamma", None)],
            page_info: page_info(2, Some(2), false),
        });

        assert_eq!(app.repositories.state.selected(), Some(0));
    }

    #[test]
    fn pull_requests_loaded_same_context_revalidation_preserves_selection_by_identity() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN"), make_pr(3, "OPEN")],
            page_info: single_page(),
        });
        // ユーザーが PR #2 を選択した状態で j/k 移動中とみなす。
        app.pull_requests.state.select(Some(1));

        // 裏側の再検証（同一 repo/filter/sort/page）が同じ内容で届く。
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN"), make_pr(3, "OPEN")],
            page_info: single_page(),
        });

        // 選択位置が先頭へリセットされず、PR #2 のまま維持される。
        assert_eq!(app.pull_requests.selected().map(|pr| pr.id), Some(2));
    }

    #[test]
    fn pull_requests_context_change_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(1, "OPEN"), make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        app.pull_requests.state.select(Some(1));

        // フィルタ切替（新しい文脈）: 先頭へリセットされる。
        app.update(Msg::Key(key(KeyCode::Char('m'))));
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Merged),
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(9, "MERGED")],
            page_info: single_page(),
        });

        assert_eq!(app.pull_requests.state.selected(), Some(0));
    }

    // ---- ジャンプパレット（#2） ----

    #[test]
    fn ctrl_k_opens_jump_palette_from_any_authenticated_screen() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.jump_palette.is_some());
    }

    #[test]
    fn ctrl_k_does_not_open_on_onboarding_screen() {
        let mut app = app();
        assert_eq!(app.screen, Screen::Onboarding);
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(app.jump_palette.is_none());
    }

    #[test]
    fn ctrl_k_does_not_open_while_another_modal_or_search_is_active() {
        let mut comment_editor_app = review_app();
        comment_editor_app.screen = Screen::PullRequestDetail;
        comment_editor_app.comment_editor = Some(CommentEditor::default());
        comment_editor_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(comment_editor_app.jump_palette.is_none());

        let mut search_app = review_app();
        search_app.screen = Screen::PullRequests;
        search_app.update(Msg::Key(key(KeyCode::Char('/'))));
        search_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(search_app.jump_palette.is_none());
        assert!(search_app.search_editing);
    }

    #[test]
    fn ctrl_k_does_not_open_while_delete_comment_modal_or_page_jump_is_active() {
        let mut delete_app = review_app();
        delete_app.screen = Screen::PullRequestDetail;
        delete_app.delete_comment_modal = Some(DeleteCommentModal {
            comment_id: 1,
            submitting: false,
        });
        delete_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(delete_app.jump_palette.is_none());
        assert!(delete_app.delete_comment_modal.is_some());

        let mut page_jump_app = review_app();
        page_jump_app.screen = Screen::PullRequests;
        page_jump_app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert!(page_jump_app.page_jump.is_some());
        page_jump_app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(page_jump_app.jump_palette.is_none());
        assert!(page_jump_app.page_jump.is_some());
    }

    #[test]
    fn jump_palette_esc_closes_without_navigating() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        assert!(app.jump_palette.is_some());
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::Workspaces);
    }

    #[test]
    fn jump_palette_typing_narrows_candidates() {
        let mut app = review_app();
        app.workspaces.set_items(vec![
            Workspace {
                slug: "acme".to_string(),
                name: Some("Acme".to_string()),
                uuid: None,
            },
            Workspace {
                slug: "other".to_string(),
                name: None,
                uuid: None,
            },
        ]);
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "other".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let palette = app.jump_palette.as_ref().expect("palette open");
        assert!(!palette.entries.matches.is_empty());
        assert!(
            palette
                .entries
                .visible()
                .all(|entry| entry.label.to_lowercase().contains("other"))
        );
    }

    #[test]
    fn jump_palette_backspace_widens_candidates_again() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        app.update(Msg::Key(key(KeyCode::Char('z'))));
        let narrowed = app
            .jump_palette
            .as_ref()
            .expect("open")
            .entries
            .matches
            .len();
        app.update(Msg::Key(key(KeyCode::Backspace)));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        let widened = app
            .jump_palette
            .as_ref()
            .expect("open")
            .entries
            .matches
            .len();
        assert!(widened >= narrowed);
    }

    #[test]
    fn jump_palette_screen_entries_depend_on_navigation_state() {
        let mut app = app();
        app.client = Some(client());
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        let palette = app.jump_palette.as_ref().expect("open");
        assert!(
            !palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::Repositories)))
        );
        app.update(Msg::Key(key(KeyCode::Esc)));

        app.selected_workspace = Some("acme".to_string());
        app.selected_repo = Some("acme/widget".to_string());
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        let palette = app.jump_palette.as_ref().expect("open");
        assert!(
            palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::Repositories)))
        );
        assert!(
            palette
                .entries
                .items
                .iter()
                .any(|entry| matches!(entry.action, JumpAction::Screen(Screen::PullRequests)))
        );
    }

    #[test]
    fn jump_palette_enter_on_screen_entry_switches_screen_only() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        // 先頭候補は常に「ワークスペース一覧へ」。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::Workspaces);
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn jump_palette_enter_on_workspace_entry_calls_jump_to_workspace() {
        let mut app = app();
        app.client = Some(client());
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "acme".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.selected_workspace.as_deref(), Some("acme"));
        assert_eq!(app.screen, Screen::Repositories);
        match cmd {
            Command::LoadRepositories { workspace, .. } => assert_eq!(workspace, "acme"),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn jump_palette_enter_on_repository_entry_calls_enter_repository() {
        let mut app = review_app();
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);
        app.screen = Screen::Repositories;
        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "widget".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.jump_palette.is_none());
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        assert_eq!(app.screen, Screen::PullRequests);
        match cmd {
            Command::LoadPullRequests { repo, .. } => assert_eq!(repo, "widget"),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn opening_pr_detail_records_recent_pr() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(42, "OPEN")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.recent_prs.len(), 1);
        assert_eq!(app.recent_prs[0].pr.id, 42);
        assert_eq!(app.recent_prs[0].repo_full_name, "acme/widget");
    }

    #[test]
    fn jump_palette_enter_on_recent_pr_jumps_across_repositories() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(42, "OPEN")]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequestDetail);

        // 別のリポジトリへ移動したとみなす（PR #42 は元のリポジトリのまま）。
        app.selected_repo = Some("acme/other".to_string());
        app.current_pr = None;
        app.screen = Screen::Repositories;

        app.update(Msg::Key(ctrl(KeyCode::Char('k'))));
        for ch in "42".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        assert!(app.jump_palette.is_none());
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert_eq!(app.selected_repo.as_deref(), Some("acme/widget"));
        assert_eq!(app.current_pr.as_ref().map(|pr| pr.id), Some(42));
        match cmd {
            Command::Batch(cmds) => {
                assert_eq!(cmds.len(), 3);
                assert!(matches!(cmds[0], Command::LoadPrDetail { id: 42, .. }));
                assert!(matches!(cmds[1], Command::LoadDiffStat { id: 42, .. }));
                assert!(matches!(cmds[2], Command::LoadComments { id: 42, .. }));
            }
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn jump_to_pr_resets_author_and_target_filters_only_when_repo_context_changes() {
        // 別リポジトリへのジャンプ: author / target はリポジトリ/ワークスペース依存なので
        // 両方リセット。
        let mut app = review_app();
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.pr_state_filter.target_branch = Some(target("release", false));
        app.jump_to_pr(
            "acme".to_string(),
            "acme/other".to_string(),
            make_pr(5, "OPEN"),
        );
        assert!(app.pr_state_filter.author.is_none());
        assert!(app.pr_state_filter.target_branch.is_none());

        // 同一リポジトリへのジャンプ: 維持する。
        let mut app = review_app();
        app.pr_state_filter.author = Some(author("Alice", "{u-1}"));
        app.pr_state_filter.target_branch = Some(target("release", false));
        app.jump_to_pr(
            "acme".to_string(),
            "acme/widget".to_string(),
            make_pr(5, "OPEN"),
        );
        assert_eq!(app.pr_state_filter.author, Some(author("Alice", "{u-1}")));
        assert_eq!(
            app.pr_state_filter.target_branch,
            Some(target("release", false))
        );
    }

    // ---- キャッシュ（stale-while-revalidate） ----

    #[test]
    fn revisit_cache_evicts_oldest_entry_once_over_capacity() {
        let mut cache: RevisitCache<u32, u32> = RevisitCache::default();
        for i in 0..REVISIT_CACHE_MAX_ENTRIES as u32 {
            cache.insert(i, i);
        }
        assert!(cache.get(&0).is_some());

        // 上限を超える 1 件を追加すると、最も古い(0)が追い出される。
        cache.insert(REVISIT_CACHE_MAX_ENTRIES as u32, 999);
        assert!(cache.get(&0).is_none());
        assert!(cache.get(&1).is_some());
        assert_eq!(cache.get(&(REVISIT_CACHE_MAX_ENTRIES as u32)), Some(&999));
    }

    #[test]
    fn revisit_cache_overwriting_existing_key_does_not_evict() {
        let mut cache: RevisitCache<&str, u32> = RevisitCache::default();
        cache.insert("a", 1);
        cache.insert("a", 2);
        assert_eq!(cache.get(&"a"), Some(&2));
    }

    #[test]
    fn revisit_cache_retain_drops_entries_failing_predicate() {
        let mut cache: RevisitCache<&str, u32> = RevisitCache::default();
        cache.insert("a", 1);
        cache.insert("b", 2);
        cache.retain(|key| *key != "a");
        assert!(cache.get(&"a").is_none());
        assert_eq!(cache.get(&"b"), Some(&2));
    }

    #[test]
    fn revisit_cache_retain_can_match_across_multiple_keys() {
        // `(String, PrStateFilter, u32)` のようなタプルキーで、一部フィールドだけが一致すれば
        // 一括で無効化できること（ページ番号を問わず repo 単位で無効化する用途）。
        let mut cache: RevisitCache<(String, u32), u32> = RevisitCache::default();
        cache.insert(("widget".to_string(), 1), 1);
        cache.insert(("widget".to_string(), 2), 2);
        cache.insert(("other".to_string(), 1), 3);
        cache.retain(|(repo, _page)| repo != "widget");
        assert!(cache.get(&("widget".to_string(), 1)).is_none());
        assert!(cache.get(&("widget".to_string(), 2)).is_none());
        assert_eq!(cache.get(&("other".to_string(), 1)), Some(&3));
    }

    #[test]
    fn revisiting_workspace_shows_cached_repositories_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);

        // 初回入場: キャッシュなし → 一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::Repositories);
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/widget", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items.len(), 1);

        // 別画面を経由して再訪する。
        app.screen = Screen::Workspaces;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 即座に一覧が表示され、Loading にはならない。
        assert_eq!(app.repositories.items.len(), 1);
        assert_eq!(app.status, Status::Idle);
        // それでも裏では再取得コマンドを発行する（stale-while-revalidate）。
        assert!(matches!(cmd, Command::LoadRepositories { .. }));
    }

    #[test]
    fn revisiting_workspace_keeps_current_sort_setting_and_shows_cached_data_for_it() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(vec![Workspace {
            slug: "acme".to_string(),
            name: None,
            uuid: None,
        }]);
        app.update(Msg::Key(key(KeyCode::Enter)));
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: single_page(),
        });

        // ソートを変更（サーバソートなので新しい問い合わせが必要＝キャッシュ未命中）。
        app.update(Msg::Key(key(KeyCode::Char('S'))));
        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: single_page(),
        });
        assert_eq!(app.repositories.items[0].name, "alpha");

        // 別画面 → 再訪: サーバソートは「取得順」のようなクライアント側概念ではなく明示的な
        // 選択なので、再訪してもリセットされず維持される。維持されたソートのキャッシュに
        // 命中して即座に表示される。
        app.screen = Screen::Workspaces;
        app.update(Msg::Key(key(KeyCode::Enter)));

        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.repositories.items[0].name, "alpha");
        assert_eq!(app.status, Status::Idle);
    }

    #[test]
    fn reentering_repository_shows_cached_pull_requests_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", None)]);

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequests);
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items.len(), 1);

        // Repositories へ戻り、同じ repo へ再入場する。
        app.screen = Screen::Repositories;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 即座に表示され、Loading にはならない。
        assert_eq!(app.pull_requests.items.len(), 1);
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn pull_requests_cache_is_keyed_by_filter_and_does_not_leak_across_filters() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });

        // Merged へ切り替え: Open 用キャッシュを誤って使わないこと。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('m'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        match cmd {
            Command::LoadPullRequests { filter, .. } => {
                assert_eq!(filter, PrStateFilter::only(PrState::Merged));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_cache_restore_keeps_current_sort_setting() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            sort: ListSort::RecentlyUpdated,
            prs: vec![make_pr(9, "OPEN")],
            page_info: single_page(),
        });
        app.update(Msg::Key(key(KeyCode::Char('S')))); // LeastRecentlyUpdated へ。
        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::PullRequestsLoaded {
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            sort: ListSort::LeastRecentlyUpdated,
            prs: vec![make_pr(2, "OPEN")],
            page_info: single_page(),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        // 別画面 → 再訪（手動リロードでもキャッシュ命中の経路を通る）。ソート設定は維持される。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('r'))));

        assert_eq!(app.pull_requests_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.pull_requests.items[0].id, 2);
    }

    #[test]
    fn reopening_pr_detail_shows_cached_data_immediately_and_revalidates() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests.set_items(vec![make_pr(7, "OPEN")]);

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(app.diffstat.items.is_empty());
        assert!(app.comments.is_empty());
        assert!(matches!(cmd, Command::Batch(_)));

        app.update(Msg::PrDetailLoaded {
            id: 7,
            pr: Box::new(make_pr(7, "OPEN")),
        });
        app.update(Msg::DiffStatLoaded {
            id: 7,
            entries: vec![make_diffstat_entry("src/lib.rs")],
        });
        app.update(Msg::CommentsLoaded {
            id: 7,
            comments: vec![make_comment(1, "LGTM")],
        });
        assert_eq!(app.diffstat.items.len(), 1);
        assert_eq!(app.comments.len(), 1);

        // 一覧へ戻り、同じ PR を再度開く。
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));

        // キャッシュ命中: 詳細/diffstat/コメントが即座に表示され、Loading にはならない。
        assert_eq!(app.screen, Screen::PullRequestDetail);
        assert_eq!(app.status, Status::Idle);
        assert_eq!(app.diffstat.items.len(), 1);
        assert_eq!(app.comments.len(), 1);
        assert_eq!(app.current_pr.as_ref().map(|pr| pr.id), Some(7));
        // それでも裏では詳細/diffstat/コメントの再取得コマンドを発行する。
        match cmd {
            Command::Batch(cmds) => assert_eq!(cmds.len(), 3),
            other => panic!("expected Batch, got {other:?}"),
        }
    }

    #[test]
    fn review_action_success_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: single_page(),
        });

        // 再訪してキャッシュ命中（Idle）を確認しておく。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert_eq!(app.status, Status::Idle);

        app.current_pr = Some(make_pr(1, "OPEN"));
        app.update(Msg::ReviewActionDone {
            id: 1,
            message: "承認しました".to_string(),
        });

        // 一覧キャッシュが無効化され、再訪すると Loading（キャッシュミス）に戻ること。
        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn merge_success_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(3, "OPEN")],
            page_info: single_page(),
        });

        app.current_pr = Some(make_pr(3, "OPEN"));
        app.merge_modal = Some(MergeModal::new(false));
        app.update(Msg::MergeDone { id: 3 });

        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn comment_posted_invalidates_pull_requests_cache_for_current_repo() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(5, "OPEN")],
            page_info: single_page(),
        });

        app.current_pr = Some(make_pr(5, "OPEN"));
        app.comment_editor = Some(CommentEditor::default());
        app.update(Msg::CommentPosted { id: 5 });

        app.screen = Screen::Repositories;
        app.screen = Screen::PullRequests;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    // ---- サーバサイド・ページネーション（1 ページ 40 件） ----

    #[test]
    fn repositories_next_page_dispatches_load_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadRepositories { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn repositories_next_page_is_noop_when_has_next_is_false() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(3, Some(3), false);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.repositories_page_info.page, 3);
    }

    #[test]
    fn repositories_prev_page_is_noop_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert!(matches!(cmd, Command::None));
        assert_eq!(app.repositories_page_info.page, 1);
    }

    #[test]
    fn repositories_prev_page_dispatches_load_with_decremented_page() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(2, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        match cmd {
            Command::LoadRepositories { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn pull_requests_next_page_preserves_current_filter() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Merged);
        app.pull_requests_page_info = page_info(2, Some(5), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadPullRequests { page, filter, .. } => {
                assert_eq!(page, 3);
                assert_eq!(filter, PrStateFilter::only(PrState::Merged));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn workspaces_next_page_dispatches_load_workspaces_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces_page_info = page_info(1, Some(2), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadWorkspaces { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadWorkspaces, got {other:?}"),
        }
    }

    #[test]
    fn page_nav_keys_do_nothing_on_non_paged_screens() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(1, "OPEN"));
        assert!(matches!(
            app.update(Msg::Key(key(KeyCode::Char(']')))),
            Command::None
        ));
        assert!(matches!(
            app.update(Msg::Key(key(KeyCode::Char('[')))),
            Command::None
        ));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_prompt_opens_with_g_and_closes_with_esc() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        let cmd = app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert!(matches!(cmd, Command::None));
        assert!(app.page_jump.is_some());
        app.update(Msg::Key(key(KeyCode::Esc)));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_does_not_open_on_non_paged_screen() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(1, "OPEN"));
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn page_jump_digit_input_and_enter_navigates_clamped_to_total_pages() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(1, Some(3), true);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        for ch in "99".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        assert_eq!(app.page_jump.as_ref().expect("open").input, "99");

        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.page_jump.is_none());
        match cmd {
            // 総ページ数(3)でクランプされる。
            Command::LoadRepositories { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
    }

    #[test]
    fn page_jump_backspace_removes_last_digit() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        app.update(Msg::Key(key(KeyCode::Char('1'))));
        app.update(Msg::Key(key(KeyCode::Char('2'))));
        app.update(Msg::Key(key(KeyCode::Backspace)));
        assert_eq!(app.page_jump.as_ref().expect("open").input, "1");
    }

    #[test]
    fn page_jump_ignores_non_digit_characters() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        app.update(Msg::Key(key(KeyCode::Char('a'))));
        assert_eq!(app.page_jump.as_ref().expect("open").input, "");
    }

    #[test]
    fn page_jump_enter_with_empty_input_reports_error_and_closes_modal() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(matches!(cmd, Command::None));
        assert!(app.page_jump.is_none());
        assert!(matches!(app.status, Status::Error(_)));
    }

    #[test]
    fn goto_page_clamps_below_one_to_one_when_total_unknown() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pull_requests_page_info = page_info(2, None, true);
        let cmd = app.goto_page(0);
        match cmd {
            Command::LoadPullRequests { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn workspaces_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Workspaces;
        app.workspaces_page_info = page_info(2, None, false);
        app.update(Msg::WorkspacesLoaded {
            workspaces: vec![Workspace {
                slug: "stale".to_string(),
                name: None,
                uuid: None,
            }],
            page_info: page_info(1, None, true),
        });
        assert!(app.workspaces.items.is_empty());
    }

    #[test]
    fn repositories_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories_page_info = page_info(2, None, false);
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/stale", None)],
            page_info: page_info(1, None, true),
        });
        assert!(app.repositories.items.is_empty());
    }

    #[test]
    fn pull_requests_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.pull_requests_page_info = page_info(2, None, false);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: page_info(1, None, true),
        });
        assert!(app.pull_requests.items.is_empty());
    }

    #[test]
    fn repositories_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/foo", None)],
            page_info: page_info(1, Some(2), true),
        });

        // 2 ページ目へ移動: 未キャッシュなので一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.repositories.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadRepositories { .. }));

        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/bar", None)],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.repositories.items[0].name, "bar");

        // 1 ページ目へ戻る: キャッシュ命中で即座に表示（Loading にならない）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.repositories.items[0].name, "foo");
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadRepositories { .. }));
    }

    #[test]
    fn pull_requests_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(1, "OPEN")],
            page_info: page_info(1, Some(2), true),
        });

        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.pull_requests.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));

        app.update(Msg::PullRequestsLoaded {
            sort: ListSort::RecentlyUpdated,
            repo: "widget".to_string(),
            filter: PrStateFilter::only(PrState::Open),
            prs: vec![make_pr(2, "OPEN")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.pull_requests.items[0].id, 2);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.pull_requests.items[0].id, 1);
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadPullRequests { .. }));
    }

    #[test]
    fn changing_page_clears_confirmed_search_filter() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            sort: ListSort::RecentlyUpdated,
            workspace: "acme".to_string(),
            repos: vec![make_repo("acme/foo", None), make_repo("acme/bar", None)],
            page_info: page_info(1, Some(2), true),
        });

        app.update(Msg::Key(key(KeyCode::Char('/'))));
        app.update(Msg::Key(key(KeyCode::Char('f'))));
        app.update(Msg::Key(key(KeyCode::Enter))); // 検索確定（Enter 後もフィルタは維持される）。
        assert_eq!(app.repositories.filter, "f");

        // ページ移動: 表示される 40 件が入れ替わるため、前ページの検索フィルタは引き継がない。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.repositories.filter.is_empty());
    }

    #[test]
    fn changing_page_preserves_current_sort_setting() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::RecentlyUpdated,
            repos: vec![make_repo("acme/zeta", None)],
            page_info: page_info(1, Some(2), true),
        });
        app.update(Msg::Key(key(KeyCode::Char('S')))); // LeastRecentlyUpdated へ（1 ページ目に戻る）。
        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/alpha", None)],
            page_info: page_info(1, Some(2), true),
        });

        // ページ移動（`]`）はソートを変えない。次ページの取得も同じソートで発行される。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadRepositories { sort, page, .. } => {
                assert_eq!(sort, ListSort::LeastRecentlyUpdated);
                assert_eq!(page, 2);
            }
            other => panic!("expected LoadRepositories, got {other:?}"),
        }
        app.update(Msg::RepositoriesLoaded {
            workspace: "acme".to_string(),
            sort: ListSort::LeastRecentlyUpdated,
            repos: vec![make_repo("acme/gamma", None)],
            page_info: page_info(2, Some(2), false),
        });

        assert_eq!(app.repositories_sort, ListSort::LeastRecentlyUpdated);
        assert_eq!(app.repositories_page_info.page, 2);
    }

    #[test]
    fn switching_pr_filter_resets_to_page_one() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.pull_requests_page_info = page_info(3, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('m')))); // Merged へ切替
        match cmd {
            Command::LoadPullRequests { page, filter, .. } => {
                assert_eq!(page, 1);
                assert_eq!(filter, PrStateFilter::only(PrState::Merged));
            }
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    #[test]
    fn reload_key_reuses_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::PullRequests;
        app.pr_state_filter = PrStateFilter::only(PrState::Open);
        app.pull_requests_page_info = page_info(2, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        match cmd {
            Command::LoadPullRequests { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadPullRequests, got {other:?}"),
        }
    }

    // ---- Branches のサーバサイド・ページネーション ----

    #[test]
    fn branches_next_page_dispatches_load_branches_with_incremented_page() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn branches_prev_page_does_nothing_on_first_page() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert!(matches!(cmd, Command::None));
    }

    #[test]
    fn branches_page_jump_navigates_clamped_to_total_pages() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(3), true);
        app.update(Msg::Key(key(KeyCode::Char('g'))));
        for ch in "99".chars() {
            app.update(Msg::Key(key(KeyCode::Char(ch))));
        }
        let cmd = app.update(Msg::Key(key(KeyCode::Enter)));
        assert!(app.page_jump.is_none());
        match cmd {
            // 総ページ数(3)でクランプされる。
            Command::LoadBranches { page, .. } => assert_eq!(page, 3),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn branches_loaded_ignored_when_page_does_not_match_current_request() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(2, None, false);
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("stale", "aaaa")],
            page_info: page_info(1, None, true),
        });
        assert!(app.branches.items.is_empty());
    }

    #[test]
    fn branches_cache_is_keyed_by_page_and_does_not_leak_across_pages() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("main", "aaaa1111")],
            page_info: page_info(1, Some(2), true),
        });

        // 2 ページ目へ移動: 未キャッシュなので一覧クリア + Loading。
        let cmd = app.update(Msg::Key(key(KeyCode::Char(']'))));
        assert!(app.branches.items.is_empty());
        assert!(matches!(app.status, Status::Loading(_)));
        assert!(matches!(cmd, Command::LoadBranches { .. }));

        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("dev", "bbbb2222")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.branches.items[0].name_str(), "dev");

        // 1 ページ目へ戻る: キャッシュ命中で即座に表示（Loading にならない）。
        let cmd = app.update(Msg::Key(key(KeyCode::Char('['))));
        assert_eq!(app.branches.items[0].name_str(), "main");
        assert_eq!(app.status, Status::Idle);
        assert!(matches!(cmd, Command::LoadBranches { .. }));
    }

    #[test]
    fn branches_revalidation_in_same_context_keeps_selection_by_name() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![
                make_branch("main", "aaaa1111"),
                make_branch("dev", "bbbb2222"),
            ],
            page_info: single_page(),
        });
        app.branches.state.select(Some(1)); // "dev" を選択中。

        // 同一文脈（同じ repo/ページ）の再検証: 順序が変わっても "dev" への選択を維持する。
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![
                make_branch("dev", "bbbb2222"),
                make_branch("main", "aaaa1111"),
            ],
            page_info: single_page(),
        });
        assert_eq!(
            app.branches.selected().map(|branch| branch.name_str()),
            Some("dev")
        );
    }

    #[test]
    fn branches_new_navigation_resets_selection_to_top() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(1, Some(2), true);
        app.branches.set_items(vec![
            make_branch("main", "aaaa1111"),
            make_branch("dev", "bbbb2222"),
        ]);
        app.branches.state.select(Some(1));

        // 新規ナビゲーション（ページ変更）: 選択は先頭にリセットされる。
        app.update(Msg::Key(key(KeyCode::Char(']'))));
        app.update(Msg::BranchesLoaded {
            repo: "widget".to_string(),
            branches: vec![make_branch("feature/x", "cccc3333")],
            page_info: page_info(2, Some(2), false),
        });
        assert_eq!(app.branches.state.selected(), Some(0));
    }

    #[test]
    fn opening_branches_from_repositories_starts_at_page_one() {
        let mut app = review_app();
        app.selected_repo = None;
        app.screen = Screen::Repositories;
        app.repositories
            .set_items(vec![make_repo("acme/widget", Some("main"))]);
        let cmd = app.update(Msg::Key(key(KeyCode::Char('b'))));
        assert_eq!(app.screen, Screen::Branches);
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 1),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn reloading_branches_reuses_current_page_not_page_one() {
        let mut app = review_app();
        app.screen = Screen::Branches;
        app.branches_page_info = page_info(2, Some(5), true);

        let cmd = app.update(Msg::Key(key(KeyCode::Char('r'))));
        match cmd {
            Command::LoadBranches { page, .. } => assert_eq!(page, 2),
            other => panic!("expected LoadBranches, got {other:?}"),
        }
    }

    #[test]
    fn mouse_hit_testing_uses_half_open_pane_boundaries() {
        let area = Rect::new(4, 7, 10, 3);
        assert!(rect_contains(area, (4, 7)));
        assert!(rect_contains(area, (13, 9)));
        assert!(!rect_contains(area, (14, 9)));
        assert!(!rect_contains(area, (13, 10)));
    }

    #[test]
    fn overview_link_hit_corrects_for_scroll_offset() {
        let mut app = app();
        app.detail_scroll = 5;
        app.layout.overview_content = Some(Rect::new(10, 20, 30, 4));
        app.overview_link_positions = vec![LinkPosition {
            visual_line: 6,
            column_range: 2..8,
            urls: vec!["https://example.com/target".to_string()],
        }];

        assert_eq!(
            app.overview_urls_at((13, 21)),
            Some(vec!["https://example.com/target".to_string()])
        );
        assert_eq!(app.overview_urls_at((13, 20)), None);
    }

    #[test]
    fn overview_multi_link_fallback_click_opens_palette() {
        let mut app = app();
        app.screen = Screen::PullRequestDetail;
        app.layout.overview_content = Some(Rect::new(10, 20, 30, 4));
        app.overview_link_positions = vec![LinkPosition {
            visual_line: 0,
            column_range: 0..30,
            urls: vec![
                "https://example.com/one".to_string(),
                "https://example.com/two".to_string(),
            ],
        }];

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            13,
            20,
        )));
        let palette = app.link_palette.as_ref().expect("palette opens");
        assert_eq!(palette.links.items.len(), 2);
    }

    #[test]
    fn list_row_click_selects_then_second_click_runs_enter_handler() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories.set_items(vec![
            make_repo("acme/one", Some("main")),
            make_repo("acme/two", Some("main")),
        ]);
        app.layout.lists.push(ListLayout {
            kind: ListKind::Repositories,
            area: Rect::new(2, 10, 30, 2),
            first_visible: 0,
        });

        let first = app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            3,
            11,
        )));
        assert!(matches!(first, Command::None));
        assert_eq!(app.repositories.state.selected(), Some(1));
        assert_eq!(app.screen, Screen::Repositories);

        let second = app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            3,
            11,
        )));
        assert!(matches!(second, Command::LoadPullRequests { .. }));
        assert_eq!(app.screen, Screen::PullRequests);
    }

    #[test]
    fn click_on_blank_space_below_list_rows_is_inert() {
        let mut app = review_app();
        app.screen = Screen::Repositories;
        app.repositories.set_items(vec![
            make_repo("acme/one", Some("main")),
            make_repo("acme/two", Some("main")),
        ]);
        app.layout.lists.push(ListLayout {
            kind: ListKind::Repositories,
            area: Rect::new(2, 10, 30, 5),
            first_visible: 0,
        });

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            3,
            14,
        )));
        assert_eq!(app.repositories.state.selected(), Some(0));
        assert_eq!(app.screen, Screen::Repositories);
    }

    #[test]
    fn click_outside_modal_runs_escape_equivalent() {
        let mut app = app();
        app.screen = Screen::Repositories;
        app.page_jump = Some(PageJumpModal::default());
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::PageJump,
            area: Rect::new(10, 10, 20, 8),
        });

        app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            2,
            2,
        )));
        assert!(app.page_jump.is_none());
    }

    #[test]
    fn confirmation_modal_is_never_decided_by_inside_click() {
        let mut app = review_app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(42, "OPEN"));
        app.merge_modal = Some(MergeModal::new(true));
        app.layout.modal = Some(ModalLayout {
            kind: ModalKind::MergeConfirm,
            area: Rect::new(10, 10, 30, 12),
        });

        let command = app.update(Msg::Mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            20,
            15,
        )));
        assert!(matches!(command, Command::None));
        let modal = app.merge_modal.as_ref().expect("modal remains open");
        assert!(!modal.submitting);
    }

    #[test]
    fn mouse_wheel_scrolls_three_lines_in_pane_under_cursor() {
        let mut app = app();
        app.screen = Screen::PullRequestDetail;
        app.current_pr = Some(make_pr(1, "OPEN"));
        app.detail_body_rendered_lines = Some(20);
        app.detail_viewport = 5;
        app.layout
            .panes
            .push((PaneKind::Overview, Rect::new(0, 0, 40, 10)));

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown, 5, 5)));
        assert_eq!(app.detail_scroll, 3);
        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollUp, 5, 5)));
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn mouse_wheel_moves_list_selection_three_rows() {
        let mut app = app();
        app.screen = Screen::Workspaces;
        app.workspaces.set_items(
            (0..6)
                .map(|index| Workspace {
                    slug: format!("ws-{index}"),
                    name: None,
                    uuid: None,
                })
                .collect(),
        );
        app.layout.lists.push(ListLayout {
            kind: ListKind::Workspaces,
            area: Rect::new(0, 0, 40, 6),
            first_visible: 0,
        });

        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollDown, 5, 2)));
        assert_eq!(app.workspaces.state.selected(), Some(3));
        app.update(Msg::Mouse(mouse(MouseEventKind::ScrollUp, 5, 2)));
        assert_eq!(app.workspaces.state.selected(), Some(0));
    }
}

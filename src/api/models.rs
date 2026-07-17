//! Bitbucket REST API 2.0 の最小モデル（serde）。
//!
//! M0 では認証検証・ワークスペース一覧・リポジトリ一覧に必要なフィールドのみを持つ。
//! 将来の互換性のため、レスポンスに未知フィールドがあっても失敗しないよう
//! （serde はデフォルトで未知フィールドを無視する）、必須でない項目は `Option` にする。

use serde::{Deserialize, Serialize};

/// Bitbucket のページングレスポンス共通形。
///
/// `{ "values": [...], "next": "<url>", "page": .., "size": .., "pagelen": .. }`
///
/// M0 では `values` と `next` のみ使用するが、メタ情報も後続マイルストーン向けに保持する。
#[derive(Debug, Clone, Deserialize)]
pub struct Paginated<T> {
    #[serde(default = "Vec::new")]
    pub values: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
    /// 現在ページ番号（1 始まり）。単一ページ取得（[`PageInfo::from_paginated`]）で使用する。
    #[serde(default)]
    pub page: Option<u32>,
    /// 総件数。単一ページ取得の総ページ数算出（[`PageInfo::from_paginated`]）で使用する。
    /// 応答によっては省略され得る。
    #[serde(default)]
    pub size: Option<u32>,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "1 ページの件数はリクエスト側の固定値として扱うため未使用"
    )]
    pub pagelen: Option<u32>,
}

/// 単一ページ取得の結果に付随するページ情報。
///
/// `page`: 現在ページ（1 始まり）。`total_pages`: `size`（総件数）が判明していれば
/// `ceil(size / page_size)`、レスポンスで `size` が省略されていれば `None`。
/// `has_next`: レスポンスの `next` の有無（次ページが存在するか）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageInfo {
    pub page: u32,
    pub total_pages: Option<u32>,
    pub has_next: bool,
}

impl Default for PageInfo {
    /// 未取得時の初期値（1 ページ目・総数不明・次ページなし）。
    fn default() -> Self {
        Self {
            page: 1,
            total_pages: None,
            has_next: false,
        }
    }
}

impl PageInfo {
    /// `Paginated<T>` から算出する。`page` は応答の `page` を優先し、無ければ
    /// リクエスト時の `requested_page` にフォールバックする。
    pub fn from_paginated<T>(
        paginated: &Paginated<T>,
        requested_page: u32,
        page_size: u32,
    ) -> Self {
        Self {
            page: paginated.page.unwrap_or(requested_page),
            total_pages: paginated.size.map(|size| size.div_ceil(page_size.max(1))),
            has_next: paginated.next.is_some(),
        }
    }
}

/// Repositories / PullRequests 一覧のサーバサイドソート順（`S` キーで巡回する）。
///
/// Bitbucket のブラウザ版と同じ 4 種類を提供する。クライアント側での並び替えはせず、
/// `sort` クエリとしてそのままサーバへ渡す（[`ListSort::query_value`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ListSort {
    /// 更新が新しい順（`sort=-updated_on`）。既定。
    #[default]
    RecentlyUpdated,
    /// 更新が古い順（`sort=updated_on`）。
    LeastRecentlyUpdated,
    /// 作成が新しい順（`sort=-created_on`）。
    Newest,
    /// 作成が古い順（`sort=created_on`）。
    Oldest,
}

impl ListSort {
    /// 次のソートへ巡回する（末尾の次は先頭に戻る）。
    pub fn next(self) -> ListSort {
        match self {
            ListSort::RecentlyUpdated => ListSort::LeastRecentlyUpdated,
            ListSort::LeastRecentlyUpdated => ListSort::Newest,
            ListSort::Newest => ListSort::Oldest,
            ListSort::Oldest => ListSort::RecentlyUpdated,
        }
    }

    /// API へ渡す `sort` クエリ値。
    pub fn query_value(self) -> &'static str {
        match self {
            ListSort::RecentlyUpdated => "-updated_on",
            ListSort::LeastRecentlyUpdated => "updated_on",
            ListSort::Newest => "-created_on",
            ListSort::Oldest => "created_on",
        }
    }

    /// UI 表示ラベル。
    pub fn label(self) -> &'static str {
        match self {
            ListSort::RecentlyUpdated => "更新が新しい順",
            ListSort::LeastRecentlyUpdated => "更新が古い順",
            ListSort::Newest => "作成が新しい順",
            ListSort::Oldest => "作成が古い順",
        }
    }
}

/// `GET /2.0/user` の応答（認証検証に使用）。
///
/// M0 では `display_name` のみ使用。他フィールドは後続で利用予定のため保持する。
#[derive(Debug, Clone, Deserialize)]
pub struct User {
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降のユーザー識別で使用予定")]
    pub uuid: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降のユーザー識別で使用予定")]
    pub account_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降の表示で使用予定")]
    pub nickname: Option<String>,
}

/// ワークスペース。`GET /2.0/user/workspaces` の各要素が内包する `workspace`。
///
/// 旧 `GET /2.0/workspaces` は `name` 付きだったが、`CHANGE-2770` で廃止された。後継の
/// `/2.0/user/workspaces` が返す `workspace_base` は `slug`/`uuid` のみで `name` を持たない。
/// そのため `name` は任意扱いとし、表示は `display_name()`（`name` が無ければ `slug`）で行う。
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub slug: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降の識別で使用予定")]
    pub uuid: Option<String>,
}

impl Workspace {
    /// 表示名。`name` があればそれを、無ければ `slug` を返す。
    pub fn display_name(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.slug)
    }
}

/// `GET /2.0/user/workspaces` の要素（ワークスペースメンバーシップ）。
///
/// 実体のワークスペースは `workspace` に入る（`{ "type": "workspace_access", "workspace": {..} }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceMembership {
    pub workspace: Workspace,
}

/// `GET /2.0/workspaces/{workspace}/members` の要素（メンバーシップ）。
///
/// 実体のユーザーは `user` に入る（`{ "type": "workspace_membership", "user": {..} }`）。
/// 応答形（`values[].user` の uuid/display_name）は未検証の仮定（`docs/LEDGER.md` 参照）。
#[derive(Debug, Clone, Deserialize)]
pub struct WorkspaceMember {
    pub user: User,
}

/// `GET /2.0/repositories/{workspace}` の要素。
#[derive(Debug, Clone, Deserialize)]
pub struct Repository {
    pub full_name: String,
    pub name: String,
    #[serde(default)]
    pub updated_on: Option<String>,
    #[serde(default)]
    pub is_private: bool,
    /// 既定ブランチ（`{ "type": "branch", "name": "main" }`）。M3 の Source ルートに使う。
    #[serde(default)]
    pub mainbranch: Option<Branch>,
}

impl Repository {
    /// 既定ブランチ名（`mainbranch.name`）。
    pub fn main_branch_name(&self) -> Option<&str> {
        self.mainbranch
            .as_ref()
            .and_then(|branch| branch.name.as_deref())
    }
}

/// PR の source / destination を表すブランチ参照。
///
/// `{ "branch": { "name": ".." }, "commit": { "hash": ".." } }`。実 API 応答での有無が
/// 未確定のため、内部フィールドはすべて `Option`。
#[derive(Debug, Clone, Deserialize)]
pub struct BranchRef {
    #[serde(default)]
    pub branch: Option<Branch>,
    #[serde(default)]
    #[allow(dead_code, reason = "コミット hash 表示で使用予定")]
    pub commit: Option<Commit>,
}

/// ブランチ。PR の source/destination では `name` のみ、M3 のブランチ一覧では
/// 最終コミット（`target`）も利用する。
#[derive(Debug, Clone, Deserialize)]
pub struct Branch {
    #[serde(default)]
    pub name: Option<String>,
    /// 最終コミット（ブランチが指す先）。`refs/branches` 応答で付与される。
    #[serde(default)]
    pub target: Option<Commit>,
}

impl Branch {
    /// ブランチ名（無ければ `?`）。
    pub fn name_str(&self) -> &str {
        self.name.as_deref().unwrap_or("?")
    }

    /// 最終コミットの短縮 hash。
    pub fn target_short_hash(&self) -> String {
        self.target
            .as_ref()
            .map(Commit::short_hash)
            .unwrap_or_else(|| "?".to_string())
    }

    /// 最終コミットの日時（無ければ空文字）。
    pub fn target_date(&self) -> &str {
        self.target.as_ref().map(Commit::date_str).unwrap_or("")
    }

    /// 最終コミットメッセージの 1 行目（概要）。
    pub fn target_summary(&self) -> &str {
        self.target.as_ref().map(Commit::summary).unwrap_or("")
    }
}

/// コミット作者（`{ "raw": "Name <email>", "user": {..} }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommitAuthor {
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    pub user: Option<User>,
}

/// コミット。PR/pipeline では `hash` のみ、M3 のコミット履歴/詳細では
/// message/date/author/parents も利用する。id/hash 以外は `Option`/`#[serde(default)]`。
#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    #[serde(default)]
    pub hash: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub author: Option<CommitAuthor>,
    /// 親コミット（マージコミットは複数）。表示は hash のみ。
    #[serde(default)]
    pub parents: Vec<Commit>,
}

impl Commit {
    /// hash 全体（無ければ `?`）。
    pub fn hash_str(&self) -> &str {
        self.hash.as_deref().unwrap_or("?")
    }

    /// 短縮 hash（先頭 8 文字、無ければ `?`）。
    pub fn short_hash(&self) -> String {
        match self.hash.as_deref() {
            Some(hash) => hash.chars().take(8).collect(),
            None => "?".to_string(),
        }
    }

    /// メッセージ全文（無ければ空文字）。
    pub fn message_str(&self) -> &str {
        self.message.as_deref().unwrap_or("")
    }

    /// メッセージの 1 行目（概要）。
    pub fn summary(&self) -> &str {
        self.message
            .as_deref()
            .and_then(|message| message.lines().next())
            .unwrap_or("(メッセージなし)")
    }

    /// コミット日時（無ければ空文字）。
    pub fn date_str(&self) -> &str {
        self.date.as_deref().unwrap_or("")
    }

    /// 作者の表示名（`user.display_name` 優先、無ければ `raw`、無ければ `?`）。
    pub fn author_name(&self) -> &str {
        if let Some(author) = &self.author {
            if let Some(name) = author
                .user
                .as_ref()
                .and_then(|user| user.display_name.as_deref())
            {
                return name;
            }
            if let Some(raw) = author.raw.as_deref() {
                return raw;
            }
        }
        "?"
    }

    /// 親コミットの短縮 hash 一覧。
    pub fn parent_short_hashes(&self) -> Vec<String> {
        self.parents.iter().map(Commit::short_hash).collect()
    }
}

/// `GET .../src/{commit}/{path}` がディレクトリのとき返す列挙エントリ。
///
/// `type`（`commit_directory`/`commit_file`）でディレクトリ/ファイルを判定する。
/// フィールドは実 API で要検証のため `Option`/`#[serde(default)]` で耐性を持たせる。
#[derive(Debug, Clone, Deserialize)]
pub struct SrcEntry {
    #[serde(rename = "type", default)]
    pub entry_type: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub mimetype: Option<String>,
}

impl SrcEntry {
    /// ディレクトリか（`type == "commit_directory"`）。
    pub fn is_dir(&self) -> bool {
        self.entry_type.as_deref() == Some("commit_directory")
    }

    /// API が返した `path` の生値（無ければ空文字）。
    ///
    /// 名目上はリポジトリルートからのフルパスだが、実 API で未検証の仮定
    /// （`docs/LEDGER.md`）。ディレクトリ階層の追跡（潜る/親へ戻る）にはこの値を
    /// 直接使わず、TUI 自身が管理する現在地 + [`SrcEntry::name`] から合成した
    /// パスを使うこと（`tui/app.rs` の `child_path`/`SourceState::path` 参照）。
    pub fn path_str(&self) -> &str {
        self.path.as_deref().unwrap_or("")
    }

    /// 末尾セグメント（表示名）。
    pub fn name(&self) -> &str {
        let path = self.path_str().trim_end_matches('/');
        match path.rsplit('/').next() {
            Some(name) if !name.is_empty() => name,
            _ => path,
        }
    }
}

/// Bitbucket の「レンダリング済みテキスト」共通形（`{ "raw": .., "html": .. }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct RenderedText {
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "HTML 表示は未対応（raw を利用）")]
    pub html: Option<String>,
}

/// PR の参加者（レビュアー/参加者）。
///
/// `approved`（承認済みか）と `state`（`approved`/`changes_requested`/null）を持つ。
/// フィールド名・値は実 API で要検証。
#[derive(Debug, Clone, Deserialize)]
pub struct Participant {
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub approved: bool,
    #[serde(default)]
    pub state: Option<String>,
}

/// `links` 内の 1 リンク（`{ "href": ".." }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct Link {
    #[serde(default)]
    pub href: Option<String>,
}

/// PR の `links`（ブラウザ表示用の `html` のみ使用。他種別は将来のため無視）。
#[derive(Debug, Clone, Deserialize)]
pub struct PrLinks {
    #[serde(default)]
    pub html: Option<Link>,
}

/// `GET /repositories/{ws}/{repo}/pullrequests/{id}` の PR。
///
/// 一覧・詳細で共通に使う。実 API 応答でのフィールド有無が未確定のため、`id` 以外は
/// すべて `Option`/`#[serde(default)]` で耐性を持たせる。
#[derive(Debug, Clone, Deserialize)]
pub struct PullRequest {
    pub id: u64,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<User>,
    #[serde(default)]
    pub source: Option<BranchRef>,
    #[serde(default)]
    pub destination: Option<BranchRef>,
    #[serde(default)]
    #[allow(dead_code, reason = "作成日時の表示は updated_on を優先")]
    pub created_on: Option<String>,
    #[serde(default)]
    pub updated_on: Option<String>,
    #[serde(default)]
    pub comment_count: Option<u64>,
    #[serde(default)]
    pub task_count: Option<u64>,
    #[serde(default)]
    pub close_source_branch: Option<bool>,
    #[serde(default)]
    pub summary: Option<RenderedText>,
    #[serde(default)]
    pub reviewers: Option<Vec<User>>,
    #[serde(default)]
    pub participants: Vec<Participant>,
    /// ブラウザで開くための URL 群（`links.html.href` のみ使用）。
    #[serde(default)]
    pub links: Option<PrLinks>,
}

impl PullRequest {
    /// 表示用タイトル（未設定時はプレースホルダ）。
    pub fn title_str(&self) -> &str {
        self.title.as_deref().unwrap_or("(タイトルなし)")
    }

    /// 状態文字列（`OPEN`/`MERGED`/`DECLINED`/`SUPERSEDED` など）。
    pub fn state_str(&self) -> &str {
        self.state.as_deref().unwrap_or("?")
    }

    /// state が OPEN か（merge 可否判定などに使用）。
    pub fn is_open(&self) -> bool {
        self.state.as_deref() == Some("OPEN")
    }

    /// 作成者の表示名。
    pub fn author_name(&self) -> &str {
        self.author
            .as_ref()
            .and_then(|user| user.display_name.as_deref())
            .unwrap_or("?")
    }

    /// source ブランチ名。
    pub fn source_branch(&self) -> &str {
        self.source
            .as_ref()
            .and_then(|reference| reference.branch.as_ref())
            .and_then(|branch| branch.name.as_deref())
            .unwrap_or("?")
    }

    /// destination ブランチ名。
    pub fn destination_branch(&self) -> &str {
        self.destination
            .as_ref()
            .and_then(|reference| reference.branch.as_ref())
            .and_then(|branch| branch.name.as_deref())
            .unwrap_or("?")
    }

    /// 本文（`description` を優先、無ければ `summary.raw`）。空文字は無しとして扱う。
    pub fn body(&self) -> Option<&str> {
        self.description
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .or_else(|| self.summary.as_ref().and_then(|text| text.raw.as_deref()))
            .filter(|text| !text.trim().is_empty())
    }

    /// 承認した参加者の数。
    pub fn approved_count(&self) -> usize {
        self.participants
            .iter()
            .filter(|participant| participant.approved)
            .count()
    }

    /// レビュアー数（role=REVIEWER の参加者数と reviewers 配列の大きい方）。
    pub fn reviewer_count(&self) -> usize {
        let by_role = self
            .participants
            .iter()
            .filter(|participant| participant.role.as_deref() == Some("REVIEWER"))
            .count();
        by_role.max(self.reviewers.as_ref().map_or(0, Vec::len))
    }

    /// ブラウザで開くための URL（`links.html.href`）。無ければ `None`。
    pub fn html_url(&self) -> Option<&str> {
        self.links.as_ref()?.html.as_ref()?.href.as_deref()
    }

    /// 承認した参加者の表示名一覧（`approved == true`）。
    pub fn approved_names(&self) -> Vec<&str> {
        self.participants
            .iter()
            .filter(|participant| participant.approved)
            .map(|participant| {
                participant
                    .user
                    .as_ref()
                    .and_then(|user| user.display_name.as_deref())
                    .unwrap_or("?")
            })
            .collect()
    }

    /// 変更要求した参加者の表示名一覧（`state == "changes_requested"`）。
    pub fn changes_requested_names(&self) -> Vec<&str> {
        self.participants
            .iter()
            .filter(|participant| participant.state.as_deref() == Some("changes_requested"))
            .map(|participant| {
                participant
                    .user
                    .as_ref()
                    .and_then(|user| user.display_name.as_deref())
                    .unwrap_or("?")
            })
            .collect()
    }
}

/// コメント本文（`{ "raw": .., "html": .. }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentContent {
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "HTML 表示は未対応（raw を利用）")]
    pub html: Option<String>,
}

/// inline コメントのアンカー（ファイルパスと行）。M1 では表示のみ。
#[derive(Debug, Clone, Deserialize)]
pub struct Inline {
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub from: Option<u64>,
    #[serde(default)]
    pub to: Option<u64>,
}

/// インラインコメント投稿時にアンカーがどちら側のファイル行を指すか。
///
/// 追加/文脈行は新ファイル側（`to`）、削除行は旧ファイル側（`from`）を指す
/// （`tui::diff::ParsedDiff::comment_anchor` が diff 行の種別から判定する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSide {
    /// 新ファイル側の行番号（`inline.to`）。
    To,
    /// 旧ファイル側の行番号（`inline.from`）。
    From,
}

/// インラインコメント投稿の対象アンカー（`BitbucketClient::create_inline_comment` の引数を
/// まとめたもの。`clippy::too_many_arguments` 回避も兼ねる）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineTarget {
    pub path: String,
    pub side: CommentSide,
    pub line: u32,
}

/// 親コメント参照（スレッド判定に使用）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentParent {
    pub id: u64,
}

/// inline コメントスレッドの解決情報（フィールドは使わず、存在のみで解決済みと判定する）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentResolution {}

/// `GET .../pullrequests/{id}/comments` の要素。
#[derive(Debug, Clone, Deserialize)]
pub struct Comment {
    #[allow(dead_code, reason = "コメント識別・将来のスレッド表示で使用予定")]
    pub id: u64,
    #[serde(default)]
    pub content: Option<CommentContent>,
    #[serde(default)]
    pub user: Option<User>,
    #[serde(default)]
    pub created_on: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "更新日時の表示は未対応")]
    pub updated_on: Option<String>,
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub inline: Option<Inline>,
    /// 親コメント（返信元）。`Some` ならスレッドの返信としてインデント表示する。
    #[serde(default)]
    pub parent: Option<CommentParent>,
    /// スレッドの解決情報（`Some` なら解決済み）。inline スレッドのルートに付く。
    #[serde(default)]
    pub resolution: Option<CommentResolution>,
}

impl Comment {
    /// 表示用の本文（raw）。
    pub fn raw(&self) -> &str {
        self.content
            .as_ref()
            .and_then(|content| content.raw.as_deref())
            .unwrap_or("")
    }

    /// 投稿者の表示名。
    pub fn author_name(&self) -> &str {
        self.user
            .as_ref()
            .and_then(|user| user.display_name.as_deref())
            .unwrap_or("?")
    }

    /// スレッドが解決済みか（`resolution` の有無で判定）。
    pub fn is_resolved(&self) -> bool {
        self.resolution.is_some()
    }
}

/// diffstat の要素（ファイル毎の変更統計）。
#[derive(Debug, Clone, Deserialize)]
pub struct DiffStatEntry {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub lines_added: Option<u64>,
    #[serde(default)]
    pub lines_removed: Option<u64>,
    #[serde(default)]
    pub old: Option<PathEntry>,
    #[serde(default)]
    pub new: Option<PathEntry>,
}

/// diffstat 内のパス（`{ "path": ".." }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct PathEntry {
    #[serde(default)]
    pub path: Option<String>,
}

impl DiffStatEntry {
    /// 表示用パス（新パス優先、無ければ旧パス）。
    pub fn path(&self) -> &str {
        self.new
            .as_ref()
            .and_then(|entry| entry.path.as_deref())
            .or_else(|| self.old.as_ref().and_then(|entry| entry.path.as_deref()))
            .unwrap_or("?")
    }

    /// 変更種別（`modified`/`added`/`removed`/`renamed` など）。
    pub fn status_str(&self) -> &str {
        self.status.as_deref().unwrap_or("?")
    }
}

/// merge のマージ戦略。API では snake_case 文字列（`merge_commit` 等）で送る。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeStrategy {
    MergeCommit,
    Squash,
    FastForward,
}

impl MergeStrategy {
    /// UI で巡回選択するための全戦略。
    pub const ALL: [MergeStrategy; 3] = [
        MergeStrategy::MergeCommit,
        MergeStrategy::Squash,
        MergeStrategy::FastForward,
    ];

    /// UI 表示ラベル。
    pub fn label(self) -> &'static str {
        match self {
            MergeStrategy::MergeCommit => "merge_commit",
            MergeStrategy::Squash => "squash",
            MergeStrategy::FastForward => "fast_forward",
        }
    }
}

/// `POST .../pullrequests/{id}/merge` のリクエストボディ。
///
/// `{"merge_strategy":"..","message":"..","close_source_branch":<bool>}`。
/// `message` は未指定なら送らない（Bitbucket 既定のマージメッセージが使われる）。
#[derive(Debug, Clone, Serialize)]
pub struct MergeParams {
    pub merge_strategy: MergeStrategy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub close_source_branch: bool,
}

/// パイプライン/ステップの状態種別（色分け・ポーリング判定に使う）。
///
/// 実 API の `state.name` / `result.name` 文字列を [`classify_pipeline_status`] で
/// この列挙へ丸める。未知の値は [`PipelineStatus::Unknown`] に落とす。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineStatus {
    /// 成功（緑）。
    Successful,
    /// 失敗・エラー（赤）。
    Failed,
    /// 実行中（黄）。
    InProgress,
    /// 停止・中止（グレー）。
    Stopped,
    /// 保留（既定色）。
    Pending,
    /// 未知の状態（既定色）。
    Unknown,
}

impl PipelineStatus {
    /// 進行中（自動ポーリング対象）か。`PENDING` も含める。
    pub fn is_active(self) -> bool {
        matches!(self, PipelineStatus::InProgress | PipelineStatus::Pending)
    }

    /// UI 表示ラベル（アイコン付き）。
    pub fn icon(self) -> &'static str {
        match self {
            PipelineStatus::Successful => "✔",
            PipelineStatus::Failed => "✖",
            PipelineStatus::InProgress => "▶",
            PipelineStatus::Stopped => "■",
            PipelineStatus::Pending => "…",
            PipelineStatus::Unknown => "?",
        }
    }
}

/// `state.name` と `result.name` から [`PipelineStatus`] を判定する。
///
/// 完了時は `result.name`（`SUCCESSFUL`/`FAILED`/`STOPPED`/`ERROR` 等）で成否が決まるため
/// result を優先する。値は実 API 未検証のため、大文字化して寛容にマッチする。
pub fn classify_pipeline_status(state: Option<&str>, result: Option<&str>) -> PipelineStatus {
    if let Some(result) = result {
        match result.to_ascii_uppercase().as_str() {
            "SUCCESSFUL" | "SUCCESS" | "PASSED" => return PipelineStatus::Successful,
            "FAILED" | "ERROR" => return PipelineStatus::Failed,
            "STOPPED" => return PipelineStatus::Stopped,
            _ => {}
        }
    }
    match state.map(str::to_ascii_uppercase).as_deref() {
        Some("IN_PROGRESS" | "BUILDING" | "RUNNING") => PipelineStatus::InProgress,
        Some("PENDING") => PipelineStatus::Pending,
        Some("PAUSED" | "HALTED" | "STOPPED") => PipelineStatus::Stopped,
        _ => PipelineStatus::Unknown,
    }
}

/// 秒数を `1m 23s` / `45s` 形式に整形する（未設定は空文字）。
pub fn format_duration_secs(seconds: Option<u64>) -> String {
    match seconds {
        Some(total) if total >= 60 => format!("{}m {}s", total / 60, total % 60),
        Some(total) => format!("{total}s"),
        None => String::new(),
    }
}

/// パイプライン/ステップ共通の状態（`{ "name": .., "result": { "name": .. }, "stage": { "name": .. } }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineState {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub result: Option<NamedRef>,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "stage 名の詳細表示は未対応（state/result を優先）"
    )]
    pub stage: Option<NamedRef>,
}

/// `{ "name": ".." }` だけを持つ共通参照（result / stage）。
#[derive(Debug, Clone, Deserialize)]
pub struct NamedRef {
    #[serde(default)]
    pub name: Option<String>,
}

/// パイプラインの実行対象（`{ "type": .., "ref_type": .., "ref_name": .., "commit": {..}, "selector": {..} }`）。
///
/// re-run（trigger）ではこの target を引き継いでリクエストボディを組み立てる。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineTarget {
    #[serde(rename = "type", default)]
    pub target_type: Option<String>,
    #[serde(default)]
    pub ref_type: Option<String>,
    #[serde(default)]
    pub ref_name: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "commit hash の詳細表示は未対応")]
    pub commit: Option<Commit>,
    #[serde(default)]
    pub selector: Option<PipelineSelector>,
}

/// パイプライン target の selector（`{ "type": "default"|"custom"|.., "pattern": ".." }`）。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineSelector {
    #[serde(rename = "type", default)]
    pub selector_type: Option<String>,
    #[serde(default)]
    pub pattern: Option<String>,
}

impl PipelineSelector {
    /// trigger ボディ用の JSON へ変換する。
    fn to_json(&self) -> serde_json::Value {
        let mut map = serde_json::Map::new();
        map.insert(
            "type".to_string(),
            serde_json::Value::String(
                self.selector_type
                    .clone()
                    .unwrap_or_else(|| "default".to_string()),
            ),
        );
        if let Some(pattern) = &self.pattern {
            map.insert(
                "pattern".to_string(),
                serde_json::Value::String(pattern.clone()),
            );
        }
        serde_json::Value::Object(map)
    }
}

impl PipelineTarget {
    /// re-run（trigger）用のリクエストボディ `{"target": {...}}` を組み立てる。
    ///
    /// 元パイプラインの target（type/ref_type/ref_name/selector）を引き継ぐ。`type` は既定で
    /// `pipeline_ref_target`、`selector` は既定で `{"type":"default"}`。commit は送らない
    /// （ブランチ先端の再実行を意図するため）。
    pub fn trigger_body(&self) -> serde_json::Value {
        let mut target = serde_json::Map::new();
        target.insert(
            "type".to_string(),
            serde_json::Value::String(
                self.target_type
                    .clone()
                    .unwrap_or_else(|| "pipeline_ref_target".to_string()),
            ),
        );
        if let Some(ref_type) = &self.ref_type {
            target.insert(
                "ref_type".to_string(),
                serde_json::Value::String(ref_type.clone()),
            );
        }
        if let Some(ref_name) = &self.ref_name {
            target.insert(
                "ref_name".to_string(),
                serde_json::Value::String(ref_name.clone()),
            );
        }
        let selector = self
            .selector
            .as_ref()
            .map(PipelineSelector::to_json)
            .unwrap_or_else(|| serde_json::json!({ "type": "default" }));
        target.insert("selector".to_string(), selector);
        serde_json::json!({ "target": serde_json::Value::Object(target) })
    }
}

/// `GET /repositories/{ws}/{repo}/pipelines/` の要素 / 詳細。
///
/// `uuid` は波括弧 `{...}` を含む文字列。URL に入れる際は percent-encode が必須。
/// `uuid` 以外は実 API 応答での有無が未確定のため `Option`/`#[serde(default)]` で耐性を持たせる。
#[derive(Debug, Clone, Deserialize)]
pub struct Pipeline {
    pub uuid: String,
    #[serde(default)]
    pub build_number: Option<u64>,
    #[serde(default)]
    pub state: Option<PipelineState>,
    #[serde(default)]
    pub creator: Option<User>,
    #[serde(default)]
    pub created_on: Option<String>,
    #[serde(default)]
    pub completed_on: Option<String>,
    #[serde(default)]
    pub target: Option<PipelineTarget>,
    #[serde(default)]
    pub trigger: Option<NamedRef>,
    #[serde(default)]
    pub duration_in_seconds: Option<u64>,
}

impl Pipeline {
    /// 状態種別（色分け・ポーリング判定）。
    pub fn status(&self) -> PipelineStatus {
        let state = self.state.as_ref();
        classify_pipeline_status(
            state.and_then(|s| s.name.as_deref()),
            state
                .and_then(|s| s.result.as_ref())
                .and_then(|r| r.name.as_deref()),
        )
    }

    /// 進行中（自動ポーリング対象・停止可能）か。
    pub fn is_active(&self) -> bool {
        self.status().is_active()
    }

    /// 表示用のビルド番号ラベル（`#123`）。
    pub fn build_label(&self) -> String {
        match self.build_number {
            Some(number) => format!("#{number}"),
            None => "#?".to_string(),
        }
    }

    /// 状態文字列（`state.name`、無ければ `?`）。
    pub fn state_name(&self) -> &str {
        self.state
            .as_ref()
            .and_then(|state| state.name.as_deref())
            .unwrap_or("?")
    }

    /// 結果文字列（`result.name`、無ければ `None`）。
    pub fn result_name(&self) -> Option<&str> {
        self.state
            .as_ref()
            .and_then(|state| state.result.as_ref())
            .and_then(|result| result.name.as_deref())
    }

    /// 対象 ref 名（`target.ref_name`、無ければ `?`）。
    pub fn target_ref(&self) -> &str {
        self.target
            .as_ref()
            .and_then(|target| target.ref_name.as_deref())
            .unwrap_or("?")
    }

    /// トリガ名（`trigger.name`、無ければ `?`）。
    pub fn trigger_name(&self) -> &str {
        self.trigger
            .as_ref()
            .and_then(|trigger| trigger.name.as_deref())
            .unwrap_or("?")
    }

    /// 作成者の表示名（無ければ `?`）。
    pub fn creator_name(&self) -> &str {
        self.creator
            .as_ref()
            .and_then(|user| user.display_name.as_deref())
            .unwrap_or("?")
    }

    /// 所要時間ラベル（`1m 23s` など）。
    pub fn duration_label(&self) -> String {
        format_duration_secs(self.duration_in_seconds)
    }
}

/// `GET /repositories/{ws}/{repo}/pipelines/{uuid}/steps/` の要素。
///
/// `uuid` はステップ識別子（波括弧 `{...}` 込み、ログ取得の URL に使う）。
#[derive(Debug, Clone, Deserialize)]
pub struct PipelineStep {
    pub uuid: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub state: Option<PipelineState>,
    #[serde(default)]
    #[allow(dead_code, reason = "開始日時の詳細表示は未対応")]
    pub started_on: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "完了日時の詳細表示は未対応")]
    pub completed_on: Option<String>,
    #[serde(default)]
    pub duration_in_seconds: Option<u64>,
}

impl PipelineStep {
    /// 状態種別（色分け・ポーリング判定）。
    pub fn status(&self) -> PipelineStatus {
        let state = self.state.as_ref();
        classify_pipeline_status(
            state.and_then(|s| s.name.as_deref()),
            state
                .and_then(|s| s.result.as_ref())
                .and_then(|r| r.name.as_deref()),
        )
    }

    /// 進行中か。
    pub fn is_active(&self) -> bool {
        self.status().is_active()
    }

    /// ステップ名（無ければ `(名前なし)`）。
    pub fn name_str(&self) -> &str {
        self.name.as_deref().unwrap_or("(名前なし)")
    }

    /// 所要時間ラベル。
    pub fn duration_label(&self) -> String {
        format_duration_secs(self.duration_in_seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_paginated_with_next() {
        let json = r#"{
            "values": [{"slug":"acme","name":"Acme"}],
            "next": "https://api.bitbucket.org/2.0/workspaces?page=2",
            "page": 1,
            "size": 3,
            "pagelen": 1
        }"#;
        let page: Paginated<Workspace> = serde_json::from_str(json).expect("valid json");
        assert_eq!(page.values.len(), 1);
        assert_eq!(page.values[0].slug, "acme");
        assert_eq!(
            page.next.as_deref(),
            Some("https://api.bitbucket.org/2.0/workspaces?page=2")
        );
        assert_eq!(page.page, Some(1));
    }

    #[test]
    fn deserializes_paginated_without_next() {
        let json = r#"{ "values": [] }"#;
        let page: Paginated<Repository> = serde_json::from_str(json).expect("valid json");
        assert!(page.values.is_empty());
        assert!(page.next.is_none());
    }

    #[test]
    fn page_info_default_starts_at_page_one_with_unknown_total() {
        let info = PageInfo::default();
        assert_eq!(info.page, 1);
        assert_eq!(info.total_pages, None);
        assert!(!info.has_next);
    }

    #[test]
    fn page_info_from_paginated_computes_ceil_total_pages() {
        let paginated: Paginated<Repository> = serde_json::from_str(
            r#"{ "values": [], "next": "https://api.bitbucket.org/2.0/x?page=2",
                 "page": 2, "size": 45 }"#,
        )
        .expect("valid json");
        let info = PageInfo::from_paginated(&paginated, 2, 20);
        assert_eq!(info.page, 2);
        // ceil(45 / 20) = 3
        assert_eq!(info.total_pages, Some(3));
        assert!(info.has_next);
    }

    #[test]
    fn page_info_from_paginated_falls_back_to_requested_page_when_omitted() {
        let paginated: Paginated<Repository> =
            serde_json::from_str(r#"{ "values": [] }"#).expect("valid json");
        let info = PageInfo::from_paginated(&paginated, 4, 20);
        assert_eq!(info.page, 4);
        assert_eq!(info.total_pages, None);
        assert!(!info.has_next);
    }

    #[test]
    fn page_info_from_paginated_total_pages_none_when_size_omitted() {
        let paginated: Paginated<Repository> = serde_json::from_str(
            r#"{ "values": [], "next": "https://api.bitbucket.org/2.0/x?page=2" }"#,
        )
        .expect("valid json");
        let info = PageInfo::from_paginated(&paginated, 1, 20);
        assert_eq!(info.total_pages, None);
        assert!(info.has_next);
    }

    #[test]
    fn page_info_from_paginated_exact_multiple_has_no_remainder_page() {
        let paginated: Paginated<Repository> =
            serde_json::from_str(r#"{ "values": [], "size": 40 }"#).expect("valid json");
        let info = PageInfo::from_paginated(&paginated, 1, 20);
        // ceil(40 / 20) = 2 ちょうど（余りによる +1 が発生しない）。
        assert_eq!(info.total_pages, Some(2));
        assert!(!info.has_next);
    }

    #[test]
    fn list_sort_default_is_recently_updated() {
        assert_eq!(ListSort::default(), ListSort::RecentlyUpdated);
    }

    #[test]
    fn list_sort_next_cycles_through_all_four_and_wraps() {
        assert_eq!(
            ListSort::RecentlyUpdated.next(),
            ListSort::LeastRecentlyUpdated
        );
        assert_eq!(ListSort::LeastRecentlyUpdated.next(), ListSort::Newest);
        assert_eq!(ListSort::Newest.next(), ListSort::Oldest);
        // 末尾の次は先頭に戻る。
        assert_eq!(ListSort::Oldest.next(), ListSort::RecentlyUpdated);
    }

    #[test]
    fn list_sort_query_value_matches_bitbucket_browser_presets() {
        assert_eq!(ListSort::RecentlyUpdated.query_value(), "-updated_on");
        assert_eq!(ListSort::LeastRecentlyUpdated.query_value(), "updated_on");
        assert_eq!(ListSort::Newest.query_value(), "-created_on");
        assert_eq!(ListSort::Oldest.query_value(), "created_on");
    }

    #[test]
    fn deserializes_user_workspaces_membership() {
        // `GET /2.0/user/workspaces`（`/2.0/workspaces` の後継）の実レスポンス形。
        // ワークスペースは `workspace` にネストし、`workspace_base` は `name` を持たない。
        let json = r#"{
            "values": [{
                "type": "workspace_access",
                "administrator": false,
                "workspace": {
                    "type": "workspace_base",
                    "uuid": "{00000000-0000-0000-0000-000000000000}",
                    "slug": "acme",
                    "links": { "self": { "href": "https://api.bitbucket.org/2.0/workspaces/acme" } }
                }
            }]
        }"#;
        let page: Paginated<WorkspaceMembership> = serde_json::from_str(json).expect("valid json");
        assert_eq!(page.values.len(), 1);
        let ws = &page.values[0].workspace;
        assert_eq!(ws.slug, "acme");
        assert!(ws.name.is_none());
        // name が無いときは slug を表示名に使う。
        assert_eq!(ws.display_name(), "acme");
    }

    #[test]
    fn ignores_unknown_repository_fields() {
        let json = r#"{
            "full_name": "acme/widget",
            "name": "widget",
            "updated_on": "2026-07-01T00:00:00Z",
            "is_private": true,
            "some_future_field": {"nested": 1}
        }"#;
        let repo: Repository = serde_json::from_str(json).expect("valid json");
        assert_eq!(repo.full_name, "acme/widget");
        assert!(repo.is_private);
        assert_eq!(repo.updated_on.as_deref(), Some("2026-07-01T00:00:00Z"));
    }

    #[test]
    fn repository_reads_main_branch() {
        let json = r#"{
            "full_name": "acme/widget",
            "name": "widget",
            "mainbranch": { "type": "branch", "name": "develop" }
        }"#;
        let repo: Repository = serde_json::from_str(json).expect("valid json");
        assert_eq!(repo.main_branch_name(), Some("develop"));
    }

    #[test]
    fn repository_without_main_branch_is_none() {
        let json = r#"{ "full_name": "acme/widget", "name": "widget" }"#;
        let repo: Repository = serde_json::from_str(json).expect("valid json");
        assert_eq!(repo.main_branch_name(), None);
    }

    #[test]
    fn deserializes_branch_with_target() {
        let json = r#"{
            "name": "main",
            "target": {
                "hash": "abcdef1234567890",
                "date": "2026-07-01T00:00:00Z",
                "message": "Fix bug\n\n詳細",
                "author": { "raw": "Alice <a@example.com>", "user": { "display_name": "Alice" } }
            }
        }"#;
        let branch: Branch = serde_json::from_str(json).expect("valid json");
        assert_eq!(branch.name_str(), "main");
        assert_eq!(branch.target_short_hash(), "abcdef12");
        assert_eq!(branch.target_date(), "2026-07-01T00:00:00Z");
        assert_eq!(branch.target_summary(), "Fix bug");
        let commit = branch.target.as_ref().expect("target present");
        assert_eq!(commit.author_name(), "Alice");
    }

    #[test]
    fn branch_tolerates_missing_target() {
        let branch: Branch =
            serde_json::from_str(r#"{ "name": "feature/x" }"#).expect("valid json");
        assert_eq!(branch.name_str(), "feature/x");
        assert_eq!(branch.target_short_hash(), "?");
        assert_eq!(branch.target_summary(), "");
    }

    #[test]
    fn deserializes_commit_with_parents_and_author() {
        let json = r#"{
            "hash": "0123456789ab",
            "message": "Subject line\n\nBody text",
            "date": "2026-07-02T03:04:05Z",
            "author": { "raw": "Bob <b@example.com>" },
            "parents": [ { "hash": "aaaaaaaaaaaa" }, { "hash": "bbbbbbbbbbbb" } ]
        }"#;
        let commit: Commit = serde_json::from_str(json).expect("valid json");
        assert_eq!(commit.short_hash(), "01234567");
        assert_eq!(commit.summary(), "Subject line");
        assert_eq!(commit.message_str(), "Subject line\n\nBody text");
        assert_eq!(commit.date_str(), "2026-07-02T03:04:05Z");
        // user が無ければ raw をフォールバックに使う。
        assert_eq!(commit.author_name(), "Bob <b@example.com>");
        assert_eq!(
            commit.parent_short_hashes(),
            vec!["aaaaaaaa".to_string(), "bbbbbbbb".to_string()]
        );
    }

    #[test]
    fn commit_tolerates_only_hash() {
        let commit: Commit = serde_json::from_str(r#"{ "hash": "deadbeef" }"#).expect("valid json");
        assert_eq!(commit.hash_str(), "deadbeef");
        assert_eq!(commit.short_hash(), "deadbeef");
        assert_eq!(commit.summary(), "(メッセージなし)");
        assert_eq!(commit.author_name(), "?");
        assert!(commit.parent_short_hashes().is_empty());
    }

    #[test]
    fn deserializes_src_directory_entry() {
        let json = r#"{ "type": "commit_directory", "path": "src/tui", "future": 1 }"#;
        let entry: SrcEntry = serde_json::from_str(json).expect("valid json");
        assert!(entry.is_dir());
        assert_eq!(entry.name(), "tui");
        assert_eq!(entry.path_str(), "src/tui");
    }

    #[test]
    fn deserializes_src_file_entry() {
        let json = r#"{
            "type": "commit_file", "path": "src/main.rs", "size": 1024, "mimetype": "text/x-rust"
        }"#;
        let entry: SrcEntry = serde_json::from_str(json).expect("valid json");
        assert!(!entry.is_dir());
        assert_eq!(entry.name(), "main.rs");
        assert_eq!(entry.size, Some(1024));
        assert_eq!(entry.mimetype.as_deref(), Some("text/x-rust"));
    }

    #[test]
    fn src_entry_name_handles_root_level_and_trailing_slash() {
        let file: SrcEntry =
            serde_json::from_str(r#"{ "type": "commit_file", "path": "README.md" }"#)
                .expect("valid json");
        assert_eq!(file.name(), "README.md");
        let dir: SrcEntry =
            serde_json::from_str(r#"{ "type": "commit_directory", "path": "docs/" }"#)
                .expect("valid json");
        assert_eq!(dir.name(), "docs");
    }

    #[test]
    fn deserializes_pull_request_with_participants() {
        let json = r#"{
            "id": 42,
            "title": "Add feature",
            "state": "OPEN",
            "description": "本文です",
            "author": { "display_name": "Alice" },
            "source": { "branch": { "name": "feature" }, "commit": { "hash": "abc" } },
            "destination": { "branch": { "name": "main" } },
            "updated_on": "2026-07-01T00:00:00Z",
            "comment_count": 3,
            "task_count": 1,
            "close_source_branch": true,
            "participants": [
                { "user": { "display_name": "Bob" }, "role": "REVIEWER", "approved": true, "state": "approved" },
                { "user": { "display_name": "Carol" }, "role": "REVIEWER", "approved": false, "state": "changes_requested" }
            ],
            "future_field": { "x": 1 }
        }"#;
        let pr: PullRequest = serde_json::from_str(json).expect("valid json");
        assert_eq!(pr.id, 42);
        assert_eq!(pr.title_str(), "Add feature");
        assert!(pr.is_open());
        assert_eq!(pr.author_name(), "Alice");
        assert_eq!(pr.source_branch(), "feature");
        assert_eq!(pr.destination_branch(), "main");
        assert_eq!(pr.body(), Some("本文です"));
        assert_eq!(pr.approved_count(), 1);
        assert_eq!(pr.reviewer_count(), 2);
        assert_eq!(pr.close_source_branch, Some(true));
        assert_eq!(pr.approved_names(), vec!["Bob"]);
        assert_eq!(pr.changes_requested_names(), vec!["Carol"]);
    }

    #[test]
    fn pull_request_reads_html_url_from_links() {
        let json = r#"{
            "id": 5,
            "links": {
                "html": { "href": "https://bitbucket.org/acme/widget/pull-requests/5" }
            }
        }"#;
        let pr: PullRequest = serde_json::from_str(json).expect("valid json");
        assert_eq!(
            pr.html_url(),
            Some("https://bitbucket.org/acme/widget/pull-requests/5")
        );
    }

    #[test]
    fn pull_request_without_links_has_no_html_url() {
        let pr: PullRequest = serde_json::from_str(r#"{ "id": 6 }"#).expect("valid json");
        assert_eq!(pr.html_url(), None);
    }

    #[test]
    fn pull_request_without_matching_participants_has_empty_name_lists() {
        let json = r#"{
            "id": 7,
            "participants": [
                { "user": { "display_name": "Dan" }, "approved": false, "state": null }
            ]
        }"#;
        let pr: PullRequest = serde_json::from_str(json).expect("valid json");
        assert!(pr.approved_names().is_empty());
        assert!(pr.changes_requested_names().is_empty());
    }

    #[test]
    fn pull_request_tolerates_missing_optional_fields() {
        // id 以外がすべて欠落していてもデシリアライズできること。
        let pr: PullRequest = serde_json::from_str(r#"{ "id": 7 }"#).expect("valid json");
        assert_eq!(pr.id, 7);
        assert_eq!(pr.title_str(), "(タイトルなし)");
        assert_eq!(pr.state_str(), "?");
        assert_eq!(pr.source_branch(), "?");
        assert!(pr.body().is_none());
        assert!(!pr.is_open());
    }

    #[test]
    fn body_falls_back_to_summary_raw() {
        let json = r#"{ "id": 1, "description": "  ", "summary": { "raw": "サマリ" } }"#;
        let pr: PullRequest = serde_json::from_str(json).expect("valid json");
        assert_eq!(pr.body(), Some("サマリ"));
    }

    #[test]
    fn deserializes_comment() {
        let json = r#"{
            "id": 100,
            "content": { "raw": "LGTM", "html": "<p>LGTM</p>" },
            "user": { "display_name": "Dave" },
            "created_on": "2026-07-02T00:00:00Z",
            "deleted": false,
            "inline": { "path": "src/lib.rs", "to": 12, "from": null }
        }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid json");
        assert_eq!(comment.id, 100);
        assert_eq!(comment.raw(), "LGTM");
        assert_eq!(comment.author_name(), "Dave");
        assert!(!comment.deleted);
        let inline = comment.inline.expect("inline present");
        assert_eq!(inline.path.as_deref(), Some("src/lib.rs"));
        assert_eq!(inline.to, Some(12));
        assert_eq!(inline.from, None);
        assert!(comment.parent.is_none());
    }

    #[test]
    fn deserializes_comment_reply_with_parent() {
        let json = r#"{
            "id": 101,
            "content": { "raw": "同意です" },
            "user": { "display_name": "Erin" },
            "parent": { "id": 100 }
        }"#;
        let comment: Comment = serde_json::from_str(json).expect("valid json");
        let parent = comment.parent.expect("parent present");
        assert_eq!(parent.id, 100);
    }

    #[test]
    fn deserializes_diffstat_entry() {
        let json = r#"{
            "status": "modified",
            "lines_added": 10,
            "lines_removed": 2,
            "old": { "path": "old.rs" },
            "new": { "path": "new.rs" }
        }"#;
        let entry: DiffStatEntry = serde_json::from_str(json).expect("valid json");
        assert_eq!(entry.status_str(), "modified");
        assert_eq!(entry.lines_added, Some(10));
        assert_eq!(entry.lines_removed, Some(2));
        assert_eq!(entry.path(), "new.rs");
    }

    #[test]
    fn diffstat_path_falls_back_to_old() {
        // 削除ファイルは new が null になり得る。
        let json = r#"{ "status": "removed", "old": { "path": "gone.rs" }, "new": null }"#;
        let entry: DiffStatEntry = serde_json::from_str(json).expect("valid json");
        assert_eq!(entry.path(), "gone.rs");
    }

    #[test]
    fn merge_params_serializes_with_message() {
        let params = MergeParams {
            merge_strategy: MergeStrategy::Squash,
            message: Some("Merge PR #1".to_string()),
            close_source_branch: true,
        };
        let value = serde_json::to_value(&params).expect("serializes");
        assert_eq!(value["merge_strategy"], "squash");
        assert_eq!(value["message"], "Merge PR #1");
        assert_eq!(value["close_source_branch"], true);
    }

    #[test]
    fn merge_params_omits_message_when_none() {
        let params = MergeParams {
            merge_strategy: MergeStrategy::MergeCommit,
            message: None,
            close_source_branch: false,
        };
        let value = serde_json::to_value(&params).expect("serializes");
        assert_eq!(value["merge_strategy"], "merge_commit");
        assert!(value.get("message").is_none());
        assert_eq!(value["close_source_branch"], false);
    }

    #[test]
    fn merge_strategy_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(MergeStrategy::FastForward).expect("serializes"),
            serde_json::json!("fast_forward")
        );
    }

    #[test]
    fn deserializes_pipeline_with_state_and_target() {
        let json = r#"{
            "uuid": "{1111-2222}",
            "build_number": 42,
            "state": { "name": "COMPLETED", "result": { "name": "SUCCESSFUL" } },
            "creator": { "display_name": "Alice" },
            "created_on": "2026-07-09T12:34:56Z",
            "completed_on": "2026-07-09T12:40:00Z",
            "target": { "type": "pipeline_ref_target", "ref_type": "branch", "ref_name": "main",
                        "selector": { "type": "default" } },
            "trigger": { "name": "PUSH" },
            "duration_in_seconds": 83,
            "future_field": { "x": 1 }
        }"#;
        let pipeline: Pipeline = serde_json::from_str(json).expect("valid json");
        assert_eq!(pipeline.uuid, "{1111-2222}");
        assert_eq!(pipeline.build_label(), "#42");
        assert_eq!(pipeline.state_name(), "COMPLETED");
        assert_eq!(pipeline.result_name(), Some("SUCCESSFUL"));
        assert_eq!(pipeline.status(), PipelineStatus::Successful);
        assert!(!pipeline.is_active());
        assert_eq!(pipeline.target_ref(), "main");
        assert_eq!(pipeline.trigger_name(), "PUSH");
        assert_eq!(pipeline.creator_name(), "Alice");
        assert_eq!(pipeline.duration_label(), "1m 23s");
    }

    #[test]
    fn pipeline_tolerates_only_uuid() {
        let pipeline: Pipeline =
            serde_json::from_str(r#"{ "uuid": "{abc}" }"#).expect("valid json");
        assert_eq!(pipeline.uuid, "{abc}");
        assert_eq!(pipeline.build_label(), "#?");
        assert_eq!(pipeline.state_name(), "?");
        assert_eq!(pipeline.status(), PipelineStatus::Unknown);
        assert_eq!(pipeline.target_ref(), "?");
        assert!(pipeline.duration_label().is_empty());
    }

    #[test]
    fn in_progress_pipeline_is_active() {
        let json = r#"{ "uuid": "{x}", "state": { "name": "IN_PROGRESS" } }"#;
        let pipeline: Pipeline = serde_json::from_str(json).expect("valid json");
        assert_eq!(pipeline.status(), PipelineStatus::InProgress);
        assert!(pipeline.is_active());
    }

    #[test]
    fn deserializes_pipeline_step() {
        let json = r#"{
            "uuid": "{step-1}",
            "name": "Build and test",
            "state": { "name": "COMPLETED", "result": { "name": "FAILED" } },
            "started_on": "2026-07-09T12:34:56Z",
            "completed_on": "2026-07-09T12:35:41Z",
            "duration_in_seconds": 45
        }"#;
        let step: PipelineStep = serde_json::from_str(json).expect("valid json");
        assert_eq!(step.uuid, "{step-1}");
        assert_eq!(step.name_str(), "Build and test");
        assert_eq!(step.status(), PipelineStatus::Failed);
        assert!(!step.is_active());
        assert_eq!(step.duration_label(), "45s");
    }

    #[test]
    fn classify_status_covers_known_values() {
        assert_eq!(
            classify_pipeline_status(Some("COMPLETED"), Some("SUCCESSFUL")),
            PipelineStatus::Successful
        );
        assert_eq!(
            classify_pipeline_status(Some("COMPLETED"), Some("ERROR")),
            PipelineStatus::Failed
        );
        assert_eq!(
            classify_pipeline_status(Some("COMPLETED"), Some("STOPPED")),
            PipelineStatus::Stopped
        );
        assert_eq!(
            classify_pipeline_status(Some("in_progress"), None),
            PipelineStatus::InProgress
        );
        assert_eq!(
            classify_pipeline_status(Some("PENDING"), None),
            PipelineStatus::Pending
        );
        assert_eq!(
            classify_pipeline_status(Some("HALTED"), None),
            PipelineStatus::Stopped
        );
        assert_eq!(
            classify_pipeline_status(Some("WHO_KNOWS"), None),
            PipelineStatus::Unknown
        );
        assert_eq!(
            classify_pipeline_status(None, None),
            PipelineStatus::Unknown
        );
    }

    #[test]
    fn format_duration_variants() {
        assert_eq!(format_duration_secs(None), "");
        assert_eq!(format_duration_secs(Some(5)), "5s");
        assert_eq!(format_duration_secs(Some(83)), "1m 23s");
        assert_eq!(format_duration_secs(Some(120)), "2m 0s");
    }

    #[test]
    fn trigger_body_preserves_target() {
        let json = r#"{ "type": "pipeline_ref_target", "ref_type": "branch",
                        "ref_name": "feature/x", "selector": { "type": "custom", "pattern": "nightly" } }"#;
        let target: PipelineTarget = serde_json::from_str(json).expect("valid json");
        let body = target.trigger_body();
        assert_eq!(body["target"]["type"], "pipeline_ref_target");
        assert_eq!(body["target"]["ref_type"], "branch");
        assert_eq!(body["target"]["ref_name"], "feature/x");
        assert_eq!(body["target"]["selector"]["type"], "custom");
        assert_eq!(body["target"]["selector"]["pattern"], "nightly");
    }

    #[test]
    fn trigger_body_defaults_when_target_sparse() {
        let target: PipelineTarget =
            serde_json::from_str(r#"{ "ref_name": "main" }"#).expect("valid json");
        let body = target.trigger_body();
        assert_eq!(body["target"]["type"], "pipeline_ref_target");
        assert_eq!(body["target"]["ref_name"], "main");
        assert_eq!(body["target"]["selector"]["type"], "default");
        assert!(body["target"].get("ref_type").is_none());
    }
}

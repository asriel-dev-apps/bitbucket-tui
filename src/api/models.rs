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
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降のページ情報表示で使用予定")]
    pub page: Option<u32>,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降のページ情報表示で使用予定")]
    pub size: Option<u32>,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降のページ情報表示で使用予定")]
    pub pagelen: Option<u32>,
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

/// `GET /2.0/workspaces` の要素。
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    #[allow(dead_code, reason = "M1 以降の識別で使用予定")]
    pub uuid: Option<String>,
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

/// ブランチ（名前のみ利用）。
#[derive(Debug, Clone, Deserialize)]
pub struct Branch {
    #[serde(default)]
    pub name: Option<String>,
}

/// コミット参照（hash のみ）。
#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    #[serde(default)]
    #[allow(dead_code, reason = "コミット hash 表示で使用予定")]
    pub hash: Option<String>,
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

/// 親コメント参照（スレッド判定に使用）。
#[derive(Debug, Clone, Deserialize)]
pub struct CommentParent {
    #[allow(dead_code, reason = "スレッド表示は未対応")]
    pub id: u64,
}

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
    #[serde(default)]
    #[allow(dead_code, reason = "スレッド表示は未対応")]
    pub parent: Option<CommentParent>,
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
}

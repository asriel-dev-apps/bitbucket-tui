//! Bitbucket REST API 2.0 の最小モデル（serde）。
//!
//! M0 では認証検証・ワークスペース一覧・リポジトリ一覧に必要なフィールドのみを持つ。
//! 将来の互換性のため、レスポンスに未知フィールドがあっても失敗しないよう
//! （serde はデフォルトで未知フィールドを無視する）、必須でない項目は `Option` にする。

use serde::Deserialize;

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
}

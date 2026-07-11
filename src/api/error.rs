//! Bitbucket API のエラー型。
//!
//! `api` モジュールは lib 的な位置づけのため、`anyhow` ではなく `thiserror` で
//! 明示的なエラー型を提供する。UI 側でステータス種別に応じた表示を行えるよう、
//! HTTP ステータスをドメインエラーへ変換して返す。

use thiserror::Error;

/// Bitbucket REST API 呼び出しで発生し得るエラー。
///
/// `Clone`/`PartialEq` を導出しているのは、mpsc で UI スレッドへ送る際や
/// ユニットテストでの比較を容易にするため。`reqwest::Error` は `Clone` でないため
/// 文字列化して保持する。
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ApiError {
    /// Bitbucket の PR 添付画像が web ログインへ転送され、API token では取得できない。
    #[error(
        "この画像（Bitbucket 添付）は API token では取得できません。o でブラウザ表示してください"
    )]
    BitbucketAttachmentUnavailable,

    /// 401。メールアドレスまたは API token が不正。
    #[error("認証に失敗しました（メールアドレスまたは API token が不正です）")]
    Auth,

    /// 403。token のスコープ不足などでアクセスが拒否された。
    #[error("アクセスが拒否されました（token のスコープ不足の可能性）: {0}")]
    Forbidden(String),

    /// 429。レート制限。`retry_after` は `Retry-After` ヘッダ由来の秒数。
    #[error("レート制限中です（{retry_after} 秒後に再試行してください）")]
    RateLimited { retry_after: u64 },

    /// 上記以外の非成功ステータス。
    #[error("HTTP エラー {status}: {message}")]
    Http { status: u16, message: String },

    /// レスポンス本文の JSON デシリアライズ失敗。
    #[error("レスポンスの解析に失敗しました: {0}")]
    Decode(String),

    /// 接続・タイムアウトなどの通信レイヤのエラー。
    #[error("通信エラー: {0}")]
    Network(String),
}

impl ApiError {
    /// 404 Not Found か（ステップログ未生成の判定などに使う）。
    pub fn is_not_found(&self) -> bool {
        matches!(self, ApiError::Http { status: 404, .. })
    }
}

/// HTTP ステータス・`Retry-After` ヘッダ・レスポンス本文から [`ApiError`] を組み立てる。
///
/// ネットワークに依存しない純粋関数として切り出しているため、ユニットテストで
/// 401/403/429 の変換を直接検証できる。
pub(crate) fn classify_error(status: u16, retry_after: Option<&str>, body: &str) -> ApiError {
    match status {
        401 => ApiError::Auth,
        403 => ApiError::Forbidden(extract_error_message(body)),
        429 => {
            let retry_after = retry_after
                .and_then(|value| value.trim().parse::<u64>().ok())
                .unwrap_or(60);
            ApiError::RateLimited { retry_after }
        }
        other => ApiError::Http {
            status: other,
            message: extract_error_message(body),
        },
    }
}

/// Bitbucket のエラー JSON（`{"error": {"message": ...}}`）から人間向けメッセージを抽出する。
///
/// JSON として解釈できない場合は本文を（長すぎないよう）そのまま返す。
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(|message| message.as_str())
    {
        return message.to_string();
    }

    let trimmed = body.trim();
    if trimmed.is_empty() {
        "詳細情報なし".to_string()
    } else {
        trimmed.chars().take(200).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_401_as_auth() {
        assert_eq!(classify_error(401, None, ""), ApiError::Auth);
    }

    #[test]
    fn classifies_403_with_message() {
        let body = r#"{"type":"error","error":{"message":"scope required: repository"}}"#;
        assert_eq!(
            classify_error(403, None, body),
            ApiError::Forbidden("scope required: repository".to_string())
        );
    }

    #[test]
    fn classifies_429_with_retry_after() {
        assert_eq!(
            classify_error(429, Some("120"), ""),
            ApiError::RateLimited { retry_after: 120 }
        );
    }

    #[test]
    fn classifies_429_without_retry_after_uses_default() {
        assert_eq!(
            classify_error(429, None, ""),
            ApiError::RateLimited { retry_after: 60 }
        );
    }

    #[test]
    fn classifies_other_status_as_http() {
        let body = r#"{"error":{"message":"boom"}}"#;
        assert_eq!(
            classify_error(500, None, body),
            ApiError::Http {
                status: 500,
                message: "boom".to_string()
            }
        );
    }

    #[test]
    fn falls_back_to_raw_body_when_not_json() {
        assert_eq!(
            classify_error(500, None, "plain text failure"),
            ApiError::Http {
                status: 500,
                message: "plain text failure".to_string()
            }
        );
    }

    #[test]
    fn is_not_found_detects_404_only() {
        assert!(classify_error(404, None, "").is_not_found());
        assert!(!classify_error(500, None, "").is_not_found());
        assert!(!ApiError::Auth.is_not_found());
    }
}

//! Bitbucket REST API 2.0 クライアント。
//!
//! HTTP Basic 認証（username = Atlassian アカウントのメール / password = API token）で
//! Bitbucket Cloud を叩く。ページングは `next` を辿って集約し、安全上限で打ち切る。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::{Client as HttpClient, Method};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::error::{ApiError, classify_error};
use crate::api::models::{
    Branch, Comment, Commit, DiffStatEntry, MergeParams, Paginated, Pipeline, PipelineStep,
    PipelineTarget, PullRequest, Repository, SrcEntry, User, Workspace, WorkspaceMembership,
};

/// API のベース URL（Bitbucket Cloud）。
const BASE_URL: &str = "https://api.bitbucket.org/2.0";

/// ページング追跡の安全上限。これを超える `next` は打ち切り、ログに残す。
const MAX_PAGES: usize = 20;

/// `User-Agent` ヘッダ値。
const USER_AGENT: &str = concat!("bitbucket-tui/", env!("CARGO_PKG_VERSION"));

/// TCP 接続確立のタイムアウト。ネットワーク不調時に無限に固まらないための早期打ち切り。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// リクエスト全体（接続〜応答受信完了）のタイムアウト。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// `paginate` に渡すページ取得フューチャの型。
///
/// `tokio::spawn` される親フューチャが `Send` を満たすよう、ページ取得フューチャにも
/// `Send` を要求する。ライフタイム `'a` は取得元（`&self`）の借用に対応する。
type PageFuture<'a, T> = Pin<Box<dyn Future<Output = Result<Paginated<T>, ApiError>> + Send + 'a>>;

/// 認証済みの Bitbucket クライアント。
///
/// `reqwest::Client` は内部的に `Arc` を持ち、クローンは安価。UI から tokio task へ
/// 渡す際にクローンして使う。
#[derive(Clone)]
pub struct BitbucketClient {
    http: HttpClient,
    email: String,
    token: String,
}

impl std::fmt::Debug for BitbucketClient {
    /// token を絶対に露出させないため手動実装（ログ/デバッグ出力対策）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitbucketClient")
            .field("email", &self.email)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl BitbucketClient {
    /// メールと API token からクライアントを構築する。
    ///
    /// `connect_timeout`/`timeout` を設定し、ネットワーク不調時に無限に待たず早期に
    /// エラーを返すようにする（TUI が「固まる」ことを防ぐ）。
    pub fn new(email: String, token: String) -> Result<Self, ApiError> {
        let http = HttpClient::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|error| ApiError::Network(error.to_string()))?;
        Ok(Self { http, email, token })
    }

    /// `GET /2.0/user` で認証情報を検証し、ユーザー情報を返す。
    pub async fn get_current_user(&self) -> Result<User, ApiError> {
        self.send_get(format!("{BASE_URL}/user"), Vec::new()).await
    }

    /// 参加しているワークスペース一覧を取得する。
    ///
    /// 旧 `/2.0/workspaces` は `CHANGE-2770` で廃止されたため `/2.0/user/workspaces` を使う。
    /// 各要素はメンバーシップ（`{ "workspace": {..} }`）なので `workspace` を取り出す。
    pub async fn list_workspaces(&self) -> Result<Vec<Workspace>, ApiError> {
        let memberships: Vec<WorkspaceMembership> = self
            .get_paged("/user/workspaces", &[("pagelen", "50")])
            .await?;
        Ok(memberships.into_iter().map(|m| m.workspace).collect())
    }

    /// 指定ワークスペースで閲覧可能なリポジトリ一覧を更新日時降順で取得する。
    pub async fn list_repositories(&self, workspace: &str) -> Result<Vec<Repository>, ApiError> {
        let path = format!("/repositories/{workspace}");
        self.get_paged(
            &path,
            &[
                ("role", "member"),
                ("sort", "-updated_on"),
                ("pagelen", "50"),
            ],
        )
        .await
    }

    /// PR 一覧を取得する（更新日時降順）。
    ///
    /// `states` は繰り返し `state` クエリとして送る（例: `["OPEN","MERGED"]`）。空の場合は
    /// Bitbucket 既定（OPEN のみ）になる。
    pub async fn list_pull_requests(
        &self,
        workspace: &str,
        repo: &str,
        states: &[&str],
    ) -> Result<Vec<PullRequest>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pullrequests");
        let mut query: Vec<(&str, &str)> = states.iter().map(|state| ("state", *state)).collect();
        query.push(("pagelen", "50"));
        query.push(("sort", "-updated_on"));
        self.get_paged(&path, &query).await
    }

    /// PR 詳細を取得する。
    pub async fn get_pull_request(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<PullRequest, ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}");
        self.send_get(url, Vec::new()).await
    }

    /// PR のユニファイド diff を生テキスト（`text/plain`）で取得する。
    pub async fn get_pr_diff(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<String, ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/diff");
        self.send_get_text(url).await
    }

    /// PR の diffstat（ファイル毎の変更統計）を取得する。
    pub async fn get_pr_diffstat(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<Vec<DiffStatEntry>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pullrequests/{id}/diffstat");
        self.get_paged(&path, &[("pagelen", "50")]).await
    }

    /// PR のコメント一覧を取得する。
    pub async fn list_comments(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<Vec<Comment>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pullrequests/{id}/comments");
        self.get_paged(&path, &[("pagelen", "50")]).await
    }

    /// PR を承認する（`POST .../approve`）。
    pub async fn approve(&self, workspace: &str, repo: &str, id: u64) -> Result<(), ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/approve");
        self.send_empty(Method::POST, url).await
    }

    /// PR の承認を取り消す（`DELETE .../approve`）。
    pub async fn unapprove(&self, workspace: &str, repo: &str, id: u64) -> Result<(), ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/approve");
        self.send_empty(Method::DELETE, url).await
    }

    /// PR に変更要求を出す（`POST .../request-changes`）。
    pub async fn request_changes(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<(), ApiError> {
        let url =
            format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/request-changes");
        self.send_empty(Method::POST, url).await
    }

    /// PR の変更要求を取り消す（`DELETE .../request-changes`）。
    pub async fn unrequest_changes(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
    ) -> Result<(), ApiError> {
        let url =
            format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/request-changes");
        self.send_empty(Method::DELETE, url).await
    }

    /// 一般コメントを投稿する（`POST .../comments`、body `{"content":{"raw":".."}}`）。
    pub async fn create_comment(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
        raw: &str,
    ) -> Result<Comment, ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/comments");
        self.send_json(Method::POST, url, &comment_body(raw)).await
    }

    /// PR をマージする（`POST .../merge`）。
    ///
    /// 大きなマージは 202（処理中）で返り得るが、いずれも成功ステータスなので `Ok(())` を返す。
    /// 応答ボディ（マージ結果 PR）は使わず、呼び出し側が改めて PR を再取得する。
    pub async fn merge_pull_request(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
        params: &MergeParams,
    ) -> Result<(), ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/merge");
        self.send_json_discard(Method::POST, url, params).await
    }

    /// パイプライン一覧を作成日時降順で取得する。
    pub async fn list_pipelines(
        &self,
        workspace: &str,
        repo: &str,
    ) -> Result<Vec<Pipeline>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pipelines/");
        self.get_paged(&path, &[("sort", "-created_on"), ("pagelen", "50")])
            .await
    }

    /// パイプライン詳細を取得する。
    ///
    /// `uuid` は波括弧 `{...}` を含むため、URL 化する際に percent-encode する。
    pub async fn get_pipeline(
        &self,
        workspace: &str,
        repo: &str,
        uuid: &str,
    ) -> Result<Pipeline, ApiError> {
        let encoded = percent_encode(uuid);
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pipelines/{encoded}");
        self.send_get(url, Vec::new()).await
    }

    /// パイプラインのステップ一覧を取得する。
    pub async fn list_pipeline_steps(
        &self,
        workspace: &str,
        repo: &str,
        uuid: &str,
    ) -> Result<Vec<PipelineStep>, ApiError> {
        let encoded = percent_encode(uuid);
        let path = format!("/repositories/{workspace}/{repo}/pipelines/{encoded}/steps/");
        self.get_paged(&path, &[("pagelen", "100")]).await
    }

    /// ステップログを生テキスト（`text/plain`）で取得する。
    ///
    /// ログ未生成時は 404 になり得る（呼び出し側で「ログなし」を表示）。
    pub async fn get_step_log(
        &self,
        workspace: &str,
        repo: &str,
        pipeline_uuid: &str,
        step_uuid: &str,
    ) -> Result<String, ApiError> {
        let pipeline = percent_encode(pipeline_uuid);
        let step = percent_encode(step_uuid);
        let url = format!(
            "{BASE_URL}/repositories/{workspace}/{repo}/pipelines/{pipeline}/steps/{step}/log"
        );
        self.send_get_text(url).await
    }

    /// パイプラインを停止する（未完了ステップを停止）。
    pub async fn stop_pipeline(
        &self,
        workspace: &str,
        repo: &str,
        uuid: &str,
    ) -> Result<(), ApiError> {
        let encoded = percent_encode(uuid);
        let url =
            format!("{BASE_URL}/repositories/{workspace}/{repo}/pipelines/{encoded}/stopPipeline");
        self.send_empty(Method::POST, url).await
    }

    /// パイプラインを再実行する（元 target を引き継いで trigger）。
    pub async fn trigger_pipeline(
        &self,
        workspace: &str,
        repo: &str,
        target: &PipelineTarget,
    ) -> Result<Pipeline, ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pipelines/");
        self.send_json(Method::POST, url, &target.trigger_body())
            .await
    }

    /// ブランチ一覧を最終コミット日時降順で取得する。
    pub async fn list_branches(
        &self,
        workspace: &str,
        repo: &str,
    ) -> Result<Vec<Branch>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/refs/branches");
        self.get_paged(&path, &[("sort", "-target.date"), ("pagelen", "50")])
            .await
    }

    /// コミット履歴を取得する。
    ///
    /// `revision`（ブランチ名/ハッシュ）を省略すると既定ブランチの履歴になる。
    /// ブランチ名に含まれ得る `/` はパスセパレータとして温存する。
    pub async fn list_commits(
        &self,
        workspace: &str,
        repo: &str,
        revision: Option<&str>,
    ) -> Result<Vec<Commit>, ApiError> {
        let path = match revision {
            Some(rev) => format!(
                "/repositories/{workspace}/{repo}/commits/{}",
                encode_path(rev)
            ),
            None => format!("/repositories/{workspace}/{repo}/commits"),
        };
        self.get_paged(&path, &[("pagelen", "50")]).await
    }

    /// コミット詳細を取得する（単数 `commit` エンドポイント）。
    pub async fn get_commit(
        &self,
        workspace: &str,
        repo: &str,
        hash: &str,
    ) -> Result<Commit, ApiError> {
        let url = format!(
            "{BASE_URL}/repositories/{workspace}/{repo}/commit/{}",
            encode_path(hash)
        );
        self.send_get(url, Vec::new()).await
    }

    /// コミット差分をユニファイド diff テキスト（`text/plain`）で取得する。
    ///
    /// `spec` が単一ハッシュのときは当該コミットの差分になる。
    pub async fn get_commit_diff(
        &self,
        workspace: &str,
        repo: &str,
        spec: &str,
    ) -> Result<String, ApiError> {
        let url = format!(
            "{BASE_URL}/repositories/{workspace}/{repo}/diff/{}",
            encode_path(spec)
        );
        self.send_get_text(url).await
    }

    /// ソースのディレクトリ列挙を取得する（`path` 空でルート）。
    ///
    /// `commit` はブランチ名/ハッシュ。`commit`/`path` の `/` は温存し、その他の文字を
    /// percent-encode する。呼び出し側は [`SrcEntry::is_dir`] でディレクトリ/ファイルを判定し、
    /// ファイルは [`BitbucketClient::get_src_file`] で内容を取得する。
    pub async fn list_src(
        &self,
        workspace: &str,
        repo: &str,
        commit: &str,
        path: &str,
    ) -> Result<Vec<SrcEntry>, ApiError> {
        let src_path = format!(
            "/repositories/{workspace}/{repo}/src/{}/{}",
            encode_path(commit),
            encode_path(path)
        );
        self.get_paged(&src_path, &[("pagelen", "100")]).await
    }

    /// ソースのファイル内容を生テキストで取得する。
    ///
    /// バイナリ/巨大ファイルの判定・打切りは呼び出し側（TUI）が行う。
    pub async fn get_src_file(
        &self,
        workspace: &str,
        repo: &str,
        commit: &str,
        path: &str,
    ) -> Result<String, ApiError> {
        let url = format!(
            "{BASE_URL}/repositories/{workspace}/{repo}/src/{}/{}",
            encode_path(commit),
            encode_path(path)
        );
        self.send_get_text(url).await
    }

    /// ページングエンドポイントを `next` に従って全ページ集約する。
    ///
    /// 初回リクエストにのみ `query` を適用する。2 ページ目以降は Bitbucket が返す
    /// `next` URL（クエリ込み）をそのまま使う。
    pub async fn get_paged<T: DeserializeOwned + Send + 'static>(
        &self,
        path: &str,
        query: &[(&str, &str)],
    ) -> Result<Vec<T>, ApiError> {
        let start_url = format!("{BASE_URL}{path}");
        let start_query: Vec<(String, String)> = query
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect();

        paginate(start_url, start_query, MAX_PAGES, |url, query| {
            Box::pin(self.send_get::<Paginated<T>>(url, query))
        })
        .await
    }

    /// 認証付き GET を実行し、成功時は本文を `T` にデシリアライズする。
    async fn send_get<T: DeserializeOwned>(
        &self,
        url: String,
        query: Vec<(String, String)>,
    ) -> Result<T, ApiError> {
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.email, Some(&self.token))
            .query(&query)
            .send()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if !response.status().is_success() {
            return Err(response_to_error(response).await);
        }

        let body = response
            .text()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        serde_json::from_str::<T>(&body).map_err(|error| ApiError::Decode(error.to_string()))
    }

    /// 認証付き GET を実行し、成功時は本文を生テキストで返す（diff 取得用）。
    async fn send_get_text(&self, url: String) -> Result<String, ApiError> {
        let response = self
            .http
            .get(&url)
            .basic_auth(&self.email, Some(&self.token))
            .send()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if !response.status().is_success() {
            return Err(response_to_error(response).await);
        }

        response
            .text()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))
    }

    /// ボディ無しの認証付きリクエスト（POST/DELETE）。成功なら `()`。
    ///
    /// approve/unapprove/request-changes は応答ボディ（participant）を持つが、UI は改めて PR を
    /// 再取得して状態を反映するため、ここではボディを読まず成功可否のみ扱う。
    async fn send_empty(&self, method: Method, url: String) -> Result<(), ApiError> {
        let response = self
            .http
            .request(method, &url)
            .basic_auth(&self.email, Some(&self.token))
            .send()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if !response.status().is_success() {
            return Err(response_to_error(response).await);
        }
        Ok(())
    }

    /// JSON ボディ付きリクエストを実行し、応答を `T` にデシリアライズする。
    async fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: Method,
        url: String,
        body: &B,
    ) -> Result<T, ApiError> {
        let text = self.send_json_text(method, url, body).await?;
        serde_json::from_str::<T>(&text).map_err(|error| ApiError::Decode(error.to_string()))
    }

    /// JSON ボディ付きリクエストを実行し、応答ボディを破棄する（成功可否のみ）。
    async fn send_json_discard<B: Serialize>(
        &self,
        method: Method,
        url: String,
        body: &B,
    ) -> Result<(), ApiError> {
        self.send_json_text(method, url, body).await.map(|_| ())
    }

    /// JSON ボディ付きリクエストの共通処理。成功時は応答本文を生テキストで返す。
    async fn send_json_text<B: Serialize>(
        &self,
        method: Method,
        url: String,
        body: &B,
    ) -> Result<String, ApiError> {
        let response = self
            .http
            .request(method, &url)
            .basic_auth(&self.email, Some(&self.token))
            .json(body)
            .send()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if !response.status().is_success() {
            return Err(response_to_error(response).await);
        }

        response
            .text()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))
    }
}

/// 一般コメント投稿のリクエストボディ（`{"content":{"raw":".."}}`）を組み立てる。
fn comment_body(raw: &str) -> serde_json::Value {
    serde_json::json!({ "content": { "raw": raw } })
}

/// URL パスセグメント用の percent-encode。
///
/// unreserved 文字（`A-Z a-z 0-9 - . _ ~`）以外をすべて `%XX` へエンコードする。
/// pipeline_uuid / step_uuid に含まれる波括弧 `{...}` を確実にエンコードするのが目的
/// （素の `{...}` を送ると Bitbucket が `The value provided is not a valid uuid` を返す）。
fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
    out
}

/// URL パス用のエンコード。[`percent_encode`] と違い `/` はセパレータとして温存する。
///
/// ブランチ名 `feature/x`・ソースパス `src/tui/app.rs` をそのまま使いつつ、空白などの
/// 特殊文字だけを `%XX` へ変換する（unreserved + `/` 以外を全てエンコード）。
fn encode_path(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for &byte in input.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(byte as char);
            }
            _ => {
                out.push('%');
                out.push(hex_digit(byte >> 4));
                out.push(hex_digit(byte & 0x0f));
            }
        }
    }
    out
}

/// 4bit のニブルを大文字 16 進数字へ変換する。
fn hex_digit(nibble: u8) -> char {
    match nibble {
        0..=9 => (b'0' + nibble) as char,
        _ => (b'A' + (nibble - 10)) as char,
    }
}

/// 非成功レスポンスから [`ApiError`] を組み立てる。
///
/// 本文を消費する前に `Retry-After` ヘッダを読み出す。
async fn response_to_error(response: reqwest::Response) -> ApiError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = response.text().await.unwrap_or_default();
    classify_error(status, retry_after.as_deref(), &body)
}

/// ページング追跡のコアロジック。
///
/// ネットワークから切り離してテストできるよう、1 ページ取得を `fetch`（URL とクエリを
/// 受け取りページを返す）として受け取る。`max_pages` に達しても `next` が残っている
/// 場合は打ち切り、警告ログを残す。
async fn paginate<'a, T, F>(
    start_url: String,
    start_query: Vec<(String, String)>,
    max_pages: usize,
    mut fetch: F,
) -> Result<Vec<T>, ApiError>
where
    F: FnMut(String, Vec<(String, String)>) -> PageFuture<'a, T>,
{
    let mut items = Vec::new();
    let mut url = start_url;
    let mut query = start_query;

    for page in 1..=max_pages {
        let mut result = fetch(url, query).await?;
        items.append(&mut result.values);

        match result.next {
            Some(next_url) => {
                if page == max_pages {
                    tracing::warn!(
                        max_pages,
                        "get_paged がページ上限に達したため結果を打ち切りました"
                    );
                    break;
                }
                url = next_url;
                query = Vec::new();
            }
            None => break,
        }
    }

    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page<T>(values: Vec<T>, next: Option<&str>) -> Paginated<T> {
        Paginated {
            values,
            next: next.map(str::to_string),
            page: None,
            size: None,
            pagelen: None,
        }
    }

    #[tokio::test]
    async fn paginate_follows_next_and_aggregates() {
        // start -> page2 -> 終了
        let result = paginate(
            "start".to_string(),
            Vec::new(),
            MAX_PAGES,
            |url: String, _query: Vec<(String, String)>| {
                let page = if url == "start" {
                    make_page(vec![1, 2], Some("page2"))
                } else {
                    make_page(vec![3], None)
                };
                Box::pin(async move { Ok(page) })
            },
        )
        .await
        .expect("no error");

        assert_eq!(result, vec![1, 2, 3]);
    }

    #[tokio::test]
    async fn paginate_passes_query_only_on_first_page() {
        let mut seen_queries: Vec<Vec<(String, String)>> = Vec::new();
        let mut counter = 0;

        let result = paginate(
            "start".to_string(),
            vec![("pagelen".to_string(), "50".to_string())],
            MAX_PAGES,
            |_url: String, query: Vec<(String, String)>| {
                counter += 1;
                seen_queries.push(query);
                let page = if counter == 1 {
                    make_page(vec![10], Some("next"))
                } else {
                    make_page(vec![20], None)
                };
                Box::pin(async move { Ok(page) })
            },
        )
        .await
        .expect("no error");

        assert_eq!(result, vec![10, 20]);
        assert_eq!(seen_queries.len(), 2);
        assert_eq!(
            seen_queries[0],
            vec![("pagelen".to_string(), "50".to_string())]
        );
        assert!(seen_queries[1].is_empty());
    }

    #[tokio::test]
    async fn paginate_truncates_at_max_pages() {
        // 常に next を返し続けるエンドポイントでも上限で打ち切ること。
        let result = paginate(
            "start".to_string(),
            Vec::new(),
            3,
            |_url: String, _query: Vec<(String, String)>| {
                Box::pin(async move { Ok(make_page(vec![1], Some("more"))) })
            },
        )
        .await
        .expect("no error");

        // 3 ページ分だけ取得して打ち切る。
        assert_eq!(result, vec![1, 1, 1]);
    }

    #[test]
    fn comment_body_wraps_raw_content() {
        let body = comment_body("hello\nworld");
        assert_eq!(body["content"]["raw"], "hello\nworld");
    }

    #[test]
    fn percent_encode_escapes_uuid_braces() {
        // 波括弧が %7B / %7D に、ハイフンは温存されること。
        assert_eq!(
            percent_encode("{d3f5e4b0-1234-5678-9abc-def012345678}"),
            "%7Bd3f5e4b0-1234-5678-9abc-def012345678%7D"
        );
    }

    #[test]
    fn percent_encode_leaves_unreserved_untouched() {
        assert_eq!(percent_encode("abcXYZ0-9._~"), "abcXYZ0-9._~");
    }

    #[test]
    fn percent_encode_escapes_slash_and_space() {
        assert_eq!(percent_encode("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn encode_path_preserves_slashes() {
        // `/` は温存し、空白などのみエンコードする。
        assert_eq!(encode_path("src/tui/my file.rs"), "src/tui/my%20file.rs");
        assert_eq!(encode_path("feature/new-thing"), "feature/new-thing");
    }

    #[test]
    fn encode_path_empty_is_empty() {
        assert_eq!(encode_path(""), "");
    }

    #[tokio::test]
    async fn paginate_propagates_error() {
        let result: Result<Vec<i32>, ApiError> = paginate(
            "start".to_string(),
            Vec::new(),
            MAX_PAGES,
            |_url: String, _query: Vec<(String, String)>| {
                Box::pin(async move { Err(ApiError::Auth) })
            },
        )
        .await;

        assert_eq!(result, Err(ApiError::Auth));
    }

    #[test]
    fn new_builds_client_with_timeouts_configured() {
        // connect_timeout/timeout を設定しても生成が panic せず、クローンも安価に行えること。
        let client = BitbucketClient::new("me@example.com".to_string(), "token".to_string())
            .expect("client builds with timeouts configured");
        let _cloned = client.clone();
    }

    /// 実 API を叩くスモークテスト。実ネットワーク＋実 token が必要なので通常はスキップする。
    ///
    /// 実行例:
    /// `BBTUI_TEST_EMAIL=you@example.com BBTUI_TEST_TOKEN=xxxx \`
    /// `  cargo test --offline -- --ignored smoke_get_current_user`
    #[tokio::test]
    #[ignore = "実ネットワーク接続と実 API token が必要"]
    async fn smoke_get_current_user() {
        let email = std::env::var("BBTUI_TEST_EMAIL").expect("BBTUI_TEST_EMAIL が未設定");
        let token = std::env::var("BBTUI_TEST_TOKEN").expect("BBTUI_TEST_TOKEN が未設定");
        let client = BitbucketClient::new(email, token).expect("クライアント生成");
        let user = client.get_current_user().await.expect("認証成功");
        assert!(user.display_name.is_some() || user.uuid.is_some());
    }
}

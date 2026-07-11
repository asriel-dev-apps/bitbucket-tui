//! Bitbucket REST API 2.0 クライアント。
//!
//! HTTP Basic 認証（username = Atlassian アカウントのメール / password = API token）で
//! Bitbucket Cloud を叩く。ページングは `next` を辿って集約し、安全上限で打ち切る。

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use reqwest::{Client as HttpClient, Method, RequestBuilder, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::api::error::{ApiError, classify_error};
use crate::api::models::{
    Branch, Comment, CommentSide, Commit, DiffStatEntry, InlineTarget, ListSort, MergeParams,
    PageInfo, Paginated, Pipeline, PipelineStep, PipelineTarget, PullRequest, Repository, SrcEntry,
    User, Workspace, WorkspaceMembership,
};

/// API のベース URL（Bitbucket Cloud）。
const BASE_URL: &str = "https://api.bitbucket.org/2.0";

/// ページング追跡の安全上限。これを超える `next` は打ち切り、ログに残す。
const MAX_PAGES: usize = 20;

/// workspaces / repositories / pull_requests / branches / pipelines のサーバサイド・
/// ページネーション 1 ページあたりの件数。
///
/// 従来の `get_paged`（`next` を最大 [`MAX_PAGES`] ページ直列取得して集約）は、全ページ揃うまで
/// 一覧が表示されず初回取得が遅くなるため、この 5 画面は 1 ページ（40 件）のみを取得して
/// 即座に表示し、ページャ UI（`tui::app`）でページ間を移動する方式に変更した。
pub const PAGE_SIZE: u32 = 40;

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

    /// 参加しているワークスペース一覧の指定ページを取得する（`pagelen` 固定 [`PAGE_SIZE`]）。
    ///
    /// 旧 `/2.0/workspaces` は `CHANGE-2770` で廃止されたため `/2.0/user/workspaces` を使う。
    /// 各要素はメンバーシップ（`{ "workspace": {..} }`）なので `workspace` を取り出す。
    pub async fn get_workspaces_page(&self, page: u32) -> Result<Page<Workspace>, ApiError> {
        let paginated: Paginated<WorkspaceMembership> = self
            .fetch_single_page("/user/workspaces", &[], page)
            .await?;
        let info = PageInfo::from_paginated(&paginated, page, PAGE_SIZE);
        let values = paginated.values.into_iter().map(|m| m.workspace).collect();
        Ok(Page { values, info })
    }

    /// 指定ワークスペースで閲覧可能なリポジトリ一覧の指定ページを、指定ソート順で取得する
    /// （`pagelen` 固定 [`PAGE_SIZE`]）。
    pub async fn get_repositories_page(
        &self,
        workspace: &str,
        sort: ListSort,
        page: u32,
    ) -> Result<Page<Repository>, ApiError> {
        let path = format!("/repositories/{workspace}");
        let query = repositories_query(sort);
        let paginated: Paginated<Repository> = self.fetch_single_page(&path, &query, page).await?;
        let info = PageInfo::from_paginated(&paginated, page, PAGE_SIZE);
        Ok(Page {
            values: paginated.values,
            info,
        })
    }

    /// PR 一覧の指定ページを、指定ソート順で取得する（`pagelen` 固定 [`PAGE_SIZE`]）。
    ///
    /// `states` は繰り返し `state` クエリとして送る（例: `["OPEN","MERGED"]`）。空の場合は
    /// Bitbucket 既定（OPEN のみ）になる。
    pub async fn get_pull_requests_page(
        &self,
        workspace: &str,
        repo: &str,
        states: &[&str],
        sort: ListSort,
        page: u32,
    ) -> Result<Page<PullRequest>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pullrequests");
        let query = pull_requests_query(states, sort);
        let paginated: Paginated<PullRequest> = self.fetch_single_page(&path, &query, page).await?;
        let info = PageInfo::from_paginated(&paginated, page, PAGE_SIZE);
        Ok(Page {
            values: paginated.values,
            info,
        })
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

    /// インラインコメントを投稿する（`POST .../comments`、
    /// body `{"content":{"raw":".."},"inline":{"path":"..","to"|"from":<line>}}`）。
    ///
    /// PR 差分の特定行への返信専用。`target.side` が `To` なら新ファイル側（`inline.to`）、
    /// `From` なら旧ファイル側（`inline.from`）を指定する
    /// （`tui::diff::ParsedDiff::comment_anchor` の出力をそのまま渡す想定。引数が
    /// `clippy::too_many_arguments` に触れるため `path`/`side`/`line` を [`InlineTarget`] へ
    /// まとめている）。
    pub async fn create_inline_comment(
        &self,
        workspace: &str,
        repo: &str,
        id: u64,
        target: &InlineTarget,
        raw: &str,
    ) -> Result<Comment, ApiError> {
        let url = format!("{BASE_URL}/repositories/{workspace}/{repo}/pullrequests/{id}/comments");
        let body = inline_comment_body(raw, &target.path, target.side, target.line);
        self.send_json(Method::POST, url, &body).await
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

    /// パイプライン一覧の指定ページを作成日時降順で取得する（`pagelen` 固定 [`PAGE_SIZE`]）。
    ///
    /// エンドポイントは末尾スラッシュ `/pipelines/` の既存仕様に合わせる（`get_pipeline`/
    /// `trigger_pipeline` と同じ形）。
    pub async fn get_pipelines_page(
        &self,
        workspace: &str,
        repo: &str,
        page: u32,
    ) -> Result<Page<Pipeline>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/pipelines/");
        let query = pipelines_query();
        let paginated: Paginated<Pipeline> = self.fetch_single_page(&path, &query, page).await?;
        let info = PageInfo::from_paginated(&paginated, page, PAGE_SIZE);
        Ok(Page {
            values: paginated.values,
            info,
        })
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

    /// ブランチ一覧の指定ページを、最終コミット日時降順で取得する
    /// （`pagelen` 固定 [`PAGE_SIZE`]）。
    ///
    /// workspaces/repositories/pull_requests と同じサーバサイド・ページネーションで、
    /// ページを跨いでも順序が安定するよう `sort=-target.date` を明示する（Bitbucket の
    /// 既定順に依存しない）。
    pub async fn get_branches_page(
        &self,
        workspace: &str,
        repo: &str,
        page: u32,
    ) -> Result<Page<Branch>, ApiError> {
        let path = format!("/repositories/{workspace}/{repo}/refs/branches");
        let query = branches_query();
        let paginated: Paginated<Branch> = self.fetch_single_page(&path, &query, page).await?;
        let info = PageInfo::from_paginated(&paginated, page, PAGE_SIZE);
        Ok(Page {
            values: paginated.values,
            info,
        })
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

    /// PR 本文の画像 URL から生バイトを取得する。
    ///
    /// Bitbucket ホストにだけ Basic 認証を付ける。リダイレクトは reqwest の既定ポリシーで
    /// 追従し、別ホストへ移る場合は reqwest が Authorization を除去する。
    ///
    /// `max_bytes` を超える場合は拒否する（`Content-Length` があれば受信前に、無ければ受信後の
    /// 実サイズで判定する）。
    pub async fn get_image_bytes(&self, url: &str, max_bytes: usize) -> Result<Vec<u8>, ApiError> {
        let request = self.http.get(url);
        let request = with_bitbucket_auth(request, url, &self.email, &self.token);
        let response = request
            .send()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if let Some(error) = image_redirect_error(response.url()) {
            return Err(error);
        }

        if !response.status().is_success() {
            return Err(response_to_error(response).await);
        }

        if let Some(length) = response.content_length()
            && length as usize > max_bytes
        {
            return Err(ApiError::Decode(format!(
                "画像が大きすぎます（{length} bytes、上限 {max_bytes} bytes）"
            )));
        }

        let bytes = response
            .bytes()
            .await
            .map_err(|error| ApiError::Network(error.to_string()))?;

        if bytes.len() > max_bytes {
            return Err(ApiError::Decode(format!(
                "画像が大きすぎます（{} bytes、上限 {max_bytes} bytes）",
                bytes.len()
            )));
        }

        Ok(bytes.to_vec())
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

    /// `page`/`pagelen`（固定 [`PAGE_SIZE`]）を含む単一ページ分の GET を行い、生のページ応答
    /// （`Paginated<T>`）を返す。全集約する [`Self::get_paged`] とは異なり、指定ページのみを
    /// 取得する（workspaces/repositories/pull_requests のサーバサイド・ページネーションで使う）。
    async fn fetch_single_page<T: DeserializeOwned>(
        &self,
        path: &str,
        extra_query: &[(&str, &str)],
        page: u32,
    ) -> Result<Paginated<T>, ApiError> {
        let url = format!("{BASE_URL}{path}");
        self.send_get(url, page_query(extra_query, page)).await
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

/// 画像 URL が Bitbucket の認証対象ホストなら Basic 認証を付ける。
///
/// URL を解析できない場合も認証情報を付けず、reqwest 自身の URL エラーとして扱わせる。
fn with_bitbucket_auth(
    request: RequestBuilder,
    url: &str,
    email: &str,
    token: &str,
) -> RequestBuilder {
    if Url::parse(url).is_ok_and(|url| should_attach_auth(&url)) {
        request.basic_auth(email, Some(token))
    } else {
        request
    }
}

/// Authorization を送ってよい Bitbucket ホストか。
fn should_attach_auth(url: &Url) -> bool {
    matches!(url.host_str(), Some("bitbucket.org" | "api.bitbucket.org"))
}

/// Bitbucket の web ログイン URL か。クエリ文字列は判定に影響しない。
fn is_bitbucket_signin_url(url: &Url) -> bool {
    url.host_str() == Some("bitbucket.org") && url.path().trim_end_matches('/') == "/account/signin"
}

/// 画像取得後の最終 URL を、Bitbucket 添付画像固有のエラーへ変換する。
fn image_redirect_error(url: &Url) -> Option<ApiError> {
    is_bitbucket_signin_url(url).then_some(ApiError::BitbucketAttachmentUnavailable)
}

/// 単一ページ取得の結果（値とページ情報）。[`BitbucketClient::get_workspaces_page`] 等が返す。
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub values: Vec<T>,
    pub info: PageInfo,
}

/// リポジトリ一覧取得の追加クエリ（`role=member` 固定 + 現在のソート）。ネットワークを
/// 介さずに検証できるよう純粋関数として切り出している（[`page_query`] と同じ狙い）。
fn repositories_query(sort: ListSort) -> [(&'static str, &'static str); 2] {
    [("role", "member"), ("sort", sort.query_value())]
}

/// PR 一覧取得の追加クエリ（state フィルタの繰り返し + 現在のソート）。
fn pull_requests_query<'a>(states: &'a [&'a str], sort: ListSort) -> Vec<(&'a str, &'a str)> {
    let mut query: Vec<(&str, &str)> = states.iter().map(|state| ("state", *state)).collect();
    query.push(("sort", sort.query_value()));
    query
}

/// ブランチ一覧取得の追加クエリ（最終コミット日時降順で固定）。ページを跨いでも順序が
/// 安定するよう明示する。ネットワークを介さずに検証できるよう純粋関数として切り出している
/// （[`repositories_query`] と同じ狙い）。
fn branches_query() -> [(&'static str, &'static str); 1] {
    [("sort", "-target.date")]
}

/// パイプライン一覧取得の追加クエリ（作成日時降順で固定）。ネットワークを介さずに検証
/// できるよう純粋関数として切り出している（[`branches_query`] と同じ狙い）。
fn pipelines_query() -> [(&'static str, &'static str); 1] {
    [("sort", "-created_on")]
}

/// 単一ページ取得のクエリを組み立てる。`extra` の後ろに `page`/`pagelen`（固定 [`PAGE_SIZE`]）
/// を追加する。ネットワークを介さずに検証できるよう、実際のリクエスト送信
/// （[`BitbucketClient::fetch_single_page`]）から切り出した純粋関数にしている。
fn page_query(extra: &[(&str, &str)], page: u32) -> Vec<(String, String)> {
    let mut query: Vec<(String, String)> = extra
        .iter()
        .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
        .collect();
    query.push(("page".to_string(), page.to_string()));
    query.push(("pagelen".to_string(), PAGE_SIZE.to_string()));
    query
}

/// 一般コメント投稿のリクエストボディ（`{"content":{"raw":".."}}`）を組み立てる。
fn comment_body(raw: &str) -> serde_json::Value {
    serde_json::json!({ "content": { "raw": raw } })
}

/// インラインコメント投稿のリクエストボディ
/// （`{"content":{"raw":".."},"inline":{"path":"..","to"|"from":<line>}}`）を組み立てる。
///
/// `side` が `To`（追加/文脈行）なら `inline.to`、`From`（削除行）なら `inline.from` を使う
/// （両方を同時には送らない。Bitbucket 側の解釈は未検証の仮定として `docs/LEDGER.md` に残す）。
fn inline_comment_body(raw: &str, path: &str, side: CommentSide, line: u32) -> serde_json::Value {
    let inline = match side {
        CommentSide::To => serde_json::json!({ "path": path, "to": line }),
        CommentSide::From => serde_json::json!({ "path": path, "from": line }),
    };
    serde_json::json!({ "content": { "raw": raw }, "inline": inline })
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
    use reqwest::header::AUTHORIZATION;

    fn make_page<T>(values: Vec<T>, next: Option<&str>) -> Paginated<T> {
        Paginated {
            values,
            next: next.map(str::to_string),
            page: None,
            size: None,
            pagelen: None,
        }
    }

    fn image_request(url: &str) -> reqwest::Request {
        let http = HttpClient::new();
        with_bitbucket_auth(http.get(url), url, "me@example.com", "secret")
            .build()
            .expect("test request should build")
    }

    #[test]
    fn image_auth_is_attached_only_to_bitbucket_hosts() {
        for url in [
            "https://bitbucket.org/workspace/repo/images/file.png",
            "https://api.bitbucket.org/2.0/repositories/workspace/repo",
        ] {
            assert!(image_request(url).headers().contains_key(AUTHORIZATION));
        }

        for url in [
            "https://images.example.com/file.png",
            "https://bitbucket.org.evil.example/file.png",
            "https://subdomain.bitbucket.org/file.png",
        ] {
            assert!(!image_request(url).headers().contains_key(AUTHORIZATION));
        }
    }

    #[test]
    fn image_auth_is_not_reattached_after_redirect_to_other_host() {
        let initial = image_request("https://bitbucket.org/workspace/repo/images/file.png");
        let redirected = image_request("https://cdn.example.com/file.png");

        assert!(initial.headers().contains_key(AUTHORIZATION));
        assert!(!redirected.headers().contains_key(AUTHORIZATION));
    }

    #[test]
    fn signin_redirect_becomes_attachment_unavailable_error_with_exact_message() {
        let url = Url::parse("https://bitbucket.org/account/signin/?next=%2Fimage")
            .expect("test URL should parse");
        let error = image_redirect_error(&url).expect("signin URL should become an error");

        assert_eq!(error, ApiError::BitbucketAttachmentUnavailable);
        assert_eq!(
            error.to_string(),
            "この画像（Bitbucket 添付）は API token では取得できません。o でブラウザ表示してください"
        );
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
    fn repositories_query_includes_role_member_and_selected_sort() {
        assert_eq!(
            repositories_query(ListSort::RecentlyUpdated),
            [("role", "member"), ("sort", "-updated_on")]
        );
        assert_eq!(
            repositories_query(ListSort::Oldest),
            [("role", "member"), ("sort", "created_on")]
        );
    }

    #[test]
    fn pull_requests_query_repeats_state_and_appends_selected_sort() {
        assert_eq!(
            pull_requests_query(&["OPEN", "MERGED"], ListSort::Newest),
            vec![
                ("state", "OPEN"),
                ("state", "MERGED"),
                ("sort", "-created_on"),
            ]
        );
    }

    #[test]
    fn pull_requests_query_with_no_states_only_has_sort() {
        assert_eq!(
            pull_requests_query(&[], ListSort::LeastRecentlyUpdated),
            vec![("sort", "updated_on")]
        );
    }

    #[test]
    fn branches_query_sorts_by_target_date_descending() {
        assert_eq!(branches_query(), [("sort", "-target.date")]);
    }

    #[test]
    fn pipelines_query_sorts_by_created_on_descending() {
        assert_eq!(pipelines_query(), [("sort", "-created_on")]);
    }

    #[test]
    fn page_query_for_pipelines_includes_sort_page_and_fixed_pagelen() {
        let query = page_query(&pipelines_query(), 2);
        assert_eq!(
            query,
            vec![
                ("sort".to_string(), "-created_on".to_string()),
                ("page".to_string(), "2".to_string()),
                ("pagelen".to_string(), "40".to_string()),
            ]
        );
    }

    #[test]
    fn page_query_for_branches_includes_sort_page_and_fixed_pagelen() {
        let query = page_query(&branches_query(), 2);
        assert_eq!(
            query,
            vec![
                ("sort".to_string(), "-target.date".to_string()),
                ("page".to_string(), "2".to_string()),
                ("pagelen".to_string(), "40".to_string()),
            ]
        );
    }

    #[test]
    fn page_query_includes_page_and_fixed_pagelen_after_extra_query() {
        let query = page_query(&[("role", "member"), ("sort", "-updated_on")], 3);
        assert_eq!(
            query,
            vec![
                ("role".to_string(), "member".to_string()),
                ("sort".to_string(), "-updated_on".to_string()),
                ("page".to_string(), "3".to_string()),
                ("pagelen".to_string(), "40".to_string()),
            ]
        );
    }

    #[test]
    fn page_query_with_no_extra_query() {
        let query = page_query(&[], 1);
        assert_eq!(
            query,
            vec![
                ("page".to_string(), "1".to_string()),
                ("pagelen".to_string(), "40".to_string()),
            ]
        );
    }

    #[test]
    fn page_size_constant_is_forty() {
        assert_eq!(PAGE_SIZE, 40);
    }

    #[test]
    fn comment_body_wraps_raw_content() {
        let body = comment_body("hello\nworld");
        assert_eq!(body["content"]["raw"], "hello\nworld");
    }

    #[test]
    fn inline_comment_body_uses_to_for_added_or_context_side() {
        let body = inline_comment_body("LGTM", "src/lib.rs", CommentSide::To, 12);
        assert_eq!(body["content"]["raw"], "LGTM");
        assert_eq!(body["inline"]["path"], "src/lib.rs");
        assert_eq!(body["inline"]["to"], 12);
        assert!(body["inline"]["from"].is_null());
    }

    #[test]
    fn inline_comment_body_uses_from_for_removed_side() {
        let body = inline_comment_body("なぜ削除？", "src/lib.rs", CommentSide::From, 7);
        assert_eq!(body["content"]["raw"], "なぜ削除？");
        assert_eq!(body["inline"]["path"], "src/lib.rs");
        assert_eq!(body["inline"]["from"], 7);
        assert!(body["inline"]["to"].is_null());
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

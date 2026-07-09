//! Bitbucket REST API 2.0 クライアントとモデル。
//!
//! エラーは `thiserror` による [`error::ApiError`] で表現する（`anyhow` は bin 側のみ）。

pub mod client;
pub mod error;
pub mod models;

pub use client::BitbucketClient;
pub use error::ApiError;
pub use models::{Repository, User, Workspace};

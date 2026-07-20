//! bitbucket-tui: Bitbucket Cloud をキーボード駆動で操作する TUI クライアント（M0 基盤）。
//!
//! bin 側のエラーは `anyhow` で扱う。TUI 実行中は stdout/stderr へ出力しない
//! （ログは `BBTUI_LOG` 設定時のみファイルへ）。

mod api;
mod auth;
mod config;
mod logging;
mod tui;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::api::BitbucketClient;
use crate::config::Config;
use crate::tui::App;

/// CLI 引数定義。
#[derive(Debug, Parser)]
#[command(
    name = "bitbucket-tui",
    version,
    about = "Bitbucket Cloud 用の TUI クライアント"
)]
struct Cli {
    /// 起動前に保存済みの認証情報（OS セキュアストアの token と config）を消去する。
    #[arg(long)]
    reset_auth: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 保存済みの認証情報（OS セキュアストアの token と config）を消去する。
    Logout,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // ログは BBTUI_LOG 設定時のみ有効。TUI 起動より前に一度だけ初期化する。
    logging::init()?;

    match cli.command {
        Some(Commands::Logout) => return logout(),
        None => {}
    }

    if cli.reset_auth {
        logout()?;
    }

    // TUI は非同期ループ。ここで tokio runtime を構築する。
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio ランタイムの構築に失敗しました")?;

    runtime.block_on(run_tui())
}

/// 認証情報を消去する（CLI コマンド。TUI 外なので stdout への出力は許容）。
fn logout() -> Result<()> {
    let config = Config::load().unwrap_or_default();
    if let Some(email) = config.email.as_deref() {
        auth::delete_token(email).with_context(|| {
            format!("OS セキュアストアからの token 削除に失敗しました（{email}）")
        })?;
    }
    Config::clear().context("設定ファイルの削除に失敗しました")?;
    println!("認証情報を消去しました。");
    if matches!(env_credentials(), EnvCredentials::Complete { .. }) {
        println!("BBTUI_EMAIL / BBTUI_TOKEN の環境変数は消去できません。");
    }
    Ok(())
}

/// 設定を読み、可能ならクライアントを復元して TUI を起動する。
async fn run_tui() -> Result<()> {
    let (config, client) = restore_client();
    let mut app = App::new(config, client);

    let mut terminal = tui::Tui::init()?;
    // 画像表示機能向けの `Picker`（`ratatui_image::picker::Picker`）生成は、raw mode +
    // alternate screen に入った直後・入力読み取りスレッド開始前の今ここで一度だけ行う
    // （`tui::run` が内部で stdin を占有する専用スレッドを起動するため、それより後に呼ぶと
    // 標準入力の読み取りが競合する）。`Picker::from_query_stdio` は端末へ問い合わせの
    // エスケープシーケンスを送受信して画像プロトコル（Sixel/Kitty/iTerm2/ハーフブロック）と
    // フォントサイズを検出する。検出に失敗しても（`None`）アプリは落とさず、画像表示機能だけを
    // 無効化する（`App::image_picker`）。
    app.image_picker = detect_image_picker();
    // ガード（terminal）は Drop で端末を復元するため、途中エラーでも安全。
    let result = tui::run(&mut terminal, app).await;
    drop(terminal);
    result
}

/// 画像表示機能向けにこの端末が使う `ratatui_image::picker::Picker` を検出する。
///
/// `Picker::from_query_stdio` は端末へ問い合わせのエスケープシーケンスを送受信するため、
/// raw mode + alternate screen へ入った後・イベントループの入力スレッド開始前に呼ぶ必要がある
/// （呼び出し元の [`run_tui`] のコメント参照）。検出に失敗した場合は `None` を返し、呼び出し側は
/// 画像表示機能を無効化する（アプリは落ちない）。
///
/// 生成した `Picker` は `App::image_picker` として保持し、`i` キーで画像を開くたびに
/// `Picker::new_resize_protocol` で `StatefulProtocol` を作る（`src/tui/app.rs` 参照）。
/// 端末が画像プロトコル（Sixel/Kitty/iTerm2）に対応していなければ `ratatui-image` 自身が
/// 内蔵のハーフブロック描画へ自動フォールバックするため、この関数側でのフォールバック処理は
/// 不要（`Picker::from_query_stdio` 自体が失敗した場合のみ `None`）。
fn detect_image_picker() -> Option<ratatui_image::picker::Picker> {
    ratatui_image::picker::Picker::from_query_stdio().ok()
}

/// 環境変数、または config の email と OS セキュアストアの token からクライアントを復元する。
///
/// `BBTUI_EMAIL` / `BBTUI_TOKEN` が揃っている場合、config は UI 設定として読み込むが、
/// config の email と OS セキュアストアの token は資格情報として参照しない。
/// token 未保存・ストアへアクセス不可などの場合は `None`（→ Onboarding へ）。
fn restore_client() -> (Config, Option<BitbucketClient>) {
    match env_credentials() {
        EnvCredentials::Complete { email, token } => {
            let config = Config::load().unwrap_or_default();
            return restore_env_client(config, email, token);
        }
        EnvCredentials::Incomplete => {
            tracing::warn!(
                "BBTUI_EMAIL / BBTUI_TOKEN は両方設定してください。通常の認証フローを使います"
            );
        }
        EnvCredentials::Absent => {}
    }

    let config = Config::load().unwrap_or_default();
    let client = restore_persisted_client(&config);
    (config, client)
}

fn restore_env_client(
    config: Config,
    email: String,
    token: String,
) -> (Config, Option<BitbucketClient>) {
    let client = match BitbucketClient::new(email, token) {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::warn!(%error, "環境変数からのクライアント復元に失敗しました");
            None
        }
    };
    (config, client)
}

fn restore_persisted_client(config: &Config) -> Option<BitbucketClient> {
    let email = config.email.as_deref()?;
    match auth::load_token(email) {
        Ok(Some(token)) => match BitbucketClient::new(email.to_string(), token) {
            Ok(client) => Some(client),
            Err(error) => {
                tracing::warn!(%error, "クライアントの復元に失敗しました");
                None
            }
        },
        Ok(None) => None,
        Err(error) => {
            tracing::warn!(%error, "OS セキュアストアからの token 読み出しに失敗しました");
            None
        }
    }
}

#[derive(PartialEq, Eq)]
enum EnvCredentials {
    Complete { email: String, token: String },
    Incomplete,
    Absent,
}

impl std::fmt::Debug for EnvCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete { email, token: _ } => f
                .debug_struct("Complete")
                .field("email", email)
                .field("token", &"<redacted>")
                .finish(),
            Self::Incomplete => f.write_str("Incomplete"),
            Self::Absent => f.write_str("Absent"),
        }
    }
}

fn env_credentials() -> EnvCredentials {
    classify_env_credentials(
        std::env::var("BBTUI_EMAIL").ok(),
        std::env::var("BBTUI_TOKEN").ok(),
    )
}

fn classify_env_credentials(email: Option<String>, token: Option<String>) -> EnvCredentials {
    match (email, token) {
        (Some(email), Some(token)) => EnvCredentials::Complete { email, token },
        (None, None) => EnvCredentials::Absent,
        _ => EnvCredentials::Incomplete,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_credentials_require_both_values() {
        assert_eq!(
            classify_env_credentials(
                Some("me@example.com".to_string()),
                Some("secret".to_string())
            ),
            EnvCredentials::Complete {
                email: "me@example.com".to_string(),
                token: "secret".to_string()
            }
        );
        assert_eq!(
            classify_env_credentials(Some("me@example.com".to_string()), None),
            EnvCredentials::Incomplete
        );
        assert_eq!(
            classify_env_credentials(None, Some("secret".to_string())),
            EnvCredentials::Incomplete
        );
        assert_eq!(classify_env_credentials(None, None), EnvCredentials::Absent);
    }

    #[test]
    fn env_credentials_debug_redacts_token() {
        let credentials = EnvCredentials::Complete {
            email: "me@example.com".to_string(),
            token: "raw-secret-token".to_string(),
        };

        let debug = format!("{credentials:?}");
        assert_eq!(
            debug,
            "Complete { email: \"me@example.com\", token: \"<redacted>\" }"
        );
        assert!(!debug.contains("raw-secret-token"));
    }

    #[test]
    fn env_client_preserves_loaded_ui_config() {
        let config = Config {
            default_workspace: Some("workspace".to_string()),
            theme: Some("dracula".to_string()),
            diff_view: Some("split".to_string()),
            diff_sidebar_visible: Some(false),
            diff_sidebar_width: Some(42),
            pr_states: Some(vec!["MERGED".to_string()]),
            ..Config::default()
        };

        let (restored, client) = restore_env_client(
            config,
            "env@example.com".to_string(),
            "env-token".to_string(),
        );

        assert!(client.is_some());
        assert_eq!(restored.default_workspace.as_deref(), Some("workspace"));
        assert_eq!(restored.theme.as_deref(), Some("dracula"));
        assert_eq!(restored.diff_view.as_deref(), Some("split"));
        assert_eq!(restored.diff_sidebar_visible, Some(false));
        assert_eq!(restored.diff_sidebar_width, Some(42));
        assert_eq!(restored.pr_states, Some(vec!["MERGED".to_string()]));
    }
}

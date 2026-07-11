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
    /// 起動前に保存済みの認証情報（Keychain の token と config）を消去する。
    #[arg(long)]
    reset_auth: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// 保存済みの認証情報（Keychain の token と config）を消去する。
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
        auth::delete_token(email)
            .with_context(|| format!("Keychain からの token 削除に失敗しました（{email}）"))?;
    }
    Config::clear().context("設定ファイルの削除に失敗しました")?;
    println!("認証情報を消去しました。");
    Ok(())
}

/// 設定を読み、可能ならクライアントを復元して TUI を起動する。
async fn run_tui() -> Result<()> {
    let config = Config::load().unwrap_or_default();
    let client = restore_client(&config);
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

/// config の email と Keychain の token から、可能ならクライアントを復元する。
///
/// token 未保存・Keychain アクセス不可などの場合は `None`（→ Onboarding へ）。
fn restore_client(config: &Config) -> Option<BitbucketClient> {
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
            tracing::warn!(%error, "Keychain からの token 読み出しに失敗しました");
            None
        }
    }
}

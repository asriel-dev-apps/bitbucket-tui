//! ログ初期化（tracing）。
//!
//! **`BBTUI_LOG` が設定されているときのみ**有効化し、出力先はキャッシュディレクトリ配下の
//! `bitbucket-tui.log`。TUI 実行中に stdout/stderr を汚さないため、ファイルにのみ書く。
//! `BBTUI_LOG` の値は `EnvFilter` のディレクティブとして解釈する（空なら `info`）。

use std::fs::OpenOptions;

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use crate::config::project_dirs;

/// ログを初期化する。`BBTUI_LOG` 未設定なら何もしない。
///
/// 二重初期化を避けるため `try_init` を使う。プロセス内で一度だけ呼ぶ想定。
pub fn init() -> Result<()> {
    let Ok(directive) = std::env::var("BBTUI_LOG") else {
        return Ok(());
    };

    let dirs = project_dirs()?;
    let cache_dir = dirs.cache_dir();
    std::fs::create_dir_all(cache_dir)
        .with_context(|| format!("ログディレクトリを作成できません: {}", cache_dir.display()))?;

    let log_path = cache_dir.join("bitbucket-tui.log");
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("ログファイルを開けません: {}", log_path.display()))?;

    let filter = if directive.trim().is_empty() {
        EnvFilter::new("info")
    } else {
        EnvFilter::new(directive)
    };

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_target(true)
        .with_writer(file)
        .try_init()
        .map_err(|error| anyhow::anyhow!("tracing の初期化に失敗しました: {error}"))
}

//! TUI レイヤ: 端末ガード（RAII + panic hook）・状態・イベントループ・描画。

pub mod app;
pub mod diff;
pub mod event;
pub mod logview;
pub mod onboarding;
pub mod theme;
pub mod ui;

use std::io::{self, Stdout};

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};

pub use app::App;
pub use event::run;

/// 端末を raw mode + alternate screen に切り替え、`Drop` で必ず復元する RAII ガード。
///
/// パニック時にも端末が壊れないよう、生成時に panic hook を仕込む。
pub struct Tui {
    pub terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    /// 端末を初期化する。以降 stdout はオルタネートスクリーンに切り替わる。
    pub fn init() -> Result<Self> {
        install_panic_hook();
        enable_raw_mode().context("raw mode への切り替えに失敗しました")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)
            .context("オルタネートスクリーンへの切り替えに失敗しました")?;
        let backend = CrosstermBackend::new(io::stdout());
        let terminal = Terminal::new(backend).context("端末バックエンドの初期化に失敗しました")?;
        Ok(Self { terminal })
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        // 復元失敗はどうにもできないので握りつぶす（TUI 中に stderr へ出さない方針）。
        let _ = restore();
    }
}

/// 端末を通常状態へ戻す。
fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen)?;
    Ok(())
}

/// パニック時に端末を復元してから既定のフックへ委譲する。
fn install_panic_hook() {
    let original = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // まず端末を復元し、その後でパニック情報を（通常状態の端末へ）出力させる。
        let _ = restore();
        original(info);
    }));
}

//! 配色テーマ機構。
//!
//! 追加クレートを増やさないため、パレットは `0xRRGGBB` の定数から `Color::Rgb` へ手動変換
//! する（`FromStr` によるパースは使わない＝パース失敗という分岐が原理的に存在しない）。
//! 色の決定ロジック（どの意味役割にどの hex を割り当てるか）は本モジュールに閉じ、
//! `ui.rs`/`diff.rs` は [`Theme`] のフィールド経由でのみ色を参照する。

use ratatui::style::Color;

/// 意味役割ベースの配色セット。
///
/// 描画コードは `Color::` を直接書かず、必ずこの構造体のフィールド経由で色を決める。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    /// 背景（モーダル等の明示的な塗りに使う。通常のペインではターミナル既定背景のまま）。
    pub bg: Color,
    /// 通常の前景（本文テキスト）。
    pub fg: Color,
    /// 補助的な低優先度テキスト（キャプション・タイムスタンプ等）。
    pub muted: Color,
    /// 非フォーカスのペインの枠線。
    pub border: Color,
    /// フォーカス中のペイン・単一ペイン画面の枠線。
    pub border_focus: Color,
    /// 強調色（タイトル・選択マーカー・アクティブフィールド等）。
    pub accent: Color,
    /// 成功・肯定（OPEN・成功ビルド・追加行・承認数 等）。
    pub success: Color,
    /// 警告・進行中・識別子バッジ（ID/ハッシュ等の装飾色も兼ねる）。
    pub warning: Color,
    /// 危険・破壊的操作・失敗・削除行。
    pub danger: Color,
    /// 補足情報（ブランチ参照・トリガ・author 等の二次テキスト）。
    pub info: Color,
    /// リスト選択行の背景。
    pub selection_bg: Color,
    /// リスト選択行の前景。
    pub selection_fg: Color,
}

/// 選択可能なテーマ名。`ThemeName::all()` の並びが `Ctrl+T` の巡回順になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeName {
    #[default]
    CatppuccinMocha,
    TokyoNight,
    Nord,
    Dracula,
    GruvboxDark,
    RosePine,
}

/// テーマ配色を組み立てるための元パレット（各テーマの hex 一式）。
struct Palette {
    bg: u32,
    fg: u32,
    accent: u32,
    green: u32,
    red: u32,
    yellow: u32,
    gray: u32,
}

/// `0xRRGGBB` を `Color::Rgb` に変換する。ビット演算とキャストのみで構成されるため
/// パニックし得ない（`FromStr` のようなパース失敗経路が存在しない）。
const fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

impl Palette {
    /// 役割マッピング: `muted`/`border` = gray、`border_focus`/`accent`/`info`/`selection_bg`
    /// = accent、`success` = green、`danger` = red、`warning` = yellow、`selection_fg` = bg。
    const fn into_theme(self) -> Theme {
        Theme {
            bg: rgb(self.bg),
            fg: rgb(self.fg),
            muted: rgb(self.gray),
            border: rgb(self.gray),
            border_focus: rgb(self.accent),
            accent: rgb(self.accent),
            success: rgb(self.green),
            warning: rgb(self.yellow),
            danger: rgb(self.red),
            info: rgb(self.accent),
            selection_bg: rgb(self.accent),
            selection_fg: rgb(self.bg),
        }
    }
}

const CATPPUCCIN_MOCHA: Palette = Palette {
    bg: 0x1e1e2e,
    fg: 0xcdd6f4,
    accent: 0xcba6f7,
    green: 0xa6e3a1,
    red: 0xf38ba8,
    yellow: 0xf9e2af,
    gray: 0x6c7086,
};

const TOKYO_NIGHT: Palette = Palette {
    bg: 0x1a1b26,
    fg: 0xc0caf5,
    accent: 0x7aa2f7,
    green: 0x9ece6a,
    red: 0xf7768e,
    yellow: 0xe0af68,
    gray: 0x565f89,
};

const NORD: Palette = Palette {
    bg: 0x2e3440,
    fg: 0xeceff4,
    accent: 0x88c0d0,
    green: 0xa3be8c,
    red: 0xbf616a,
    yellow: 0xebcb8b,
    gray: 0x4c566a,
};

const DRACULA: Palette = Palette {
    bg: 0x282a36,
    fg: 0xf8f8f2,
    accent: 0xbd93f9,
    green: 0x50fa7b,
    red: 0xff5555,
    yellow: 0xf1fa8c,
    gray: 0x6272a4,
};

const GRUVBOX_DARK: Palette = Palette {
    bg: 0x282828,
    fg: 0xebdbb2,
    accent: 0x83a598,
    green: 0xb8bb26,
    red: 0xfb4934,
    yellow: 0xfabd2f,
    gray: 0x928374,
};

const ROSE_PINE: Palette = Palette {
    bg: 0x191724,
    fg: 0xe0def4,
    accent: 0xc4a7e7,
    green: 0x31748f,
    red: 0xeb6f92,
    yellow: 0xf6c177,
    gray: 0x6e6a86,
};

impl ThemeName {
    /// 全テーマ名（`Ctrl+T` の巡回順）。
    pub const fn all() -> [ThemeName; 6] {
        [
            ThemeName::CatppuccinMocha,
            ThemeName::TokyoNight,
            ThemeName::Nord,
            ThemeName::Dracula,
            ThemeName::GruvboxDark,
            ThemeName::RosePine,
        ]
    }

    /// 次のテーマへ巡回する（末尾の次は先頭に戻る）。
    pub fn next(self) -> ThemeName {
        let all = Self::all();
        let index = all
            .iter()
            .position(|candidate| *candidate == self)
            .unwrap_or(0);
        all[(index + 1) % all.len()]
    }

    /// `config.toml` へ書き出す識別子。
    pub const fn as_str(self) -> &'static str {
        match self {
            ThemeName::CatppuccinMocha => "catppuccin-mocha",
            ThemeName::TokyoNight => "tokyo-night",
            ThemeName::Nord => "nord",
            ThemeName::Dracula => "dracula",
            ThemeName::GruvboxDark => "gruvbox-dark",
            ThemeName::RosePine => "rose-pine",
        }
    }

    /// 設定ファイルの文字列からテーマ名を復元する。未知の値は既定（Catppuccin Mocha）に
    /// フォールバックする（起動時に古い/壊れた設定でパニックしないため）。
    pub fn from_config_str(value: &str) -> ThemeName {
        Self::all()
            .into_iter()
            .find(|candidate| candidate.as_str() == value)
            .unwrap_or_default()
    }

    /// このテーマ名に対応する配色を組み立てる。
    pub const fn theme(self) -> Theme {
        match self {
            ThemeName::CatppuccinMocha => CATPPUCCIN_MOCHA.into_theme(),
            ThemeName::TokyoNight => TOKYO_NIGHT.into_theme(),
            ThemeName::Nord => NORD.into_theme(),
            ThemeName::Dracula => DRACULA.into_theme(),
            ThemeName::GruvboxDark => GRUVBOX_DARK.into_theme(),
            ThemeName::RosePine => ROSE_PINE.into_theme(),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        ThemeName::default().theme()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_returns_six_themes() {
        assert_eq!(ThemeName::all().len(), 6);
    }

    #[test]
    fn next_cycles_through_all_themes_and_wraps() {
        let mut current = ThemeName::default();
        let mut seen = Vec::new();
        for _ in 0..ThemeName::all().len() {
            seen.push(current);
            current = current.next();
        }
        // 6 回巡回するとちょうど全種を 1 回ずつ経由し、7 回目で先頭に戻る。
        assert_eq!(seen.len(), 6);
        for theme in ThemeName::all() {
            assert!(seen.contains(&theme));
        }
        assert_eq!(current, ThemeName::default());
    }

    #[test]
    fn next_from_last_wraps_to_first() {
        let last = *ThemeName::all().last().expect("all() is non-empty");
        assert_eq!(last.next(), ThemeName::default());
    }

    #[test]
    fn as_str_and_from_config_str_round_trip() {
        for theme in ThemeName::all() {
            assert_eq!(ThemeName::from_config_str(theme.as_str()), theme);
        }
    }

    #[test]
    fn from_config_str_falls_back_to_default_on_unknown_value() {
        assert_eq!(
            ThemeName::from_config_str("this-theme-does-not-exist"),
            ThemeName::default()
        );
        assert_eq!(ThemeName::from_config_str(""), ThemeName::default());
    }

    #[test]
    fn every_theme_builds_without_panicking_and_has_distinct_readable_roles() {
        for name in ThemeName::all() {
            let theme = name.theme();
            // 背景と前景・フォーカス枠と非フォーカス枠は必ず異なる色であるべき
            // （パレット表の転記ミスを検出するための最低限のサニティチェック）。
            assert_ne!(theme.bg, theme.fg);
            assert_ne!(theme.border, theme.border_focus);
            assert_eq!(theme.selection_fg, theme.bg);
        }
    }

    #[test]
    fn default_theme_is_catppuccin_mocha() {
        assert_eq!(ThemeName::default(), ThemeName::CatppuccinMocha);
        assert_eq!(Theme::default(), ThemeName::CatppuccinMocha.theme());
    }
}

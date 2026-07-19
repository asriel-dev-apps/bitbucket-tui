//! 設定ファイル（`directories` + `toml`）。
//!
//! 保存先は `ProjectDirs::from("dev", "", "bitbucket-tui")` の config ディレクトリ配下
//! `config.toml`。**token は含めない**（永続化する場合は OS セキュアストアのみ）。email は平文で保存してよい。

use std::path::PathBuf;

use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

/// 永続設定。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    /// Atlassian アカウントのメールアドレス（Basic 認証の username）。
    #[serde(default)]
    pub email: Option<String>,
    /// 表示名（`GET /2.0/user` の `display_name`）。任意。
    #[serde(default)]
    pub display_name: Option<String>,
    /// 既定のワークスペース slug。任意。
    #[serde(default)]
    pub default_workspace: Option<String>,
    /// 選択中のテーマ名（[`crate::tui::theme::ThemeName::as_str`] の値）。任意。
    /// 未設定/未知の値は既定テーマ（Catppuccin Mocha）にフォールバックする。
    #[serde(default)]
    pub theme: Option<String>,
    /// Diff 画面の表示モード（[`crate::tui::app::DiffViewMode::as_str`] の値、`v` で切替）。
    /// 任意。未設定/未知の値は既定（unified）にフォールバックする。
    #[serde(default)]
    pub diff_view: Option<String>,
    /// Diff 画面のファイル一覧サイドバーの表示/非表示（`t` で切替）。任意。未設定は既定
    /// （表示）にフォールバックする。
    #[serde(default)]
    pub diff_sidebar_visible: Option<bool>,
    /// Diff 画面のファイル一覧サイドバーの幅（セル数）。境界のマウスドラッグで変更した場合に
    /// 保存される。任意。未設定は既定の幅比率（[`crate::tui::app::DIFF_SIDEBAR_DEFAULT_PERCENT`]）
    /// にフォールバックする。
    #[serde(default)]
    pub diff_sidebar_width: Option<u16>,
    /// PR 一覧の state フィルタの選択（`"OPEN"`/`"MERGED"`/`"DECLINED"`/`"SUPERSEDED"` の配列）。
    /// 任意。読み込み時に不正値は無視し、未設定/全滅なら既定（OPEN のみ）にフォールバック
    /// する（[`crate::tui::app::PrStateFilter::from_config`]）。author フィルタは保存しない。
    #[serde(default)]
    pub pr_states: Option<Vec<String>>,
}

/// このアプリの `ProjectDirs` を返す。
pub fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("dev", "", "bitbucket-tui")
        .context("設定・キャッシュディレクトリを特定できませんでした（HOME 未設定の可能性）")
}

/// `config.toml` の絶対パス。
pub fn config_path() -> Result<PathBuf> {
    Ok(project_dirs()?.config_dir().join("config.toml"))
}

impl Config {
    /// 設定を読み込む。ファイルが無ければ既定値を返す。
    pub fn load() -> Result<Config> {
        let path = config_path()?;
        if !path.exists() {
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("設定ファイルを読み込めません: {}", path.display()))?;
        toml::from_str(&text)
            .with_context(|| format!("設定ファイルの解析に失敗しました: {}", path.display()))
    }

    /// 設定を `config.toml` に書き出す（ディレクトリが無ければ作成）。
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("設定ディレクトリを作成できません: {}", parent.display())
            })?;
        }
        let text = toml::to_string_pretty(self).context("設定の TOML 変換に失敗しました")?;
        std::fs::write(&path, text)
            .with_context(|| format!("設定ファイルを書き込めません: {}", path.display()))
    }

    /// 設定ファイルを削除する（存在しなくても成功扱い）。
    pub fn clear() -> Result<()> {
        let path = config_path()?;
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("設定ファイルを削除できません: {}", path.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_states_round_trip_through_toml() {
        let config = Config {
            pr_states: Some(vec!["OPEN".to_string(), "MERGED".to_string()]),
            ..Config::default()
        };
        let text = toml::to_string_pretty(&config).expect("serialize");
        let parsed: Config = toml::from_str(&text).expect("parse");
        assert_eq!(parsed.pr_states, config.pr_states);
    }

    #[test]
    fn config_without_pr_states_parses_as_none() {
        // 旧バージョンの config.toml（pr_states 無し）も読めること。
        let parsed: Config = toml::from_str("email = \"me@example.com\"\n").expect("parse");
        assert!(parsed.pr_states.is_none());
        assert_eq!(parsed.email.as_deref(), Some("me@example.com"));
    }

    #[test]
    fn pr_states_with_unknown_values_still_parse_as_strings() {
        // 不正値の無視は読み込み側（PrStateFilter::from_config）の責務。TOML としては
        // 文字列配列のまま保持する。
        let parsed: Config = toml::from_str("pr_states = [\"OPEN\", \"bogus\"]\n").expect("parse");
        assert_eq!(
            parsed.pr_states,
            Some(vec!["OPEN".to_string(), "bogus".to_string()])
        );
    }
}

//! 設定ファイル（`directories` + `toml`）。
//!
//! 保存先は `ProjectDirs::from("dev", "", "bitbucket-tui")` の config ディレクトリ配下
//! `config.toml`。**token は含めない**（token は Keychain のみ）。email は平文で保存してよい。

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

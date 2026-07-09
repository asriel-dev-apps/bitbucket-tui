//! 認証情報（API token）の保存/読込/削除。
//!
//! token は **OS Keychain（keyring crate）にのみ**保存し、平文ファイルには決して書かない。
//! keyring の `Entry` は `service = "bitbucket-tui"`、`user = <email>` で識別する。
//!
//! 注意（このビルド環境の制約）: `keyring` の OS バックエンド機能（macOS の `apple-native` 等）は
//! 現在の凍結された依存セットでは無効のため、実行時は **mock バックエンド**にフォールバックする。
//! そのため token はプロセス間で永続化されない。実 Keychain 連携には `security-framework` /
//! `core-foundation` の vendoring と `keyring` の `apple-native` feature 有効化が必要（要ネットワーク）。
//! コードは keyring API のみを使い平文保存は行わないため、依存を差し替えれば本番挙動になる。

use keyring::{Entry, Error as KeyringError};

/// keyring 上のサービス名。
const SERVICE: &str = "bitbucket-tui";

/// email に対応する keyring エントリを作る。
fn entry(email: &str) -> Result<Entry, KeyringError> {
    Entry::new(SERVICE, email)
}

/// API token を Keychain に保存する（既存があれば上書き）。
pub fn save_token(email: &str, token: &str) -> Result<(), KeyringError> {
    entry(email)?.set_password(token)
}

/// API token を Keychain から読み出す。未登録なら `Ok(None)`。
pub fn load_token(email: &str) -> Result<Option<String>, KeyringError> {
    match entry(email)?.get_password() {
        Ok(token) => Ok(Some(token)),
        Err(KeyringError::NoEntry) => Ok(None),
        Err(error) => Err(error),
    }
}

/// API token を Keychain から削除する。未登録でも成功扱い。
pub fn delete_token(email: &str) -> Result<(), KeyringError> {
    match entry(email)?.delete_credential() {
        Ok(()) => Ok(()),
        Err(KeyringError::NoEntry) => Ok(()),
        Err(error) => Err(error),
    }
}

//! Storage backends.
//!
//! Every backend implements [`StorageProvider`] with "keys" such as `backups/backup-full-….tar.zst`.
//! Object stores (S3, GCS, Azure, B2) use the key as the object name. Consumer cloud drives
//! (Google Drive, OneDrive, Dropbox, Box, pCloud, MEGA) store files flat in the configured folder
//! under the key's file name, and `list("backups/")` returns their keys as `backups/<name>`.

mod b2;
mod boxcom;
mod cloud_store;
mod dropbox;
mod gdrive;
mod http;
mod mega;
mod oauth;
mod onedrive;
mod pcloud;
mod s3;

use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::path::Path;

// async_trait's generated code trips clippy::double_must_use on newer toolchains.
#[allow(clippy::double_must_use)]
#[async_trait]
pub trait StorageProvider: Send + Sync {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()>;
    async fn download(&self, key: &str, output_path: &Path) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>>;
    async fn delete(&self, key: &str) -> Result<()>;
}

#[derive(Debug, Clone)]
pub struct BackupItem {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

/// All provider names accepted in `storage.provider`.
pub const PROVIDER_NAMES: &str = "s3, aws, contabo, digitalocean, wasabi, minio, r2, gcs, google, \
azure, b2, backblaze, googledrive, gdrive, onedrive, dropbox, box, pcloud, mega";

pub async fn from_config(config: &StorageConfig) -> Result<Box<dyn StorageProvider>> {
    let provider = config.provider.to_ascii_lowercase();
    Ok(match provider.as_str() {
        "s3" | "aws" | "contabo" | "digitalocean" | "wasabi" | "minio" | "r2" => {
            Box::new(s3::S3Provider::new(&provider, config).await?)
        }
        "gcs" | "google" => Box::new(cloud_store::ObjectStoreProvider::gcs(config)?),
        "azure" => Box::new(cloud_store::ObjectStoreProvider::azure(config)?),
        "b2" | "backblaze" => Box::new(b2::B2Provider::new(config).await?),
        "googledrive" | "gdrive" => Box::new(gdrive::GoogleDriveProvider::new(config)?),
        "onedrive" => Box::new(onedrive::OneDriveProvider::new(config)?),
        "dropbox" => Box::new(dropbox::DropboxProvider::new(config)?),
        "box" => Box::new(boxcom::BoxProvider::new(config)?),
        "pcloud" => Box::new(pcloud::PCloudProvider::new(config).await?),
        "mega" => Box::new(mega::MegaProvider::new(config)?),
        other => bail!(
            "Unknown provider: {:?}. Valid values: {}",
            other,
            PROVIDER_NAMES
        ),
    })
}

/// Non-empty value of an optional config field.
fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

fn required<'a>(value: &'a Option<String>, what: &str) -> Result<&'a str> {
    non_empty(value).with_context(|| format!("{} is required for this provider", what))
}

/// For flat-folder providers: the file name stored remotely for `key`.
fn file_name(key: &str) -> &str {
    key.rsplit('/').next().unwrap_or(key)
}

/// For flat-folder providers: turn a remote file name back into a key, if it matches `prefix`.
///
/// `prefix` ending with `/` (like `backups/`) is treated as a directory: every file in the
/// folder matches. Otherwise the file name must start with the part after the last `/`.
fn key_for(prefix: &str, name: &str) -> Option<String> {
    let (dir, name_prefix) = match prefix.rfind('/') {
        Some(i) => (&prefix[..=i], &prefix[i + 1..]),
        None => ("", prefix),
    };
    name.starts_with(name_prefix)
        .then(|| format!("{}{}", dir, name))
}

/// Normalise a folder path to `/A/B` form (`/` for the root).
fn folder_path(value: Option<&str>) -> String {
    let trimmed = value.unwrap_or("").trim().trim_matches('/');
    format!("/{}", trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_keys() {
        assert_eq!(file_name("backups/a.tar.zst"), "a.tar.zst");
        assert_eq!(
            key_for("backups/", "a.tar.zst").as_deref(),
            Some("backups/a.tar.zst")
        );
        assert_eq!(key_for("backups/backup-full", "backup-incr-1"), None);
        assert_eq!(key_for("", "x").as_deref(), Some("x"));
        assert_eq!(folder_path(None), "/");
        assert_eq!(folder_path(Some("Backups/")), "/Backups");
        assert_eq!(folder_path(Some("/a/b")), "/a/b");
    }
}

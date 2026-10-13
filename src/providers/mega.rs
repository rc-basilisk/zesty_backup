//! MEGA, through the official MEGAcmd tools (`mega-login`, `mega-put`, `mega-ls`, ...).
//! Docs: https://github.com/meganz/MEGAcmd/blob/master/UserGuide.md
//!
//! MEGA encrypts everything client-side, which MEGAcmd handles. Its tools talk to a background
//! `mega-cmd-server` that keeps the session, so the easiest setup is to run `mega-login` once
//! by hand and leave `account_name`/`account_key` out of the config.

use super::{file_name, folder_path, key_for, non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{NaiveDateTime, TimeZone, Utc};
use std::path::Path;
use tokio::process::Command;
use tracing::info;

pub struct MegaProvider {
    credentials: Option<(String, String)>,
    folder: String,
}

impl MegaProvider {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        if which::which("mega-whoami").is_err() {
            bail!("MEGAcmd is not installed or not in PATH (mega-whoami not found). Install it from https://mega.nz/cmd");
        }
        let credentials = match (non_empty(&config.account_name), non_empty(&config.account_key)) {
            (Some(email), Some(password)) => Some((email.to_string(), password.to_string())),
            (None, None) => None,
            _ => bail!("MEGA: set both account_name (email) and account_key (password), or neither and run mega-login yourself"),
        };
        Ok(Self {
            credentials,
            folder: folder_path(config.bucket_id.as_deref()),
        })
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{}", self.folder.trim_end_matches('/'), file_name(key))
    }

    async fn run(&self, tool: &str, args: &[&str]) -> Result<std::process::Output> {
        Command::new(tool)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .await
            .with_context(|| format!("Failed to run {} (is MEGAcmd installed?)", tool))
    }

    async fn run_ok(&self, tool: &str, args: &[&str]) -> Result<std::process::Output> {
        let output = self.run(tool, args).await?;
        if !output.status.success() {
            bail!(
                "{} failed: {}{}",
                tool,
                String::from_utf8_lossy(&output.stderr).trim(),
                String::from_utf8_lossy(&output.stdout).trim()
            );
        }
        Ok(output)
    }

    async fn ensure_logged_in(&self) -> Result<()> {
        if self.run("mega-whoami", &[]).await?.status.success() {
            return Ok(());
        }
        let Some((email, password)) = &self.credentials else {
            bail!("MEGA: not logged in. Run `mega-login <email>` once, or set account_name/account_key");
        };
        info!("Logging in to MEGA as {}", email);
        self.run_ok("mega-login", &[email, password]).await?;
        Ok(())
    }
}

#[async_trait]
impl StorageProvider for MegaProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        self.ensure_logged_in().await?;
        let local = file_path.to_str().context("Non UTF-8 file path")?;
        // -c creates the remote folder if needed
        self.run_ok("mega-put", &["-c", local, &self.path(key)])
            .await?;
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        self.ensure_logged_in().await?;
        // mega-get keeps the remote file name, so download into a scratch dir and move it.
        let parent = output_path.parent().unwrap_or(Path::new("."));
        let scratch = tempfile::Builder::new()
            .prefix(".mega-")
            .tempdir_in(parent)?;
        let dir = scratch.path().to_str().context("Non UTF-8 path")?;
        self.run_ok("mega-get", &[&self.path(key), dir]).await?;
        std::fs::rename(scratch.path().join(file_name(key)), output_path)
            .context("MEGA download finished but the file was not where expected")?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        self.ensure_logged_in().await?;
        let output = self
            .run(
                "mega-ls",
                &["-l", "--time-format=ISO6081_WITH_TIME", &self.folder],
            )
            .await?;
        if !output.status.success() {
            let err = String::from_utf8_lossy(&output.stderr);
            if err.contains("Couldn't find") || err.contains("not found") {
                return Ok(Vec::new()); // folder not created yet
            }
            bail!("mega-ls failed: {}", err.trim());
        }
        Ok(parse_ls(&String::from_utf8_lossy(&output.stdout), prefix))
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.ensure_logged_in().await?;
        self.run_ok("mega-rm", &[&self.path(key)]).await?;
        Ok(())
    }
}

/// Parse `mega-ls -l --time-format=ISO6081_WITH_TIME` output:
/// `FLAGS VERS SIZE DATE NAME`, e.g. `---- 1 1048576 2024-01-01T12:00:00 backup.tar.zst`.
/// The last FLAGS character is `d` for folders and `-` for files. Dates are UTC.
fn parse_ls(output: &str, prefix: &str) -> Vec<BackupItem> {
    output
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let flags = parts.next()?;
            let _versions = parts.next()?;
            let size: u64 = parts.next()?.parse().ok()?;
            let date = parts.next()?;
            let name = parts.collect::<Vec<_>>().join(" ");
            if flags.len() != 4 || !flags.ends_with('-') || name.is_empty() {
                return None;
            }
            Some(BackupItem {
                key: key_for(prefix, &name)?,
                size,
                last_modified: NaiveDateTime::parse_from_str(date, "%Y-%m-%dT%H:%M:%S")
                    .ok()
                    .map(|dt| Utc.from_utc_datetime(&dt)),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_ls_output() {
        let out = "FLAGS VERS SIZE DATE NAME\n\
                   ---- 1 2048 2024-01-01T12:00:00 backup-full-20240101-120000.tar.zst\n\
                   d--- - - 2024-01-01T12:00:00 somefolder\n";
        let items = super::parse_ls(out, "backups/");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].key, "backups/backup-full-20240101-120000.tar.zst");
        assert_eq!(items[0].size, 2048);
        assert!(items[0].last_modified.is_some());
    }
}

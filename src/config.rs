//! Configuration file format (`config.toml`).
//!
//! See `config.example.toml` at the repository root for a fully commented example.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub storage: StorageConfig,
    pub backup: BackupConfig,
    pub database: Option<DatabaseConfig>,
    pub system: Option<SystemConfig>,
    pub logging: Option<LoggingConfig>,
    pub encryption: Option<EncryptionConfig>,
    pub drill: Option<DrillConfig>,
    pub alerts: Option<AlertsConfig>,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self> {
        let content = fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path))?;
        Self::parse(&content).with_context(|| format!("Failed to parse config file: {}", path))
    }

    pub fn parse(content: &str) -> Result<Self> {
        let config: AppConfig = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if let Some(level) = self.backup.compression_level {
            anyhow::ensure!(
                (0..=22).contains(&level),
                "backup.compression_level must be between 0 and 22 (got {})",
                level
            );
        }
        if let Some(n) = self.backup.incremental_per_day {
            anyhow::ensure!(n > 0, "backup.incremental_per_day must be at least 1");
        }
        if let Some(drill) = &self.drill {
            anyhow::ensure!(
                drill.interval_hours != Some(0) && drill.max_age_hours != Some(0),
                "drill.interval_hours and drill.max_age_hours must be at least 1"
            );
            for pattern in drill.files.iter().flatten() {
                globset::Glob::new(pattern).map_err(|e| {
                    anyhow::anyhow!("drill.files: invalid pattern {:?}: {}", pattern, e)
                })?;
            }
        }
        if let Some(alerts) = &self.alerts {
            for (name, url) in [
                ("healthchecks_url", &alerts.healthchecks_url),
                ("healthchecks_drill_url", &alerts.healthchecks_drill_url),
                ("ntfy_url", &alerts.ntfy_url),
                ("webhook_url", &alerts.webhook_url),
            ] {
                if let Some(url) = url {
                    let parsed = url::Url::parse(url).map_err(|e| {
                        anyhow::anyhow!("alerts.{}: invalid URL {:?}: {}", name, url, e)
                    })?;
                    anyhow::ensure!(
                        matches!(parsed.scheme(), "http" | "https"),
                        "alerts.{} must be an http(s) URL",
                        name
                    );
                }
            }
        }
        if let Some(h) = self.backup.upload_interval_hours {
            anyhow::ensure!(h > 0, "backup.upload_interval_hours must be at least 1");
        }
        Ok(())
    }
}

/// Where backups are uploaded. Which fields are needed depends on `provider`;
/// see the README for a table.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub provider: String,
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Bucket (S3/GCS/B2) or container (Azure). Not used by consumer providers.
    #[serde(default)]
    pub bucket: String,
    /// S3 access key, or the OAuth access token for consumer providers.
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub account_key: Option<String>,
    pub application_key: Option<String>,
    /// B2 bucket ID, or the remote folder (ID or path) for consumer providers.
    pub bucket_id: Option<String>,
    pub credentials_path: Option<String>,
    /// Microsoft Entra tenant used to refresh OneDrive tokens (default: "common").
    pub tenant_id: Option<String>,
    /// Use path-style S3 URLs (`https://endpoint/bucket/key`). Defaults to true for MinIO and Contabo.
    pub force_path_style: Option<bool>,
    /// OAuth2 refresh token. When set together with `client_id`, access tokens are refreshed automatically.
    pub refresh_token: Option<String>,
    pub client_id: Option<String>,
    pub client_secret: Option<String>,
    /// File used to persist rotated OAuth refresh tokens (required for Box, whose refresh tokens are single-use).
    pub token_cache: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    pub local_backup_dir: String,
    pub project_path: String,
    pub additional_paths: Option<Vec<String>>,
    /// Used by `daemon` as the default backup frequency (24 / incremental_per_day hours).
    pub incremental_per_day: Option<u32>,
    /// Used by `daemon` as the default upload frequency.
    pub upload_interval_hours: Option<u32>,
    pub retention_days: Option<u32>,
    pub compression_level: Option<i32>,
    /// Only "zst" is supported; kept for backwards compatibility.
    pub compression_format: Option<String>,
    pub exclude: Option<Vec<String>>,
}

impl BackupConfig {
    pub fn retention_days(&self) -> u32 {
        self.retention_days.unwrap_or(7)
    }

    /// zstd level. 0 means "zstd default", which is currently 3.
    pub fn compression_level(&self) -> i32 {
        self.compression_level.unwrap_or(3)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub enabled: Option<bool>,
    #[serde(rename = "type")]
    pub db_type: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    /// Database name, or the path to the database file for SQLite.
    pub database: Option<String>,
    pub username: Option<String>,
    /// Can also come from the DB_PASSWORD env var or DATABASE_URL in `<project_path>/.env`.
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommandOutput {
    pub command: String,
    pub args: Option<Vec<String>>,
    pub output_file: String,
    pub enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemConfig {
    pub systemd_services: Option<Vec<String>>,
    pub systemd_timers: Option<Vec<String>>,
    pub command_outputs: Option<Vec<CommandOutput>>,
    pub presets: Option<PresetsConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetsConfig {
    pub nginx_sites: Option<Vec<String>>,
    pub nginx_enabled: Option<bool>,
    pub crontab_enabled: Option<bool>,
    pub crontab_user: Option<String>,
    pub user_configs: Option<Vec<String>>,
    pub user_configs_home: Option<String>,
    pub etc_files: Option<Vec<String>>,
    pub etc_dirs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// trace, debug, info, warn or error. RUST_LOG overrides it.
    pub level: Option<String>,
    /// When set, logs are also appended to `<log_dir>/zesty-backup.log`.
    pub log_dir: Option<String>,
}

/// Optional age encryption of archives (see `encryption.rs`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EncryptionConfig {
    /// Defaults to true when the section is present.
    pub enabled: Option<bool>,
    /// age public keys (`age1…`, from `age-keygen`). Recommended.
    pub recipients: Option<Vec<String>>,
    /// File with one public key per line.
    pub recipients_file: Option<String>,
    /// Alternative to recipients. Prefer the ZESTY_BACKUP_PASSPHRASE env var.
    pub passphrase: Option<String>,
    /// Private key file used by `restore`/`verify` (not needed for creating backups).
    pub identity_file: Option<String>,
}

/// Where to report successes and failures (see `alerts.rs`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AlertsConfig {
    /// healthchecks.io (or self-hosted) ping URL: pinged on every success, `/fail` on failures.
    pub healthchecks_url: Option<String>,
    /// Separate check for restore drills (default: drill failures go to healthchecks_url).
    pub healthchecks_drill_url: Option<String>,
    /// ntfy topic URL, e.g. https://ntfy.sh/my-secret-topic
    pub ntfy_url: Option<String>,
    pub ntfy_token: Option<String>,
    /// Slack/Discord incoming webhook, or any URL accepting a JSON POST.
    pub webhook_url: Option<String>,
    /// Also send ntfy/webhook messages for successes (default: failures only).
    pub notify_success: Option<bool>,
    /// Name used in messages (default: the hostname).
    pub server_name: Option<String>,
}

/// Restore drills (`zesty-backup drill`): restore the latest backup somewhere temporary
/// and check that it is actually usable.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrillConfig {
    /// Fail when the newest backup is older than this.
    pub max_age_hours: Option<u64>,
    /// Glob patterns (archive paths, e.g. "project/myapp/.env") that must match at least one file.
    pub files: Option<Vec<String>>,
    /// Shell commands run inside the restored folder; each must exit with 0.
    pub commands: Option<Vec<DrillCommand>>,
    /// Load the database dump into a throwaway container (default: true when [database] is enabled).
    pub database_restore: Option<bool>,
    /// Container image for that (default: chosen from the version in the dump, e.g. postgres:16).
    pub database_image: Option<String>,
    /// SQL queries run on the restored database; each must return a value that is not empty, 0 or false.
    pub database_checks: Option<Vec<String>>,
    /// "docker" (default) or "podman".
    pub container_runtime: Option<String>,
    /// Where the backup is downloaded and extracted (default: the system temp dir).
    pub work_dir: Option<String>,
    /// When set, `daemon` runs a drill this often.
    pub interval_hours: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DrillCommand {
    /// Shown in the report (default: the command itself).
    pub name: Option<String>,
    /// Run with `sh -c` in the restored folder; $ZESTY_RESTORE_DIR points to it.
    pub run: String,
}

impl DrillConfig {
    pub fn runtime(&self) -> &str {
        self.container_runtime.as_deref().unwrap_or("docker")
    }
}

pub const LOG_FILE_NAME: &str = "zesty-backup.log";

impl LoggingConfig {
    pub fn log_file(&self) -> Option<std::path::PathBuf> {
        self.log_dir
            .as_ref()
            .map(|dir| Path::new(dir).join(LOG_FILE_NAME))
    }
}

/// The commented example shipped with the project, also written by `generate-config`.
pub const EXAMPLE_CONFIG: &str = include_str!("../config.example.toml");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let config = AppConfig::parse(EXAMPLE_CONFIG).expect("example config must parse");
        assert_eq!(config.storage.provider, "s3");
        assert_eq!(config.backup.compression_level(), 3);
    }

    #[test]
    fn consumer_provider_does_not_need_bucket() {
        let config = AppConfig::parse(
            r#"
            [storage]
            provider = "dropbox"
            access_key = "token"
            [backup]
            local_backup_dir = "./backups"
            project_path = "."
            "#,
        )
        .unwrap();
        assert_eq!(config.storage.bucket, "");
    }

    #[test]
    fn rejects_typos() {
        let err = AppConfig::parse(
            r#"
            [storage]
            provider = "s3"
            bukcet = "oops"
            [backup]
            local_backup_dir = "./backups"
            project_path = "."
            "#,
        )
        .unwrap_err();
        assert!(format!("{:#}", err).contains("bukcet"));
    }

    #[test]
    fn rejects_bad_compression_level() {
        let err = AppConfig::parse(
            r#"
            [storage]
            provider = "s3"
            [backup]
            local_backup_dir = "./backups"
            project_path = "."
            compression_level = 40
            "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("compression_level"));
    }
}

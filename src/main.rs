mod providers;

use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use clap::{Parser, Subcommand};
use providers::{Provider, StorageConfig as ProviderStorageConfig, StorageProvider};
use serde::Deserialize;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use tar::Builder;
use tracing::{info, warn};
use walkdir::WalkDir;
use zstd::Encoder;

#[derive(Parser)]
#[command(name = "zesty-backup")]
#[command(about = "A flexible, multi-provider backup utility for cloud storage")]
#[command(version)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, global = true)]
    config: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new backup
    Backup {
        /// Force full backup (ignore incremental)
        #[arg(long)]
        full: bool,
    },
    /// Upload local backups to cloud storage
    Upload {
        /// Upload specific backup file
        #[arg(short, long)]
        file: Option<String>,
    },
    /// List available backups (local and remote)
    List {
        /// Show remote backups only
        #[arg(long)]
        remote: bool,
    },
    /// Download backup from cloud storage
    Download {
        /// Backup key/name to download
        key: String,
        /// Output directory
        #[arg(short, long, default_value = "./restored")]
        output: String,
    },
    /// Clean old backups (local and remote)
    Clean {
        /// Dry run (don't actually delete)
        #[arg(long)]
        dry_run: bool,
    },
    /// Restore from backup
    Restore {
        /// Backup file path
        file: String,
        /// Target directory
        #[arg(short, long)]
        target: Option<String>,
    },
    /// Run as daemon (background service)
    Daemon {
        /// Interval between backups in hours
        #[arg(short, long, default_value = "6")]
        backup_interval: u64,
        /// Interval between uploads in hours
        #[arg(short, long, default_value = "24")]
        upload_interval: u64,
        /// PID file path
        #[arg(short, long, default_value = "/var/run/zesty-backup.pid")]
        pid_file: String,
    },
    /// Client-only operations (for desktop access)
    Client {
        /// Use config file instead of command-line credentials
        #[arg(short, long)]
        config: Option<String>,
        /// Storage provider (s3, gcs, azure, b2)
        #[arg(short, long)]
        provider: Option<String>,
        /// Endpoint URL (for S3-compatible providers)
        #[arg(short, long)]
        endpoint: Option<String>,
        /// Region (for S3-compatible providers)
        #[arg(short, long)]
        region: Option<String>,
        /// Bucket/container name
        #[arg(short, long)]
        bucket: Option<String>,
        /// Access key (for S3-compatible providers)
        #[arg(short, long)]
        access_key: Option<String>,
        /// Secret key (for S3-compatible providers)
        #[arg(short, long)]
        secret_key: Option<String>,
        #[command(subcommand)]
        operation: ClientOperation,
    },
    /// Generate an example configuration file
    GenerateConfig {
        /// Output path for the config file
        #[arg(short, long, default_value = "config.toml.example")]
        output: String,
    },
    /// Show status information
    Status,
    /// Show recent logs
    Logs {
        /// Number of log lines to show
        #[arg(short, long, default_value = "50")]
        lines: usize,
    },
}

#[derive(Subcommand)]
enum ClientOperation {
    /// List remote backups
    List,
    /// Download a backup
    Download {
        /// Backup key/name to download
        key: String,
        /// Output directory
        #[arg(short, long, default_value = "./restored")]
        output: String,
    },
}

#[derive(Debug, Deserialize)]
struct AppConfig {
    storage: StorageConfig,
    backup: BackupConfig,
    database: Option<DatabaseConfig>,
    system: Option<SystemConfig>,
    logging: Option<LoggingConfig>,
}

#[derive(Debug, Deserialize)]
struct StorageConfig {
    provider: String,
    endpoint: Option<String>,
    region: Option<String>,
    bucket: String,
    access_key: Option<String>,
    secret_key: Option<String>,
    // Optional fields for different providers
    account_id: Option<String>,
    account_name: Option<String>,
    account_key: Option<String>,
    application_key: Option<String>,
    bucket_id: Option<String>,
    credentials_path: Option<String>,
    tenant_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BackupConfig {
    local_backup_dir: String,
    project_path: String,
    additional_paths: Option<Vec<String>>,
    #[allow(dead_code)]
    incremental_per_day: Option<u32>,
    #[allow(dead_code)]
    upload_interval_hours: Option<u32>,
    retention_days: Option<u32>,
    compression_level: Option<u32>,
    #[allow(dead_code)]
    compression_format: Option<String>,
    exclude: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct DatabaseConfig {
    enabled: Option<bool>,
    #[serde(rename = "type")]
    db_type: Option<String>, // postgres, mariadb, mysql, cassandra, scylla, mongodb, redis, sqlite, etc.
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    username: Option<String>,
    password: Option<String>, // Can also use DB_PASSWORD env var
}

#[derive(Debug, Deserialize)]
struct CommandOutput {
    command: String,
    args: Option<Vec<String>>,
    output_file: String,
    enabled: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SystemConfig {
    // Systemd services and timers
    systemd_services: Option<Vec<String>>,
    systemd_timers: Option<Vec<String>>,

    // Command outputs to capture
    command_outputs: Option<Vec<CommandOutput>>,

    // Presets for common scenarios
    presets: Option<PresetsConfig>,
}

#[derive(Debug, Deserialize)]
struct PresetsConfig {
    // Nginx presets
    nginx_sites: Option<Vec<String>>, // List of site names to backup configs for
    nginx_enabled: Option<bool>,      // Backup /etc/nginx/nginx.conf and sites-available/enabled

    // Crontab
    crontab_enabled: Option<bool>, // Backup user crontab
    crontab_user: Option<String>,  // User to backup crontab for (default: current user)

    // User config files
    user_configs: Option<Vec<String>>, // e.g., [".zshrc", ".bashrc", ".vimrc"]
    user_configs_home: Option<String>, // Home directory (default: $HOME)

    // Common system files
    etc_files: Option<Vec<String>>, // Files in /etc/ to backup
    etc_dirs: Option<Vec<String>>,  // Directories in /etc/ to backup
}

#[derive(Debug, Deserialize)]
struct LoggingConfig {
    #[allow(dead_code)]
    level: Option<String>,
    log_dir: Option<String>,
}

struct BackupManager {
    config: Option<AppConfig>,
    provider: Option<Provider>,
}

impl BackupManager {
    async fn new(config_path: Option<&str>) -> Result<Self> {
        if let Some(path) = config_path {
            let config_content = fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file: {}", path))?;
            let config: AppConfig =
                toml::from_str(&config_content).context("Failed to parse config file")?;

            // Convert to provider storage config
            let provider_config = ProviderStorageConfig {
                provider: config.storage.provider.clone(),
                endpoint: config.storage.endpoint.clone().unwrap_or_default(),
                region: config
                    .storage
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
                bucket: config.storage.bucket.clone(),
                access_key: config.storage.access_key.clone().unwrap_or_default(),
                secret_key: config.storage.secret_key.clone().unwrap_or_default(),
                account_id: config.storage.account_id.clone(),
                account_name: config.storage.account_name.clone(),
                account_key: config.storage.account_key.clone(),
                application_key: config.storage.application_key.clone(),
                bucket_id: config.storage.bucket_id.clone(),
                credentials_path: config.storage.credentials_path.clone(),
                tenant_id: config.storage.tenant_id.clone(),
            };

            let provider = Provider::from_config(&provider_config).await?;

            Ok(Self {
                config: Some(config),
                provider: Some(provider),
            })
        } else {
            Ok(Self {
                config: None,
                provider: None,
            })
        }
    }

    async fn new_client(provider_config: ProviderStorageConfig) -> Result<Self> {
        let provider = Provider::from_config(&provider_config).await?;
        Ok(Self {
            config: None,
            provider: Some(provider),
        })
    }

    fn get_provider(&self) -> Result<&Provider> {
        self.provider
            .as_ref()
            .context("Storage provider not initialized")
    }

    async fn create_backup(&self, full: bool) -> Result<PathBuf> {
        let config = self
            .config
            .as_ref()
            .context("Backup creation requires server configuration")?;

        info!("Starting backup creation...");

        // Create backup directory
        fs::create_dir_all(&config.backup.local_backup_dir)
            .context("Failed to create backup directory")?;

        let timestamp = Local::now().format("%Y%m%d-%H%M%S");
        let backup_name = if full {
            format!("backup-full-{}.tar.zst", timestamp)
        } else {
            format!("backup-incr-{}.tar.zst", timestamp)
        };
        let backup_path = Path::new(&config.backup.local_backup_dir).join(&backup_name);

        info!("Creating backup: {}", backup_path.display());

        // Create tar archive with zstd compression
        let compression_level = config.backup.compression_level.unwrap_or(3) as i32;
        let file = fs::File::create(&backup_path).context("Failed to create backup file")?;
        let encoder = Encoder::new(file, compression_level)?;
        let mut tar = Builder::new(encoder);

        // Backup main project
        info!("Backing up project: {}", config.backup.project_path);
        self.add_directory_to_tar(&mut tar, &config.backup.project_path, "project")
            .context("Failed to backup project directory")?;

        // Backup additional paths
        if let Some(ref additional_paths) = config.backup.additional_paths {
            for path in additional_paths {
                if Path::new(path).exists() {
                    info!("Backing up: {}", path);
                    if Path::new(path).is_dir() {
                        let name = Path::new(path)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("unknown");
                        self.add_directory_to_tar(&mut tar, path, &format!("system/{}", name))
                            .with_context(|| format!("Failed to backup directory: {}", path))?;
                    } else if let Ok(mut file) = fs::File::open(path) {
                        let mut contents = Vec::new();
                        file.read_to_end(&mut contents)?;
                        let archive_path = format!(
                            "system/{}",
                            Path::new(path)
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                        );
                        let mut header = tar::Header::new_gnu();
                        header.set_path(&archive_path)?;
                        header.set_size(contents.len() as u64);
                        header.set_cksum();
                        tar.append(&header, contents.as_slice())?;
                    }
                } else {
                    warn!("Path does not exist: {}", path);
                }
            }
        }

        // Backup system configuration
        if let Some(ref system_config) = config.system {
            // Backup systemd services
            if let Some(ref services) = system_config.systemd_services {
                info!("Backing up systemd services...");
                for service in services {
                    let service_path = format!("/etc/systemd/system/{}", service);
                    if Path::new(&service_path).exists() {
                        if let Ok(mut file) = fs::File::open(&service_path) {
                            let mut contents = Vec::new();
                            file.read_to_end(&mut contents)?;
                            let archive_path = format!("systemd/services/{}", service);
                            let mut header = tar::Header::new_gnu();
                            header.set_path(&archive_path)?;
                            header.set_size(contents.len() as u64);
                            header.set_cksum();
                            tar.append(&header, contents.as_slice())?;
                        }
                    }
                }
            }

            // Backup systemd timers
            if let Some(ref timers) = system_config.systemd_timers {
                for timer in timers {
                    let timer_path = format!("/etc/systemd/system/{}", timer);
                    if Path::new(&timer_path).exists() {
                        if let Ok(mut file) = fs::File::open(&timer_path) {
                            let mut contents = Vec::new();
                            file.read_to_end(&mut contents)?;
                            let archive_path = format!("systemd/timers/{}", timer);
                            let mut header = tar::Header::new_gnu();
                            header.set_path(&archive_path)?;
                            header.set_size(contents.len() as u64);
                            header.set_cksum();
                            tar.append(&header, contents.as_slice())?;
                        }
                    }
                }
            }

            // Apply presets
            if let Some(ref presets) = system_config.presets {
                self.apply_presets(&mut tar, presets)?;
            }

            // Backup command outputs
            if let Some(ref commands) = system_config.command_outputs {
                info!("Backing up command outputs...");
                for cmd_output in commands {
                    if cmd_output.enabled.unwrap_or(true) {
                        self.backup_command_output(&mut tar, cmd_output)?;
                    }
                }
            }
        }

        // Backup database
        if let Some(ref db_config) = config.database {
            if db_config.enabled.unwrap_or(false) {
                info!("Backing up database...");
                self.backup_database(&mut tar, config)
                    .await
                    .context("Failed to backup database")?;
            }
        }

        // Finish archive
        tar.finish().context("Failed to finish tar archive")?;

        info!("Backup created successfully: {}", backup_path.display());
        Ok(backup_path)
    }

    fn add_directory_to_tar(
        &self,
        tar: &mut Builder<Encoder<'_, fs::File>>,
        path: &str,
        prefix: &str,
    ) -> Result<()> {
        let base_path = Path::new(path);
        let walker = WalkDir::new(path).follow_links(false);

        // Get exclude patterns from config
        let exclude_patterns = if let Some(ref config) = self.config {
            config.backup.exclude.as_deref().unwrap_or(&[])
        } else {
            &[]
        };

        for entry in walker {
            let entry = entry.context("Failed to read directory entry")?;
            let entry_path = entry.path();

            // Check if path should be excluded
            let should_exclude = exclude_patterns
                .iter()
                .any(|pattern| entry_path.to_string_lossy().contains(pattern));
            if should_exclude {
                continue;
            }

            // Skip directories (tar handles them automatically)
            if entry_path.is_dir() {
                continue;
            }

            // Calculate relative path
            let relative_path = entry_path
                .strip_prefix(base_path.parent().unwrap_or(base_path))
                .or_else(|_| entry_path.strip_prefix(base_path))
                .unwrap_or(entry_path);

            let archive_path = if prefix.is_empty() {
                relative_path.to_string_lossy().to_string()
            } else {
                format!("{}/{}", prefix, relative_path.to_string_lossy())
            };

            if let Ok(mut file) = fs::File::open(entry_path) {
                let mut contents = Vec::new();
                if file.read_to_end(&mut contents).is_ok() {
                    let mut header = tar::Header::new_gnu();
                    if header.set_path(&archive_path).is_ok() {
                        header.set_size(contents.len() as u64);
                        header.set_cksum();
                        if tar.append(&header, contents.as_slice()).is_ok() {
                            continue;
                        }
                    }
                }
            }

            // Fallback: try append_path_with_name
            tar.append_path_with_name(entry_path, &archive_path)
                .with_context(|| {
                    format!("Failed to add file to archive: {}", entry_path.display())
                })?;
        }
        Ok(())
    }

    async fn backup_database(
        &self,
        tar: &mut Builder<Encoder<'_, fs::File>>,
        config: &AppConfig,
    ) -> Result<()> {
        let db_config = config
            .database
            .as_ref()
            .context("Database config not found")?;

        let db_type = db_config.db_type.as_deref().unwrap_or("postgres");
        let host = db_config
            .host
            .as_ref()
            .context("Database host not configured")?;
        let port = db_config.port.context("Database port not configured")?;
        let database = db_config
            .database
            .as_ref()
            .context("Database name not configured")?;
        let username = db_config
            .username
            .as_ref()
            .context("Database username not configured")?;

        // Try to get password from config, environment, or .env file
        let db_password = db_config.password.clone()
            .or_else(|| std::env::var("DB_PASSWORD").ok())
            .or_else(|| {
                let env_path = format!("{}/.env", config.backup.project_path);
                if Path::new(&env_path).exists() {
                    if let Ok(content) = fs::read_to_string(&env_path) {
                        for line in content.lines() {
                            if line.starts_with("DATABASE_URL=") {
                                // Extract password from postgresql://user:pass@host/db
                                if let Some(start) = line.find("://") {
                                    let rest = &line[start + 3..];
                                    if let Some(at) = rest.find('@') {
                                        let user_pass = &rest[..at];
                                        if let Some(colon) = user_pass.find(':') {
                                            return Some(user_pass[colon + 1..].to_string());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                None
            })
            .context("Database password not found. Set password in config, DB_PASSWORD env var, or .env file")?;

        let dump_file = format!(
            "/tmp/backup_db_{}_{}.dump",
            database,
            Local::now().format("%Y%m%d-%H%M%S")
        );

        let output = match db_type.to_lowercase().as_str() {
            "postgres" | "postgresql" => {
                let mut cmd = Command::new("pg_dump");
                cmd.arg("-h").arg(host)
                    .arg("-p").arg(port.to_string())
                    .arg("-U").arg(username)
                    .arg("-d").arg(database)
                    .arg("-F").arg("plain");
                if db_type == "postgres" || db_type == "postgresql" {
                    cmd.env("PGPASSWORD", &db_password);
                }
                cmd.output()
            }
            "mariadb" | "mysql" => {
                Command::new("mysqldump")
                    .arg(format!("-h{}", host))
                    .arg(format!("-P{}", port))
                    .arg(format!("-u{}", username))
                    .arg(format!("-p{}", db_password))
                    .arg(database)
                    .output()
            }
            "mongodb" => {
                Command::new("mongodump")
                    .arg(format!("--host={}:{}", host, port))
                    .arg(format!("--username={}", username))
                    .arg(format!("--password={}", db_password))
                    .arg(format!("--db={}", database))
                    .arg("--archive")
                    .output()
            }
            "cassandra" | "scylla" => {
                Command::new("cqlsh")
                    .arg(host)
                    .arg(format!("{}", port))
                    .arg("-u").arg(username)
                    .arg("-p").arg(&db_password)
                    .arg("-e").arg(format!("DESCRIBE KEYSPACE {};", database))
                    .output()
            }
            "redis" => {
                Command::new("redis-cli")
                    .arg("-h").arg(host)
                    .arg("-p").arg(port.to_string())
                    .arg("-a").arg(&db_password)
                    .arg("--rdb").arg(&dump_file)
                    .output()
            }
            "sqlite" => {
                // SQLite doesn't need dump command, just copy the file
                if Path::new(database).exists() {
                    let contents = fs::read(database)
                        .with_context(|| format!("Failed to read SQLite database: {}", database))?;
                    fs::write(&dump_file, contents)
                        .context("Failed to write SQLite dump file")?;
                    self.add_file_to_tar(tar, &PathBuf::from(&dump_file),
                        &format!("database/{}.sqlite", database))?;
                    fs::remove_file(&dump_file).ok(); // Clean up
                    return Ok(());
                } else {
                    return Err(anyhow::anyhow!("SQLite database file not found: {}", database));
                }
            }
            _ => {
                return Err(anyhow::anyhow!("Unsupported database type: {}. Supported: postgres, mariadb, mysql, mongodb, cassandra, scylla, redis, sqlite", db_type));
            }
        }
        .context(format!("Failed to execute {} dump command", db_type))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(anyhow::anyhow!("Database dump failed: {}", stderr));
        }

        // For MongoDB, the output is already in the dump_file
        if db_type == "mongodb" {
            // mongodump creates a directory, we need to handle it differently
            warn!("MongoDB backup creates a directory structure. Consider using command_outputs pattern instead.");
            return Ok(());
        }

        fs::write(&dump_file, &output.stdout).context("Failed to write database dump")?;

        let extension = match db_type {
            "postgres" | "postgresql" | "mariadb" | "mysql" => "sql",
            "cassandra" | "scylla" => "cql",
            "redis" => "rdb",
            _ => "dump",
        };

        self.add_file_to_tar(
            tar,
            &PathBuf::from(&dump_file),
            &format!("database/{}.{}", database, extension),
        )?;

        fs::remove_file(&dump_file).ok(); // Clean up
        Ok(())
    }

    fn add_file_to_tar(
        &self,
        tar: &mut Builder<Encoder<'_, fs::File>>,
        file_path: &PathBuf,
        archive_path: &str,
    ) -> Result<()> {
        if let Ok(mut file) = fs::File::open(file_path) {
            let mut contents = Vec::new();
            file.read_to_end(&mut contents)?;
            let mut header = tar::Header::new_gnu();
            header.set_path(archive_path)?;
            header.set_size(contents.len() as u64);
            header.set_cksum();
            tar.append(&header, contents.as_slice())?;
        }
        Ok(())
    }

    fn backup_command_output(
        &self,
        tar: &mut Builder<Encoder<'_, fs::File>>,
        cmd_output: &CommandOutput,
    ) -> Result<()> {
        info!("Executing command: {}", cmd_output.command);

        let mut cmd = Command::new(&cmd_output.command);
        if let Some(ref args) = cmd_output.args {
            cmd.args(args);
        }

        let output = cmd
            .output()
            .with_context(|| format!("Failed to execute command: {}", cmd_output.command))?;

        if output.status.success() {
            let content = String::from_utf8_lossy(&output.stdout);
            let mut header = tar::Header::new_gnu();
            header
                .set_path(format!("commands/{}", cmd_output.output_file))
                .context("Failed to set path in tar header")?;
            header.set_size(content.len() as u64);
            header.set_cksum();
            tar.append(&header, content.as_bytes()).with_context(|| {
                format!(
                    "Failed to add command output to archive: {}",
                    cmd_output.output_file
                )
            })?;
            info!(
                "Successfully backed up command output: {}",
                cmd_output.output_file
            );
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            warn!("Command failed: {} - {}", cmd_output.command, stderr);
        }

        Ok(())
    }

    fn apply_presets(
        &self,
        tar: &mut Builder<Encoder<'_, fs::File>>,
        presets: &PresetsConfig,
    ) -> Result<()> {
        // Nginx presets
        if presets.nginx_enabled.unwrap_or(false) {
            info!("Backing up nginx configuration...");

            // Backup main nginx config
            let nginx_conf = "/etc/nginx/nginx.conf";
            if Path::new(nginx_conf).exists() {
                self.add_file_to_tar(tar, &PathBuf::from(nginx_conf), "system/nginx/nginx.conf")?;
            }

            // Backup sites-available and sites-enabled
            let sites_available = "/etc/nginx/sites-available";
            let sites_enabled = "/etc/nginx/sites-enabled";

            if Path::new(sites_available).exists() {
                self.add_directory_to_tar(tar, sites_available, "system/nginx/sites-available")?;
            }
            if Path::new(sites_enabled).exists() {
                self.add_directory_to_tar(tar, sites_enabled, "system/nginx/sites-enabled")?;
            }
        }

        // Backup specific nginx sites
        if let Some(ref sites) = presets.nginx_sites {
            for site in sites {
                info!("Backing up nginx site: {}", site);
                let site_available = format!("/etc/nginx/sites-available/{}", site);
                let site_enabled = format!("/etc/nginx/sites-enabled/{}", site);

                if Path::new(&site_available).exists() {
                    self.add_file_to_tar(
                        tar,
                        &PathBuf::from(&site_available),
                        &format!("system/nginx/sites-available/{}", site),
                    )?;
                }
                if Path::new(&site_enabled).exists() {
                    self.add_file_to_tar(
                        tar,
                        &PathBuf::from(&site_enabled),
                        &format!("system/nginx/sites-enabled/{}", site),
                    )?;
                }
            }
        }

        // Crontab
        if presets.crontab_enabled.unwrap_or(false) {
            info!("Backing up crontab...");
            let user = presets
                .crontab_user
                .clone()
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "root".to_string()));
            let current_user = std::env::var("USER").unwrap_or_default();

            let output = if user == "root" || user == current_user {
                Command::new("crontab").arg("-l").output()
            } else {
                Command::new("crontab")
                    .arg("-u")
                    .arg(&user)
                    .arg("-l")
                    .output()
            };

            if let Ok(cron_output) = output {
                if cron_output.status.success() {
                    let content = String::from_utf8_lossy(&cron_output.stdout);
                    let mut header = tar::Header::new_gnu();
                    header.set_path(format!("system/crontab-{}.txt", user))?;
                    header.set_size(content.len() as u64);
                    header.set_cksum();
                    tar.append(&header, content.as_bytes())?;
                }
            }
        }

        // User config files
        if let Some(ref configs) = presets.user_configs {
            let home_dir_str = presets
                .user_configs_home
                .clone()
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()));
            let home_dir = home_dir_str.as_str();

            info!("Backing up user config files from: {}", home_dir);
            for config_file in configs {
                let config_path = Path::new(home_dir).join(config_file);
                if config_path.exists() {
                    let archive_path = format!("user-configs/{}", config_file);
                    if config_path.is_file() {
                        self.add_file_to_tar(tar, &config_path, &archive_path)?;
                    } else if config_path.is_dir() {
                        self.add_directory_to_tar(
                            tar,
                            config_path.to_str().unwrap(),
                            &format!("user-configs/{}", config_file),
                        )?;
                    }
                }
            }
        }

        // /etc files
        if let Some(ref etc_files) = presets.etc_files {
            for etc_file in etc_files {
                let etc_path = Path::new("/etc").join(etc_file);
                if etc_path.exists() {
                    let archive_path = format!("etc/{}", etc_file);
                    if etc_path.is_file() {
                        self.add_file_to_tar(tar, &etc_path, &archive_path)?;
                    } else if etc_path.is_dir() {
                        self.add_directory_to_tar(tar, etc_path.to_str().unwrap(), &archive_path)?;
                    }
                }
            }
        }

        // /etc directories
        if let Some(ref etc_dirs) = presets.etc_dirs {
            for etc_dir in etc_dirs {
                let etc_path = Path::new("/etc").join(etc_dir);
                if etc_path.exists() && etc_path.is_dir() {
                    self.add_directory_to_tar(
                        tar,
                        etc_path.to_str().unwrap(),
                        &format!("etc/{}", etc_dir),
                    )?;
                }
            }
        }

        Ok(())
    }

    async fn upload_backup(&self, backup_path: Option<&str>) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .context("Upload requires server configuration")?;
        let provider = self.get_provider()?;

        let backups_to_upload = if let Some(path) = backup_path {
            vec![PathBuf::from(path)]
        } else {
            // Find all local backups
            let backup_dir = Path::new(&config.backup.local_backup_dir);
            let mut backups: Vec<PathBuf> = fs::read_dir(backup_dir)
                .context("Failed to read backup directory")?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|s| s.to_str())
                        .map(|s| s == "zst")
                        .unwrap_or(false)
                })
                .collect();
            backups.sort();
            backups
        };

        for backup_path in backups_to_upload {
            let file_name = backup_path
                .file_name()
                .and_then(|n| n.to_str())
                .context("Invalid backup file name")?;
            let storage_key = format!("backups/{}", file_name);

            info!("Uploading {} to {}...", file_name, config.storage.provider);
            provider.upload(&storage_key, &backup_path).await?;
        }

        Ok(())
    }

    async fn list_backups(&self, remote: bool) -> Result<()> {
        if !remote {
            if let Some(config) = &self.config {
                info!("Local backups:");
                let backup_dir = Path::new(&config.backup.local_backup_dir);
                if backup_dir.exists() {
                    let mut backups: Vec<PathBuf> = fs::read_dir(backup_dir)
                        .context("Failed to read backup directory")?
                        .filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .and_then(|s| s.to_str())
                                .map(|s| s == "zst")
                                .unwrap_or(false)
                        })
                        .collect();
                    backups.sort();
                    for backup in backups.iter().rev() {
                        if let Ok(metadata) = fs::metadata(backup) {
                            let size_mb = metadata.len() as f64 / 1_048_576.0;
                            println!(
                                "  {} ({:.2} MB)",
                                backup.file_name().unwrap().to_string_lossy(),
                                size_mb
                            );
                        }
                    }
                }
            } else {
                info!("Local backups: (not available in client mode)");
            }
        }

        if remote {
            info!("Remote backups:");
            let provider = self.get_provider()?;

            let items = provider.list("backups/").await?;
            for item in items {
                let size_mb = item.size as f64 / 1_048_576.0;
                if let Some(name) = item.key.strip_prefix("backups/") {
                    if let Some(last_modified) = item.last_modified {
                        println!("  {} ({:.2} MB) - {}", name, size_mb, last_modified);
                    } else {
                        println!("  {} ({:.2} MB)", name, size_mb);
                    }
                }
            }
        }

        Ok(())
    }

    async fn download_backup(&self, key: &str, output_dir: &str) -> Result<()> {
        let provider = self.get_provider()?;

        let storage_key = if key.starts_with("backups/") {
            key.to_string()
        } else {
            format!("backups/{}", key)
        };

        fs::create_dir_all(output_dir).context("Failed to create output directory")?;

        let output_path = Path::new(output_dir).join(key.strip_prefix("backups/").unwrap_or(key));

        provider.download(&storage_key, &output_path).await?;
        Ok(())
    }

    async fn clean_backups(&self, dry_run: bool) -> Result<()> {
        let config = self
            .config
            .as_ref()
            .context("Clean requires server configuration")?;
        let provider = self.get_provider()?;

        // Clean local backups
        info!("Cleaning local backups...");
        let backup_dir = Path::new(&config.backup.local_backup_dir);
        let retention_days = config.backup.retention_days.unwrap_or(7);

        if backup_dir.exists() {
            let cutoff = Local::now() - chrono::Duration::days(retention_days as i64);
            let mut backups: Vec<(PathBuf, DateTime<Local>)> = fs::read_dir(backup_dir)
                .context("Failed to read backup directory")?
                .filter_map(|e| {
                    let e = e.ok()?;
                    let path = e.path();
                    let metadata = fs::metadata(&path).ok()?;
                    let modified = metadata.modified().ok()?;
                    let datetime: DateTime<Local> = modified.into();
                    Some((path, datetime))
                })
                .collect();

            backups.sort_by_key(|(_, dt)| *dt);

            for (path, dt) in backups {
                if dt < cutoff {
                    if dry_run {
                        info!("Would delete: {}", path.display());
                    } else {
                        fs::remove_file(&path)
                            .with_context(|| format!("Failed to delete: {}", path.display()))?;
                        info!("Deleted: {}", path.display());
                    }
                }
            }
        }

        // Clean remote backups
        if !dry_run {
            info!("Cleaning remote backups...");
            let cutoff_utc = Utc::now() - chrono::Duration::days(retention_days as i64);

            let items = provider.list("backups/").await?;
            for item in items {
                if let Some(last_modified) = item.last_modified {
                    if last_modified < cutoff_utc {
                        provider.delete(&item.key).await?;
                    }
                }
            }
        }

        Ok(())
    }
}

async fn restore_backup(backup_file: &str, target_dir: Option<String>) -> Result<()> {
    let target = target_dir.unwrap_or_else(|| "./restored".to_string());
    info!("Restoring backup from {} to {}", backup_file, target);

    fs::create_dir_all(&target).context("Failed to create target directory")?;

    let output = Command::new("tar")
        .arg("-I")
        .arg("zstd -d")
        .arg("-xf")
        .arg(backup_file)
        .arg("-C")
        .arg(&target)
        .output()
        .context("Failed to execute tar command")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow::anyhow!("Restore failed: {}", stderr));
    }

    info!("Restore completed successfully");
    Ok(())
}

async fn run_daemon(
    backup_interval: u64,
    upload_interval: u64,
    pid_file: String,
    config_path: Option<String>,
) -> Result<()> {
    use std::fs::File;
    use std::io::Write;
    use std::time::Duration;

    // Write PID file
    let pid = std::process::id();
    let mut file = File::create(&pid_file)
        .with_context(|| format!("Failed to create PID file: {}", pid_file))?;
    writeln!(file, "{}", pid)
        .with_context(|| format!("Failed to write PID to file: {}", pid_file))?;

    info!("Daemon started with PID: {}", pid);
    info!("Backup interval: {} hours", backup_interval);
    info!("Upload interval: {} hours", upload_interval);

    let default_config = "config.toml";
    let config_path = config_path.as_deref().unwrap_or(default_config);
    let manager = BackupManager::new(Some(config_path)).await?;

    let backup_interval_duration = Duration::from_secs(backup_interval * 3600);
    let upload_interval_duration = Duration::from_secs(upload_interval * 3600);

    let mut backup_interval_timer = tokio::time::interval(backup_interval_duration);
    let mut upload_interval_timer = tokio::time::interval(upload_interval_duration);

    // Initial immediate backup
    backup_interval_timer.reset();

    loop {
        tokio::select! {
            _ = backup_interval_timer.tick() => {
                info!("Scheduled backup triggered");
                if let Err(e) = manager.create_backup(false).await {
                    warn!("Backup failed: {}", e);
                }
            }
            _ = upload_interval_timer.tick() => {
                info!("Scheduled upload triggered");
                if let Err(e) = manager.upload_backup(None).await {
                    warn!("Upload failed: {}", e);
                }
            }
        }
    }
}

async fn show_status(config_path: Option<String>) -> Result<()> {
    let default_config = "config.toml";
    let config_path = config_path.as_deref().unwrap_or(default_config);

    if let Ok(manager) = BackupManager::new(Some(config_path)).await {
        if let Some(config) = &manager.config {
            println!("📊 Backup System Status");
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
            println!("Provider: {}", config.storage.provider);
            println!("Bucket: {}", config.storage.bucket);
            if let Some(ref endpoint) = config.storage.endpoint {
                println!("Endpoint: {}", endpoint);
            }
            println!("Backup Directory: {}", config.backup.local_backup_dir);
            println!("Project Path: {}", config.backup.project_path);
            println!(
                "Retention: {} days",
                config.backup.retention_days.unwrap_or(7)
            );
            println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

            // Count local backups
            let backup_dir = Path::new(&config.backup.local_backup_dir);
            if backup_dir.exists() {
                let count = fs::read_dir(backup_dir)
                    .ok()
                    .map(|dir| {
                        dir.filter_map(|e| e.ok())
                            .filter(|e| {
                                e.path()
                                    .extension()
                                    .and_then(|s| s.to_str())
                                    .map(|s| s == "zst")
                                    .unwrap_or(false)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                println!("Local Backups: {}", count);
            }
        }

        // List remote backups count
        if manager.list_backups(true).await.is_ok() {
            // Already printed by list_backups
        }
    } else {
        println!("⚠️  Could not load configuration");
    }

    Ok(())
}

async fn generate_example_config(output_path: &str) -> Result<()> {
    use std::io::Write;

    let example_config = r#"# Zesty Backup System Configuration
# Multi-provider cloud backup utility

[storage]
# Provider: s3, aws, contabo, digitalocean, wasabi, minio, r2, gcs, google, azure, b2, backblaze,
#          googledrive, gdrive, onedrive, dropbox, box, pcloud, mega
provider = "s3"

# For S3-compatible providers (AWS, Contabo, DigitalOcean Spaces, Wasabi, MinIO, Cloudflare R2)
endpoint = "https://s3.amazonaws.com"  # Leave empty for AWS, set for S3-compatible
region = "us-east-1"
bucket = "your-bucket-name"
access_key = "your-access-key"
secret_key = "your-secret-key"

# For Google Cloud Storage (enterprise)
# provider = "gcs"  # or "google"
# bucket = "my-backups"
# credentials_path = "/path/to/service-account-key.json"  # Optional: uses GOOGLE_APPLICATION_CREDENTIALS env var if not set

# For Azure Blob Storage (enterprise)
# provider = "azure"
# account_name = "your-account-name"
# account_key = "your-account-key"  # Optional: can also use AZURE_STORAGE_ACCOUNT_KEY env var
# bucket = "my-container"  # Azure uses "container" instead of "bucket"

# For Backblaze B2 (enterprise)
# account_id = "your-account-id"
# application_key = "your-application-key"
# bucket_id = "your-bucket-id"

# For Google Drive (consumer-grade)
# provider = "googledrive"  # or "gdrive"
# access_key = "ya29.a0AfH6SMC..."  # OAuth2 access token
# bucket_id = "folder-id-here"  # Optional: Google Drive folder ID

# For OneDrive (consumer-grade)
# provider = "onedrive"
# access_key = "eyJ0eXAiOiJKV1QiLCJub..."  # OAuth2 access token
# bucket_id = "/drive/root:/Backups"  # Optional: folder path

# For Dropbox (consumer-grade)
# provider = "dropbox"
# access_key = "sl.Bk..."  # Dropbox access token
# bucket_id = "/Backups"  # Optional: folder path

# For Box (consumer-grade)
# provider = "box"
# access_key = "T9cE5asOhuy8CC6..."  # OAuth2 access token
# bucket_id = "123456789"  # Optional: folder ID

# For pCloud (consumer-grade)
# provider = "pcloud"
# access_key = "your-api-access-token"  # Get from https://my.pcloud.com/#page=apikeys
# region = "us"  # "us" (default) or "eu" for European data center
# bucket_id = "/Backups"  # Optional: folder path

# For MEGA (requires MEGAcmd - install from https://mega.nz/cmd)
# provider = "mega"
# account_name = "your-email@example.com"  # MEGA email
# account_key = "your-password"  # MEGA password
# bucket_id = "/Backups"  # Optional: folder path

[backup]
# Local backup directory
local_backup_dir = "./backups"

# Main project path to backup
project_path = "/path/to/your/project"

# Additional paths to include (files or directories)
additional_paths = [
    # "/etc/nginx/nginx.conf",
    # "/etc/nginx/sites-available/your-site",
]

# Incremental backups per day (local)
incremental_per_day = 4

# Upload to cloud storage interval in hours
upload_interval_hours = 24

# Retention: keep backups for N days
retention_days = 7

# Compression level (0-22, higher = better compression but slower)
compression_level = 3
compression_format = "zst"

# Paths to exclude from backup (patterns)
exclude = [
    # "node_modules",
    # ".git",
    # "*.log",
]

[database]
# Database backup (optional)
# Supported types: postgres, mariadb, mysql, mongodb, cassandra, scylla, redis, sqlite
enabled = false
# type = "postgres"  # Default: postgres
# host = "localhost"
# port = 5432
# database = "your_database"
# username = "your_user"
# password = "your_password"  # Optional: can also use DB_PASSWORD env var or .env file

[system]
# Systemd services to backup (optional)
systemd_services = [
    # "your-service.service",
]

# Systemd timers to backup (optional)
systemd_timers = [
    # "your-timer.timer",
]

# Command outputs to capture (general pattern for any command)
command_outputs = [
    # { command = "ollama", args = ["list"], output_file = "ollama_models.txt", enabled = true },
    # { command = "docker", args = ["ps", "-a"], output_file = "docker_containers.txt", enabled = true },
    # { command = "systemctl", args = ["list-units", "--type=service"], output_file = "systemd_services.txt", enabled = false },
]

# Presets for common backup scenarios
[system.presets]
# Nginx configuration presets
nginx_enabled = false  # Backup /etc/nginx/nginx.conf and sites-available/enabled
nginx_sites = [
    # "example.com",
    # "another-site.com",
]

# Crontab backup
crontab_enabled = false
crontab_user = null  # null = current user, or specify username

# User config files (from home directory)
user_configs = [
    # ".zshrc",
    # ".bashrc",
    # ".vimrc",
    # ".gitconfig",
]
user_configs_home = null  # null = $HOME, or specify path

# Common /etc files and directories
etc_files = [
    # "hosts",
    # "fstab",
]
etc_dirs = [
    # "ssl",
    # "letsencrypt",
]

[logging]
level = "info"
log_dir = "./logs"
"#;

    let mut file = fs::File::create(output_path)
        .with_context(|| format!("Failed to create config file: {}", output_path))?;
    file.write_all(example_config.as_bytes())
        .with_context(|| format!("Failed to write config file: {}", output_path))?;

    println!("✅ Example configuration file generated: {}", output_path);
    println!("📝 Please edit it with your actual credentials and paths");

    Ok(())
}

async fn show_logs(lines: usize, config_path: Option<String>) -> Result<()> {
    let default_config = "config.toml";
    let config_path = config_path.as_deref().unwrap_or(default_config);

    if let Ok(config_content) = fs::read_to_string(config_path) {
        if let Ok(config) = toml::from_str::<AppConfig>(&config_content) {
            let log_dir = config
                .logging
                .as_ref()
                .and_then(|l| l.log_dir.as_ref())
                .map(|s| s.as_str())
                .unwrap_or("./logs");
            let log_file = format!("{}/zesty-backup.log", log_dir);
            if Path::new(&log_file).exists() {
                let content = fs::read_to_string(&log_file)?;
                let log_lines: Vec<&str> = content.lines().collect();
                let start = log_lines.len().saturating_sub(lines);
                for line in log_lines.iter().skip(start) {
                    println!("{}", line);
                }
            } else {
                println!("No log file found at: {}", log_file);
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt()
        .with_env_filter("zesty_backup=info")
        .init();

    let cli = Cli::parse();
    let default_config = "config.toml";
    let config_path = cli.config.as_deref().unwrap_or(default_config);

    match cli.command {
        Commands::Backup { full } => {
            let manager = BackupManager::new(Some(config_path)).await?;
            manager.create_backup(full).await?;
        }
        Commands::Upload { file } => {
            let manager = BackupManager::new(Some(config_path)).await?;
            manager.upload_backup(file.as_deref()).await?;
        }
        Commands::List { remote } => {
            let manager = BackupManager::new(Some(config_path)).await?;
            manager.list_backups(remote).await?;
        }
        Commands::Download { key, output } => {
            let manager = BackupManager::new(Some(config_path)).await?;
            manager.download_backup(&key, &output).await?;
        }
        Commands::Clean { dry_run } => {
            let manager = BackupManager::new(Some(config_path)).await?;
            manager.clean_backups(dry_run).await?;
        }
        Commands::Restore { file, target } => {
            restore_backup(&file, target).await?;
        }
        Commands::Daemon {
            backup_interval,
            upload_interval,
            pid_file,
        } => {
            run_daemon(backup_interval, upload_interval, pid_file, cli.config).await?;
        }
        Commands::Client {
            config,
            provider,
            endpoint,
            region,
            bucket,
            access_key,
            secret_key,
            operation,
        } => {
            let provider_config = if let Some(config_path) = config {
                // Load from config file
                let config_content = fs::read_to_string(&config_path)
                    .with_context(|| format!("Failed to read config file: {}", config_path))?;
                let app_config: AppConfig =
                    toml::from_str(&config_content).context("Failed to parse config file")?;
                ProviderStorageConfig {
                    provider: app_config.storage.provider,
                    endpoint: app_config.storage.endpoint.unwrap_or_default(),
                    region: app_config
                        .storage
                        .region
                        .unwrap_or_else(|| "us-east-1".to_string()),
                    bucket: app_config.storage.bucket,
                    access_key: app_config.storage.access_key.unwrap_or_default(),
                    secret_key: app_config.storage.secret_key.unwrap_or_default(),
                    account_id: app_config.storage.account_id,
                    account_name: app_config.storage.account_name,
                    account_key: app_config.storage.account_key,
                    application_key: app_config.storage.application_key,
                    bucket_id: app_config.storage.bucket_id,
                    credentials_path: app_config.storage.credentials_path,
                    tenant_id: app_config.storage.tenant_id,
                }
            } else {
                // Use command-line arguments
                let provider_name =
                    provider.context("--provider is required when not using --config")?;
                ProviderStorageConfig {
                    provider: provider_name,
                    endpoint: endpoint.unwrap_or_default(),
                    region: region.unwrap_or_else(|| "us-east-1".to_string()),
                    bucket: bucket.context("--bucket is required when not using --config")?,
                    access_key: access_key.unwrap_or_default(),
                    secret_key: secret_key.unwrap_or_default(),
                    account_id: None,
                    account_name: None,
                    account_key: None,
                    application_key: None,
                    bucket_id: None,
                    credentials_path: None,
                    tenant_id: None,
                }
            };
            let manager = BackupManager::new_client(provider_config).await?;
            match operation {
                ClientOperation::List => {
                    manager.list_backups(true).await?;
                }
                ClientOperation::Download { key, output } => {
                    manager.download_backup(&key, &output).await?;
                }
            }
        }
        Commands::GenerateConfig { output } => {
            generate_example_config(&output).await?;
        }
        Commands::Status => {
            show_status(cli.config).await?;
        }
        Commands::Logs { lines } => {
            show_logs(lines, cli.config).await?;
        }
    }

    Ok(())
}

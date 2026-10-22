use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tracing::{error, info, warn};
use zesty_backup::alerts::{Alerter, Event, EventKind};
use zesty_backup::archive;
use zesty_backup::backup::{self, human_size, REMOTE_PREFIX};
use zesty_backup::config::{AppConfig, StorageConfig, EXAMPLE_CONFIG};
use zesty_backup::encryption::DecryptionKeys;
use zesty_backup::providers::{self, StorageProvider};
use zesty_backup::{anomaly, doctor, drill};

#[derive(Parser)]
#[command(name = "zesty-backup")]
#[command(about = "A flexible, multi-provider backup utility for cloud storage")]
#[command(version)]
struct Cli {
    /// Path to the configuration file
    #[arg(
        short,
        long,
        global = true,
        env = "ZESTY_BACKUP_CONFIG",
        default_value = "config.toml"
    )]
    config: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Create a new backup archive in local_backup_dir
    Backup {
        /// Name the archive backup-full-… instead of backup-incr-… (the content is the same)
        #[arg(long)]
        full: bool,
    },
    /// Upload local backups that are not in cloud storage yet
    Upload {
        /// Upload only this file (re-uploads it even if it already exists remotely)
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// List backups (local by default)
    List {
        /// List backups in cloud storage instead
        #[arg(long)]
        remote: bool,
    },
    /// Download a backup from cloud storage
    Download {
        /// Backup name, e.g. backup-full-20240101-120000.tar.zst
        key: String,
        /// Output directory
        #[arg(short, long, default_value = "./restored")]
        output: PathBuf,
    },
    /// Delete backups older than retention_days (local and remote)
    Clean {
        /// Only show what would be deleted
        #[arg(long)]
        dry_run: bool,
        /// Only clean local backups
        #[arg(long)]
        local_only: bool,
        /// Delete even while retention is paused because the newest backup looked wrong
        #[arg(long)]
        force: bool,
    },
    /// Accept the newest backup as normal after an anomaly warning (e.g. you really deleted files)
    AcceptChanges,
    /// Extract a backup archive
    Restore {
        /// Backup file path
        file: PathBuf,
        /// Target directory
        #[arg(short, long, default_value = "./restored")]
        target: PathBuf,
        /// age private key file, for encrypted backups (passphrase: ZESTY_BACKUP_PASSPHRASE)
        #[arg(short, long)]
        identity: Option<PathBuf>,
    },
    /// Check that a backup archive is complete and not corrupted
    Verify {
        /// Backup file path
        file: PathBuf,
        /// age private key file, for encrypted backups (passphrase: ZESTY_BACKUP_PASSPHRASE)
        #[arg(short, long)]
        identity: Option<PathBuf>,
    },
    /// Run in the foreground, creating, uploading and cleaning backups on a schedule
    Daemon {
        /// Hours between backups [default: 24 / incremental_per_day, or 6]
        #[arg(short, long)]
        backup_interval: Option<u64>,
        /// Hours between uploads (followed by a clean) [default: upload_interval_hours, or 24]
        #[arg(short, long)]
        upload_interval: Option<u64>,
        /// Write the process ID to this file
        #[arg(short, long)]
        pid_file: Option<PathBuf>,
    },
    /// Access remote backups without a full config file (e.g. from your desktop)
    Client {
        #[command(flatten)]
        storage: Box<ClientStorage>,
        #[command(subcommand)]
        operation: ClientOperation,
    },
    /// Write the example configuration file
    GenerateConfig {
        /// Output path
        #[arg(short, long, default_value = "config.toml.example")]
        output: PathBuf,
        /// Overwrite the file if it exists
        #[arg(long)]
        force: bool,
    },
    /// Check the whole setup: paths, encryption keys, database access, storage round trip
    Doctor {
        /// age private key, to prove it matches the configured public key
        #[arg(short, long)]
        identity: Option<PathBuf>,
        /// Don't touch remote storage
        #[arg(long)]
        skip_storage: bool,
    },
    /// Restore the latest backup into a temporary place and check that it is usable
    Drill {
        /// Backup to test (default: the newest)
        backup: Option<String>,
        /// Test a local backup instead of downloading the remote one
        #[arg(long)]
        local: bool,
        /// age private key, for encrypted backups (passphrase: ZESTY_BACKUP_PASSPHRASE)
        #[arg(short, long)]
        identity: Option<PathBuf>,
        /// Keep the restored files and print where they are
        #[arg(long)]
        keep: bool,
    },
    /// Send a test message to every channel in [alerts]
    TestAlerts,
    /// Show configuration and backup counts
    Status,
    /// Show the last lines of the log file (needs [logging] log_dir)
    Logs {
        /// Number of lines
        #[arg(short, long, default_value = "50")]
        lines: usize,
    },
}

/// Storage settings for `client`. When `--provider` is given these are used instead of the
/// config file's [storage] section; otherwise the config file (`--config`) is used.
#[derive(Args)]
struct ClientStorage {
    /// Storage provider (s3, aws, gcs, azure, b2, dropbox, ...)
    #[arg(short = 'p', long)]
    provider: Option<String>,
    /// Endpoint URL (S3-compatible providers)
    #[arg(short = 'e', long)]
    endpoint: Option<String>,
    #[arg(short = 'r', long)]
    region: Option<String>,
    /// Bucket / container name
    #[arg(short = 'b', long)]
    bucket: Option<String>,
    /// S3 access key, or OAuth access token for consumer providers
    #[arg(short = 'a', long, env = "ZESTY_ACCESS_KEY", hide_env_values = true)]
    access_key: Option<String>,
    #[arg(short = 's', long, env = "ZESTY_SECRET_KEY", hide_env_values = true)]
    secret_key: Option<String>,
    #[arg(long)]
    account_id: Option<String>,
    #[arg(long)]
    account_name: Option<String>,
    #[arg(long, env = "ZESTY_ACCOUNT_KEY", hide_env_values = true)]
    account_key: Option<String>,
    #[arg(long, env = "ZESTY_APPLICATION_KEY", hide_env_values = true)]
    application_key: Option<String>,
    /// B2 bucket ID, or the folder for consumer providers
    #[arg(long)]
    bucket_id: Option<String>,
    #[arg(long)]
    credentials_path: Option<String>,
}

#[derive(Subcommand)]
enum ClientOperation {
    /// List remote backups
    List,
    /// Download a backup
    Download {
        /// Backup name
        key: String,
        /// Output directory
        #[arg(short, long, default_value = "./restored")]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    // Logging needs the config (log level and file), but a broken config must still be reported.
    let config = AppConfig::load(&cli.config);
    init_logging(config.as_ref().ok());

    if let Err(e) = run(cli, config).await {
        error!("{:#}", e);
        std::process::exit(1);
    }
}

/// Keys for reading encrypted archives: `--identity`, then the config's [encryption] section.
fn decryption_keys(identity: Option<&Path>, config: Option<&AppConfig>) -> DecryptionKeys {
    DecryptionKeys::new(identity, config.and_then(|c| c.encryption.as_ref()))
}

async fn run(cli: Cli, config: Result<AppConfig>) -> Result<()> {
    match cli.command {
        Commands::GenerateConfig { output, force } => generate_config(&output, force),
        Commands::Restore {
            file,
            target,
            identity,
        } => {
            info!("Restoring {} into {}", file.display(), target.display());
            let keys = decryption_keys(identity.as_deref(), config.as_ref().ok());
            tokio::task::spawn_blocking(move || archive::extract(&file, &target, &keys)).await??;
            info!("Restore completed");
            Ok(())
        }
        Commands::Verify { file, identity } => {
            let path = file.clone();
            let keys = decryption_keys(identity.as_deref(), config.as_ref().ok());
            let entries =
                tokio::task::spawn_blocking(move || archive::verify(&path, &keys)).await??;
            println!("OK: {} ({} entries)", file.display(), entries);
            Ok(())
        }
        Commands::Client { storage, operation } => {
            let storage_config =
                match storage.provider.clone() {
                    Some(provider) => storage.into_config(provider),
                    None => config
                        .context(
                            "Pass --provider (and its credentials), or a config file with --config",
                        )?
                        .storage,
                };
            let provider = providers::from_config(&storage_config).await?;
            match operation {
                ClientOperation::List => print_remote(provider.as_ref()).await,
                ClientOperation::Download { key, output } => {
                    backup::download(provider.as_ref(), &key, &output)
                        .await
                        .map(|_| ())
                }
            }
        }
        command => {
            let config = config?;
            run_with_config(command, config, &cli.config).await
        }
    }
}

async fn run_with_config(command: Commands, config: AppConfig, config_path: &str) -> Result<()> {
    let alerter = Alerter::new(config.alerts.as_ref());
    match command {
        Commands::Backup { full } => {
            backup_and_alert(Arc::new(config), full, &alerter).await?;
        }
        Commands::Upload { file } => {
            let result = async {
                let provider = providers::from_config(&config.storage).await?;
                backup::upload(provider.as_ref(), &config, file.as_deref()).await
            }
            .await;
            alerter
                .notify(&Event::from_result(EventKind::Upload, &result, |n| {
                    format!("{} backup(s) uploaded", n)
                }))
                .await;
            info!("{} backup(s) uploaded", result?);
        }
        Commands::List { remote } => {
            if remote {
                let provider = providers::from_config(&config.storage).await?;
                print_remote(provider.as_ref()).await?;
            } else {
                let backups = backup::local_backups(Path::new(&config.backup.local_backup_dir))?;
                if backups.is_empty() {
                    println!("No local backups in {}", config.backup.local_backup_dir);
                }
                for path in backups.iter().rev() {
                    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                    println!(
                        "  {}  ({})",
                        path.file_name().unwrap().to_string_lossy(),
                        human_size(size)
                    );
                }
            }
        }
        Commands::Download { key, output } => {
            let provider = providers::from_config(&config.storage).await?;
            backup::download(provider.as_ref(), &key, &output).await?;
        }
        Commands::Clean {
            dry_run,
            local_only,
            force,
        } => {
            let result = async {
                let provider = if local_only {
                    None
                } else {
                    Some(providers::from_config(&config.storage).await?)
                };
                backup::clean_with(provider.as_deref(), &config, dry_run, force).await
            }
            .await;
            if !dry_run {
                alerter
                    .notify(&Event::from_result(EventKind::Clean, &result, |summary| {
                        clean_summary_text(*summary)
                    }))
                    .await;
            }
            result?;
        }
        Commands::Daemon {
            backup_interval,
            upload_interval,
            pid_file,
        } => run_daemon(config, backup_interval, upload_interval, pid_file).await?,
        Commands::Doctor {
            identity,
            skip_storage,
        } => {
            let options = doctor::DoctorOptions {
                identity: identity.as_deref(),
                skip_storage,
            };
            println!("Zesty Backup doctor ({})", config_path);
            let report = doctor::run(&config, &options).await;
            println!("{}", report);
            if !report.passed() {
                std::process::exit(1);
            }
        }
        Commands::Drill {
            backup,
            local,
            identity,
            keep,
        } => {
            let provider: Option<Arc<dyn StorageProvider>> = if local {
                None
            } else {
                Some(Arc::from(providers::from_config(&config.storage).await?))
            };
            let job = DrillJob {
                config: Arc::new(config),
                provider,
                backup,
                local,
                identity,
                keep,
            };
            let outcome = tokio::select! {
                outcome = job.spawn() => outcome?,
                _ = shutdown_signal() => {
                    drill::cleanup_interrupted();
                    bail!("Interrupted: the drill's containers and restored files were removed");
                }
            };
            alerter
                .notify(&Event::from_drill(
                    outcome.backup.as_deref(),
                    &outcome.report,
                ))
                .await;
            println!(
                "Restore drill: {}",
                outcome.backup.as_deref().unwrap_or("(no backup)")
            );
            println!("{}", outcome.report);
            if let Some(dir) = &outcome.kept_dir {
                println!("Restored files kept in {}", dir.display());
            }
            if !outcome.report.passed() {
                std::process::exit(1);
            }
        }
        Commands::AcceptChanges => match anomaly::accept(&config)? {
            Some(name) => println!(
                "{} is now the baseline for anomaly checks; retention resumes.",
                name
            ),
            None => println!("No backup history yet: nothing to accept."),
        },
        Commands::TestAlerts => {
            if !alerter.is_configured() {
                bail!("No alert channels configured: add an [alerts] section (see README)");
            }
            let event = Event {
                kind: EventKind::Test,
                success: true,
                summary: "Test message from `zesty-backup test-alerts`. If you can read this, alerts work.".into(),
                details: None,
            };
            let mut failed = false;
            for delivery in alerter.send(&event).await {
                match delivery.result {
                    Ok(()) => println!("  ✔ {}", delivery.channel),
                    Err(e) => {
                        failed = true;
                        println!("  ✖ {}: {:#}", delivery.channel, e);
                    }
                }
            }
            if failed {
                std::process::exit(1);
            }
        }
        Commands::Status => show_status(&config, config_path).await,
        Commands::Logs { lines } => show_logs(&config, lines)?,
        Commands::GenerateConfig { .. }
        | Commands::Restore { .. }
        | Commands::Verify { .. }
        | Commands::Client { .. } => unreachable!("handled without a config"),
    }
    Ok(())
}

impl ClientStorage {
    fn into_config(self, provider: String) -> StorageConfig {
        StorageConfig {
            provider,
            endpoint: self.endpoint,
            region: self.region,
            bucket: self.bucket.unwrap_or_default(),
            access_key: self.access_key,
            secret_key: self.secret_key,
            account_id: self.account_id,
            account_name: self.account_name,
            account_key: self.account_key,
            application_key: self.application_key,
            bucket_id: self.bucket_id,
            credentials_path: self.credentials_path,
            ..Default::default()
        }
    }
}

async fn print_remote(provider: &dyn StorageProvider) -> Result<()> {
    let items = backup::remote_backups(provider).await?;
    if items.is_empty() {
        println!("No remote backups");
    }
    for item in items.iter().rev() {
        let name = &item.key[REMOTE_PREFIX.len()..];
        match item.last_modified {
            Some(t) => println!(
                "  {}  ({}, uploaded {})",
                name,
                human_size(item.size),
                t.format("%Y-%m-%d %H:%M UTC")
            ),
            None => println!("  {}  ({})", name, human_size(item.size)),
        }
    }
    Ok(())
}

fn init_logging(config: Option<&AppConfig>) {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{fmt, EnvFilter};

    let logging = config.and_then(|c| c.logging.as_ref());
    let level = logging
        .and_then(|l| l.level.clone())
        .unwrap_or_else(|| "info".to_string());
    // RUST_LOG wins over the config file.
    let filter = || {
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(format!("zesty_backup={}", level)))
    };

    let mut file_error = None;
    let file_layer = logging.and_then(|l| l.log_file()).and_then(|path| {
        let open = || -> std::io::Result<std::fs::File> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
        };
        match open() {
            Ok(file) => Some(
                fmt::layer()
                    .with_ansi(false)
                    .with_writer(Mutex::new(file))
                    .with_filter(filter()),
            ),
            Err(e) => {
                file_error = Some(format!("Cannot open log file {}: {}", path.display(), e));
                None
            }
        }
    });

    tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(filter()),
        )
        .with(file_layer)
        .init();
    if let Some(e) = file_error {
        warn!("{}", e);
    }
}

fn generate_config(output: &Path, force: bool) -> Result<()> {
    if output.exists() && !force {
        bail!(
            "{} already exists (use --force to overwrite)",
            output.display()
        );
    }
    std::fs::write(output, EXAMPLE_CONFIG)
        .with_context(|| format!("Failed to write {}", output.display()))?;
    println!("Example configuration written to {}", output.display());
    println!("Edit it, then save it as config.toml (and chmod 600 it: it contains credentials).");
    Ok(())
}

async fn run_daemon(
    config: AppConfig,
    backup_interval: Option<u64>,
    upload_interval: Option<u64>,
    pid_file: Option<PathBuf>,
) -> Result<()> {
    let backup_hours = backup_interval
        .or_else(|| {
            config
                .backup
                .incremental_per_day
                .map(|n| (24 / n.max(1) as u64).max(1))
        })
        .unwrap_or(6);
    let upload_hours = upload_interval
        .or(config.backup.upload_interval_hours.map(u64::from))
        .unwrap_or(24);
    anyhow::ensure!(
        backup_hours > 0 && upload_hours > 0,
        "Intervals must be at least 1 hour"
    );

    if let Some(pid_file) = &pid_file {
        std::fs::write(pid_file, format!("{}\n", std::process::id()))
            .with_context(|| format!("Failed to write PID file {}", pid_file.display()))?;
    }

    let alerter = Alerter::new(config.alerts.as_ref());
    let provider: Arc<dyn StorageProvider> = match providers::from_config(&config.storage).await {
        Ok(p) => Arc::from(p),
        Err(e) => {
            alerter.notify(&Event::failure(EventKind::Upload, &e)).await;
            return Err(e);
        }
    };
    let config = Arc::new(config);
    info!(
        "Daemon started (PID {}): backup every {}h, upload + clean every {}h",
        std::process::id(),
        backup_hours,
        upload_hours
    );

    let mut backup_timer = tokio::time::interval(Duration::from_secs(backup_hours * 3600));
    let mut upload_timer = tokio::time::interval(Duration::from_secs(upload_hours * 3600));
    for timer in [&mut backup_timer, &mut upload_timer] {
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    }

    // Optional scheduled restore drills; the first one runs after one interval.
    let drill_hours = config.drill.as_ref().and_then(|d| d.interval_hours);
    let mut drill_timer =
        tokio::time::interval(Duration::from_secs(drill_hours.unwrap_or(24 * 365) * 3600));
    drill_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    drill_timer.tick().await;
    if let Some(h) = drill_hours {
        info!("Restore drills every {}h", h);
    }

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    // Both timers fire immediately: start with a backup, then upload it.
    backup_timer.tick().await;
    run_backup(&config, &alerter).await;
    upload_timer.tick().await;
    run_upload(provider.as_ref(), &config, &alerter).await;

    // Drills run on their own thread so a slow one can't hold up backups.
    let mut drill_running: Option<std::thread::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            _ = &mut shutdown => break,
            _ = backup_timer.tick() => run_backup(&config, &alerter).await,
            _ = upload_timer.tick() => run_upload(provider.as_ref(), &config, &alerter).await,
            _ = drill_timer.tick(), if drill_hours.is_some() => {
                if drill_running.as_ref().is_some_and(|h| !h.is_finished()) {
                    warn!("Previous restore drill still running, skipping this one");
                } else {
                    drill_running = Some(start_scheduled_drill(provider.clone(), config.clone()));
                }
            }
        }
    }

    info!("Shutting down");
    drill::cleanup_interrupted();
    if let Some(pid_file) = &pid_file {
        let _ = std::fs::remove_file(pid_file);
    }
    Ok(())
}

async fn run_backup(config: &Arc<AppConfig>, alerter: &Alerter) {
    if let Err(e) = backup_and_alert(config.clone(), false, alerter).await {
        error!("Scheduled backup failed: {:#}", e);
    }
}

/// Create a backup, run the anomaly checks, and report the result to the alert channels.
async fn backup_and_alert(
    config: Arc<AppConfig>,
    full: bool,
    alerter: &Alerter,
) -> Result<PathBuf> {
    let result = tokio::task::spawn_blocking(move || backup::create_backup_checked(&config, full))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
    let event = match &result {
        Ok(outcome) if !outcome.anomalies.is_empty() => {
            Event::anomaly(&outcome.stats.name, &outcome.anomalies)
        }
        _ => Event::from_result(EventKind::Backup, &result, |o| describe_archive(&o.path)),
    };
    alerter.notify(&event).await;
    result.map(|o| o.path)
}

fn describe_archive(path: &Path) -> String {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    format!(
        "{} created ({})",
        path.file_name().unwrap_or_default().to_string_lossy(),
        human_size(size)
    )
}

/// A drill to run on its own thread, with its own runtime: it does blocking work
/// (extraction, containers, user commands) that must not stall the async scheduler.
struct DrillJob {
    config: Arc<AppConfig>,
    provider: Option<Arc<dyn StorageProvider>>,
    backup: Option<String>,
    local: bool,
    identity: Option<PathBuf>,
    keep: bool,
}

impl DrillJob {
    fn run_blocking(self) -> drill::DrillOutcome {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("failed to start the drill runtime");
        let options = drill::DrillOptions {
            backup: self.backup.as_deref(),
            local: self.local,
            identity: self.identity.as_deref(),
            keep: self.keep,
        };
        runtime.block_on(drill::run(&self.config, self.provider.as_deref(), &options))
    }

    async fn spawn(self) -> Result<drill::DrillOutcome> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(self.run_blocking());
        });
        rx.await.context("the drill crashed")
    }
}

fn start_scheduled_drill(
    provider: Arc<dyn StorageProvider>,
    config: Arc<AppConfig>,
) -> std::thread::JoinHandle<()> {
    info!("Scheduled restore drill starting");
    std::thread::spawn(move || {
        let job = DrillJob {
            config: config.clone(),
            provider: Some(provider),
            backup: None,
            local: false,
            identity: None,
            keep: false,
        };
        let outcome = job.run_blocking();
        let backup = outcome.backup.as_deref().unwrap_or("(no backup)");
        if outcome.report.passed() {
            info!("Restore drill of {}: {}", backup, outcome.report.summary());
        } else {
            error!(
                "Restore drill of {}: {}\n{}",
                backup,
                outcome.report.summary(),
                outcome.report
            );
        }
        let alerter = Alerter::new(config.alerts.as_ref());
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            rt.block_on(alerter.notify(&Event::from_drill(
                outcome.backup.as_deref(),
                &outcome.report,
            )));
        }
    })
}

fn clean_summary_text(summary: backup::CleanSummary) -> String {
    if summary.paused {
        "retention paused by an anomaly: nothing deleted".to_string()
    } else {
        format!("{} old backup(s) deleted", summary.deleted)
    }
}

async fn run_upload(provider: &dyn StorageProvider, config: &AppConfig, alerter: &Alerter) {
    let result = backup::upload(provider, config, None).await;
    alerter
        .notify(&Event::from_result(EventKind::Upload, &result, |n| {
            format!("{} backup(s) uploaded", n)
        }))
        .await;
    match result {
        Ok(n) => info!("Scheduled upload finished: {} backup(s) uploaded", n),
        Err(e) => {
            error!("Scheduled upload failed: {:#}", e);
            // Don't delete local copies that may not be uploaded yet.
            return;
        }
    }
    let result = backup::clean(Some(provider), config, false).await;
    if let Err(e) = &result {
        error!("Cleaning old backups failed: {:#}", e);
        alerter.notify(&Event::failure(EventKind::Clean, e)).await;
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn show_status(config: &AppConfig, config_path: &str) {
    let storage = &config.storage;
    println!("Zesty Backup status");
    println!("  Config:            {}", config_path);
    println!("  Provider:          {}", storage.provider);
    if !storage.bucket.is_empty() {
        println!("  Bucket:            {}", storage.bucket);
    }
    if let Some(endpoint) = &storage.endpoint {
        println!("  Endpoint:          {}", endpoint);
    }
    println!("  Project path:      {}", config.backup.project_path);
    println!("  Backup directory:  {}", config.backup.local_backup_dir);
    println!(
        "  Retention:         {}",
        zesty_backup::retention::Policy::from_config(config).describe()
    );

    let local =
        backup::local_backups(Path::new(&config.backup.local_backup_dir)).unwrap_or_default();
    let local_size: u64 = local
        .iter()
        .filter_map(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .sum();
    println!(
        "  Local backups:     {} ({})",
        local.len(),
        human_size(local_size)
    );
    if let Some(latest) = local.last() {
        println!(
            "  Latest local:      {}",
            latest.file_name().unwrap().to_string_lossy()
        );
    }

    let remote = async {
        let provider = providers::from_config(storage).await?;
        backup::remote_backups(provider.as_ref()).await
    };
    match remote.await {
        Ok(items) => {
            let size: u64 = items.iter().map(|i| i.size).sum();
            println!(
                "  Remote backups:    {} ({})",
                items.len(),
                human_size(size)
            );
            if let Some(latest) = items.last() {
                println!(
                    "  Latest remote:     {}",
                    &latest.key[REMOTE_PREFIX.len()..]
                );
            }
        }
        Err(e) => println!("  Remote backups:    unavailable ({:#})", e),
    }
    if let Some(reason) = anomaly::retention_pause_reason(config) {
        println!("  Retention:         PAUSED, {}", reason);
        println!("                     (run `zesty-backup accept-changes` if this was expected)");
    }
    match drill::load_last(config) {
        Some(last) => {
            let ago = (chrono::Utc::now() - last.time).num_hours();
            println!(
                "  Last drill:        {} ({}h ago, {})",
                last.summary,
                ago,
                last.backup.as_deref().unwrap_or("no backup")
            );
        }
        None => println!("  Last drill:        never (run `zesty-backup drill`)"),
    }
}

fn show_logs(config: &AppConfig, lines: usize) -> Result<()> {
    let Some(path) = config.logging.as_ref().and_then(|l| l.log_file()) else {
        bail!("No log file configured. Set log_dir in the [logging] section (or use journalctl when running under systemd).");
    };
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let all: Vec<&str> = content.lines().collect();
    for line in &all[all.len().saturating_sub(lines)..] {
        println!("{}", line);
    }
    Ok(())
}

//! The backup operations behind the CLI commands: create, upload, list, download, clean.

use crate::archive::{ArchiveWriter, Excluder};
use crate::config::{AppConfig, CommandOutput, PresetsConfig};
use crate::database;
use crate::encryption::Encryption;
use crate::providers::{BackupItem, StorageProvider};
use anyhow::{bail, Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone, Utc};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tracing::{info, warn};

/// Remote "directory" all backups are uploaded to.
pub const REMOTE_PREFIX: &str = "backups/";
const TIMESTAMP_FORMAT: &str = "%Y%m%d-%H%M%S";

/// `backup-full-YYYYMMDD-HHMMSS.tar.zst` or `backup-incr-…`, optionally ending in `.age`
/// when encrypted.
pub fn is_backup_file_name(name: &str) -> bool {
    name.starts_with("backup-") && (name.ends_with(".tar.zst") || name.ends_with(".tar.zst.age"))
}

/// The creation time encoded in a backup file name (local time).
pub fn timestamp_from_name(name: &str) -> Option<DateTime<Utc>> {
    let name = name
        .strip_suffix(crate::encryption::EXTENSION)
        .unwrap_or(name);
    let stem = name.strip_suffix(".tar.zst")?;
    let ts = stem.get(stem.len().checked_sub(15)?..)?;
    let naive = NaiveDateTime::parse_from_str(ts, TIMESTAMP_FORMAT).ok()?;
    Local
        .from_local_datetime(&naive)
        .earliest()
        .map(|dt| dt.with_timezone(&Utc))
}

fn sort_key(name: &str) -> (Option<DateTime<Utc>>, String) {
    (timestamp_from_name(name), name.to_string())
}

/// Create a backup archive in `local_backup_dir` and return its path.
///
/// Every backup is a complete snapshot; `full` only changes the file name
/// (`backup-full-…` vs `backup-incr-…`) so you can tell manual and scheduled backups apart.
pub fn create_backup(config: &AppConfig, full: bool) -> Result<PathBuf> {
    let backup = &config.backup;
    let project = Path::new(&backup.project_path);
    if !project.exists() {
        bail!("project_path does not exist: {}", backup.project_path);
    }
    fs::create_dir_all(&backup.local_backup_dir).with_context(|| {
        format!(
            "Failed to create backup directory {}",
            backup.local_backup_dir
        )
    })?;

    let mut excluder = Excluder::new(backup.exclude.as_deref().unwrap_or_default())?;
    // A backup dir inside the project would otherwise end up inside every new backup.
    excluder.exclude_dir(Path::new(&backup.local_backup_dir));

    // Checked before doing any work, so a bad key fails fast.
    let encryption = Encryption::from_config(config.encryption.as_ref())?;
    let kind = if full { "full" } else { "incr" };
    let name = format!(
        "backup-{}-{}.tar.zst{}",
        kind,
        Local::now().format(TIMESTAMP_FORMAT),
        if encryption.is_some() {
            crate::encryption::EXTENSION
        } else {
            ""
        }
    );
    let path = Path::new(&backup.local_backup_dir).join(&name);
    info!("Creating backup {}", path.display());

    let mut archive =
        ArchiveWriter::create(&path, backup.compression_level(), encryption.as_ref())?;

    info!("Adding project: {}", backup.project_path);
    let project_name = base_name(project);
    archive.add_path(project, &format!("project/{}", project_name), &excluder)?;

    for extra in backup.additional_paths.iter().flatten() {
        let p = Path::new(extra);
        if p.exists() {
            info!("Adding {}", extra);
            archive.add_path(p, &format!("system/{}", base_name(p)), &excluder)?;
        } else {
            warn!("additional_paths: {} does not exist, skipping", extra);
        }
    }

    if let Some(system) = &config.system {
        for (units, kind) in [
            (&system.systemd_services, "services"),
            (&system.systemd_timers, "timers"),
        ] {
            for unit in units.iter().flatten() {
                let p = Path::new("/etc/systemd/system").join(unit);
                if p.exists() {
                    archive.add_path(&p, &format!("systemd/{}/{}", kind, unit), &excluder)?;
                } else {
                    warn!("systemd unit {} not found in /etc/systemd/system", unit);
                }
            }
        }
        if let Some(presets) = &system.presets {
            apply_presets(&mut archive, presets, &excluder)?;
        }
        for cmd in system.command_outputs.iter().flatten() {
            if cmd.enabled.unwrap_or(true) {
                backup_command_output(&mut archive, cmd)?;
            }
        }
    }

    if let Some(db) = &config.database {
        if db.enabled.unwrap_or(false) {
            info!("Backing up database...");
            database::backup_database(&mut archive, db, &backup.project_path)
                .context("Database backup failed")?;
        }
    }

    let skipped = archive.skipped();
    let path = archive.finish()?;
    let size = fs::metadata(&path)?.len();
    info!("Backup created: {} ({})", path.display(), human_size(size));
    if skipped > 0 {
        warn!(
            "{} file(s) could not be read and were skipped (see warnings above)",
            skipped
        );
    }
    Ok(path)
}

fn base_name(path: &Path) -> String {
    path.canonicalize()
        .ok()
        .as_deref()
        .unwrap_or(path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "root".to_string())
}

fn backup_command_output(archive: &mut ArchiveWriter, cmd: &CommandOutput) -> Result<()> {
    info!(
        "Running command: {} {}",
        cmd.command,
        cmd.args.as_deref().unwrap_or_default().join(" ")
    );
    let output = Command::new(&cmd.command)
        .args(cmd.args.as_deref().unwrap_or_default())
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => {
            archive.add_bytes(&format!("commands/{}", cmd.output_file), &output.stdout)
        }
        Ok(output) => {
            warn!(
                "Command {} failed ({}): {}",
                cmd.command,
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            );
            Ok(())
        }
        Err(e) => {
            warn!("Could not run {}: {}", cmd.command, e);
            Ok(())
        }
    }
}

fn apply_presets(
    archive: &mut ArchiveWriter,
    presets: &PresetsConfig,
    excluder: &Excluder,
) -> Result<()> {
    let add_if_exists = |archive: &mut ArchiveWriter, src: &Path, dest: &str| -> Result<()> {
        if src.exists() {
            archive.add_path(src, dest, excluder)
        } else {
            warn!("{} not found, skipping", src.display());
            Ok(())
        }
    };

    if presets.nginx_enabled.unwrap_or(false) {
        info!("Adding nginx configuration");
        add_if_exists(
            archive,
            Path::new("/etc/nginx/nginx.conf"),
            "system/nginx/nginx.conf",
        )?;
        add_if_exists(
            archive,
            Path::new("/etc/nginx/sites-available"),
            "system/nginx/sites-available",
        )?;
        add_if_exists(
            archive,
            Path::new("/etc/nginx/sites-enabled"),
            "system/nginx/sites-enabled",
        )?;
    } else {
        for site in presets.nginx_sites.iter().flatten() {
            info!("Adding nginx site {}", site);
            for dir in ["sites-available", "sites-enabled"] {
                let p = Path::new("/etc/nginx").join(dir).join(site);
                if p.exists() || p.is_symlink() {
                    archive.add_path(&p, &format!("system/nginx/{}/{}", dir, site), excluder)?;
                }
            }
        }
    }

    if presets.crontab_enabled.unwrap_or(false) {
        let mut cmd = Command::new("crontab");
        if let Some(user) = &presets.crontab_user {
            cmd.arg("-u").arg(user);
        }
        let user = presets
            .crontab_user
            .clone()
            .or_else(|| std::env::var("USER").ok())
            .unwrap_or_else(|| "current".into());
        match cmd.arg("-l").stdin(Stdio::null()).output() {
            Ok(out) if out.status.success() => {
                archive.add_bytes(&format!("system/crontab-{}.txt", user), &out.stdout)?;
            }
            Ok(out) => warn!(
                "crontab -l failed for {}: {}",
                user,
                String::from_utf8_lossy(&out.stderr).trim()
            ),
            Err(e) => warn!("Could not run crontab: {}", e),
        }
    }

    if let Some(configs) = &presets.user_configs {
        let home = presets
            .user_configs_home
            .clone()
            .or_else(|| std::env::var("HOME").ok())
            .unwrap_or_else(|| "/root".into());
        for name in configs {
            add_if_exists(
                archive,
                &Path::new(&home).join(name),
                &format!("user-configs/{}", name),
            )?;
        }
    }

    for name in presets
        .etc_files
        .iter()
        .flatten()
        .chain(presets.etc_dirs.iter().flatten())
    {
        add_if_exists(
            archive,
            &Path::new("/etc").join(name),
            &format!("etc/{}", name),
        )?;
    }
    Ok(())
}

/// Backup archives in `dir`, oldest first.
pub fn local_backups(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut backups: Vec<PathBuf> = fs::read_dir(dir)
        .with_context(|| format!("Failed to read {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(is_backup_file_name)
        })
        .collect();
    backups.sort_by_cached_key(|p| sort_key(&p.file_name().unwrap().to_string_lossy()));
    Ok(backups)
}

/// Remote backups, oldest first.
pub async fn remote_backups(provider: &dyn StorageProvider) -> Result<Vec<BackupItem>> {
    let mut items: Vec<BackupItem> = provider
        .list(REMOTE_PREFIX)
        .await?
        .into_iter()
        .filter(|item| {
            item.key
                .strip_prefix(REMOTE_PREFIX)
                .is_some_and(is_backup_file_name)
        })
        .collect();
    items.sort_by_cached_key(|i| sort_key(&i.key[REMOTE_PREFIX.len()..]));
    Ok(items)
}

/// Upload `file`, or every local backup that is not already in remote storage.
/// Returns the number of files uploaded.
pub async fn upload(
    provider: &dyn StorageProvider,
    config: &AppConfig,
    file: Option<&Path>,
) -> Result<usize> {
    let candidates = match file {
        Some(f) => {
            anyhow::ensure!(f.is_file(), "{} is not a file", f.display());
            vec![f.to_path_buf()]
        }
        None => local_backups(Path::new(&config.backup.local_backup_dir))?,
    };
    if candidates.is_empty() {
        info!("No local backups to upload");
        return Ok(0);
    }

    // Skip what is already uploaded (same name and size), so repeated runs are cheap.
    let remote: HashMap<String, u64> = match remote_backups(provider).await {
        Ok(items) => items.into_iter().map(|i| (i.key, i.size)).collect(),
        Err(e) => {
            warn!(
                "Could not list remote backups ({:#}); uploading everything",
                e
            );
            HashMap::new()
        }
    };

    let mut uploaded = 0;
    for path in candidates {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Invalid backup file name")?;
        let key = format!("{}{}", REMOTE_PREFIX, name);
        let size = fs::metadata(&path)?.len();
        if file.is_none() && remote.get(&key) == Some(&size) {
            info!("{} is already uploaded, skipping", name);
            continue;
        }
        info!("Uploading {} ({})...", name, human_size(size));
        let started = std::time::Instant::now();
        provider
            .upload(&key, &path)
            .await
            .with_context(|| format!("Upload of {} failed", name))?;
        info!("Uploaded {} in {:.0?}", name, started.elapsed());
        uploaded += 1;
    }
    Ok(uploaded)
}

/// Download a remote backup into `output_dir`. `key` may be a file name or `backups/<name>`.
pub async fn download(
    provider: &dyn StorageProvider,
    key: &str,
    output_dir: &Path,
) -> Result<PathBuf> {
    let name = key.strip_prefix(REMOTE_PREFIX).unwrap_or(key);
    anyhow::ensure!(
        !name.is_empty() && !name.contains('/') && !name.contains('\\') && name != "..",
        "Invalid backup name: {}",
        key
    );
    fs::create_dir_all(output_dir)
        .with_context(|| format!("Failed to create {}", output_dir.display()))?;
    let output = output_dir.join(name);
    let partial = output_dir.join(format!("{}.partial", name));
    info!("Downloading {}...", name);
    if let Err(e) = provider
        .download(&format!("{}{}", REMOTE_PREFIX, name), &partial)
        .await
    {
        let _ = fs::remove_file(&partial);
        return Err(e);
    }
    fs::rename(&partial, &output)?;
    info!("Downloaded to {}", output.display());
    Ok(output)
}

/// Delete backups older than `retention_days`, locally and (if a provider is given) remotely.
/// The newest backup in each place is always kept, even if it is older than the retention period.
pub async fn clean(
    provider: Option<&dyn StorageProvider>,
    config: &AppConfig,
    dry_run: bool,
) -> Result<()> {
    let retention = config.backup.retention_days();
    let cutoff = Utc::now() - chrono::Duration::days(retention as i64);
    info!(
        "Removing backups older than {} days{}",
        retention,
        if dry_run { " (dry run)" } else { "" }
    );

    let local = local_backups(Path::new(&config.backup.local_backup_dir))?;
    let keep_local = local.len().saturating_sub(1);
    for path in local.iter().take(keep_local) {
        let name = path.file_name().unwrap().to_string_lossy();
        let created = timestamp_from_name(&name).or_else(|| {
            fs::metadata(path)
                .and_then(|m| m.modified())
                .ok()
                .map(DateTime::<Utc>::from)
        });
        if created.is_some_and(|c| c < cutoff) {
            if dry_run {
                info!("Would delete local {}", path.display());
            } else {
                fs::remove_file(path)
                    .with_context(|| format!("Failed to delete {}", path.display()))?;
                info!("Deleted local {}", path.display());
            }
        }
    }

    let Some(provider) = provider else {
        return Ok(());
    };
    let remote = remote_backups(provider).await?;
    let keep_remote = remote.len().saturating_sub(1);
    for item in remote.iter().take(keep_remote) {
        let created = item
            .last_modified
            .or_else(|| timestamp_from_name(&item.key[REMOTE_PREFIX.len()..]));
        if created.is_some_and(|c| c < cutoff) {
            if dry_run {
                info!("Would delete remote {}", item.key);
            } else {
                provider.delete(&item.key).await?;
                info!("Deleted remote {}", item.key);
            }
        }
    }
    Ok(())
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} B", bytes)
    } else {
        format!("{:.2} {}", value, UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_names() {
        assert!(is_backup_file_name("backup-full-20240101-120000.tar.zst"));
        assert!(!is_backup_file_name(
            "backup-full-20240101-120000.tar.zst.partial"
        ));
        assert!(!is_backup_file_name("notes.txt"));
        assert!(is_backup_file_name(
            "backup-full-20240101-120000.tar.zst.age"
        ));
        assert!(timestamp_from_name("backup-full-20240101-120000.tar.zst.age").is_some());
        let ts = timestamp_from_name("backup-incr-20240101-120000.tar.zst").unwrap();
        assert_eq!(
            ts.with_timezone(&Local)
                .format(TIMESTAMP_FORMAT)
                .to_string(),
            "20240101-120000"
        );
        // full and incremental backups sort by time, not by type
        let mut names = [
            "backup-incr-20240102-000000.tar.zst",
            "backup-full-20240103-000000.tar.zst",
            "backup-full-20240101-000000.tar.zst",
        ];
        names.sort_by_cached_key(|n| sort_key(n));
        assert_eq!(names[0], "backup-full-20240101-000000.tar.zst");
        assert_eq!(names[1], "backup-incr-20240102-000000.tar.zst");
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(10), "10 B");
        assert_eq!(human_size(1536), "1.50 KiB");
    }
}

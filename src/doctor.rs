//! `zesty-backup doctor`: check the whole setup without making a backup.

use crate::archive::Excluder;
use crate::backup::{self, human_size, REMOTE_PREFIX};
use crate::config::AppConfig;
use crate::database::{self, DbKind};
use crate::encryption::{DecryptionKeys, Encryption};
use crate::providers::{self, StorageProvider};
use crate::report::{Report, Timer};
use anyhow::Result;
use chrono::Utc;
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

#[derive(Default)]
pub struct DoctorOptions<'a> {
    /// Private key to check encryption against (in addition to [encryption] identity_file).
    pub identity: Option<&'a Path>,
    /// Skip the remote storage round trip.
    pub skip_storage: bool,
}

pub async fn run(config: &AppConfig, options: &DoctorOptions<'_>) -> Report {
    let mut report = Report::new();
    check_local(config, &mut report);
    check_encryption(config, options.identity, &mut report);
    check_dependencies(config, &mut report);
    if crate::alerts::Alerter::new(config.alerts.as_ref()).is_configured() {
        report.pass(
            "Alerts",
            "configured (check delivery with `zesty-backup test-alerts`)",
        );
    } else {
        report
            .warn("Alerts", "none configured: failures will go unnoticed")
            .hint("add an [alerts] section (healthchecks.io, ntfy or a webhook)");
    }
    if options.skip_storage {
        report.skip("Storage", "skipped (--skip-storage)");
    } else {
        match providers::from_config(&config.storage).await {
            Ok(provider) => {
                report.pass(
                    "Storage credentials",
                    format!("{} client ready", config.storage.provider),
                );
                check_remote_backups(provider.as_ref(), config, &mut report).await;
                check_storage(provider.as_ref(), &mut report).await;
            }
            Err(e) => {
                let text = format!("{:#}", e);
                let hint = storage_hint(&text);
                let check = report.fail("Storage credentials", text);
                if let Some(h) = hint {
                    check.hint(h);
                }
            }
        }
    }
    report
}

fn check_local(config: &AppConfig, report: &mut Report) {
    let b = &config.backup;
    match fs::read_dir(&b.project_path) {
        Ok(entries) => {
            report.pass(
                "Project path",
                format!("{} ({} entries)", b.project_path, entries.count()),
            );
        }
        Err(e) => {
            report
                .fail("Project path", format!("{}: {}", b.project_path, e))
                .hint("set backup.project_path to a directory this user can read");
        }
    }

    for extra in b.additional_paths.iter().flatten() {
        if !Path::new(extra).exists() {
            report.warn(
                "Additional path",
                format!("{} does not exist (skipped by backups)", extra),
            );
        }
    }

    let writable = (|| -> Result<()> {
        fs::create_dir_all(&b.local_backup_dir)?;
        let mut f = tempfile::Builder::new()
            .prefix(".zesty-doctor-")
            .tempfile_in(&b.local_backup_dir)?;
        f.write_all(b"ok")?;
        Ok(())
    })();
    match writable {
        Ok(()) => {
            let local = backup::local_backups(Path::new(&b.local_backup_dir)).unwrap_or_default();
            let size: u64 = local
                .iter()
                .filter_map(|p| fs::metadata(p).ok())
                .map(|m| m.len())
                .sum();
            report.pass(
                "Backup directory",
                format!(
                    "{} is writable ({} backups, {})",
                    b.local_backup_dir,
                    local.len(),
                    human_size(size)
                ),
            );
        }
        Err(e) => {
            report
                .fail(
                    "Backup directory",
                    format!("{}: {:#}", b.local_backup_dir, e),
                )
                .hint("set backup.local_backup_dir to a directory this user can write");
        }
    }

    match Excluder::new(b.exclude.as_deref().unwrap_or_default()) {
        Ok(_) => {
            let n = b.exclude.as_ref().map_or(0, |e| e.len());
            report.pass("Exclude patterns", format!("{} pattern(s)", n));
        }
        Err(e) => {
            report.fail("Exclude patterns", format!("{:#}", e));
        }
    }

    if let Some(path) = config.logging.as_ref().and_then(|l| l.log_file()) {
        let ok = path
            .parent()
            .map(|d| fs::create_dir_all(d).is_ok())
            .unwrap_or(true)
            && fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .is_ok();
        if ok {
            report.pass("Log file", path.display().to_string());
        } else {
            report.fail("Log file", format!("cannot write {}", path.display()));
        }
    }
}

fn check_encryption(config: &AppConfig, identity: Option<&Path>, report: &mut Report) {
    let enc_config = config.encryption.as_ref();
    let encryption = match Encryption::from_config(enc_config) {
        Ok(Some(e)) => e,
        Ok(None) => {
            report
                .warn("Encryption", "off: archives are stored unencrypted")
                .hint("add an [encryption] section with an age public key (see README)");
            return;
        }
        Err(e) => {
            report.fail("Encryption", format!("{:#}", e));
            return;
        }
    };

    // Round trip: encrypt a sample with the config, decrypt it with the keys available here.
    let keys = DecryptionKeys::new(identity, enc_config);
    let is_passphrase = matches!(encryption, Encryption::Passphrase(_));
    if !is_passphrase && keys.identity_files.is_empty() {
        report
            .pass("Encryption", "on (public key); private key not available here, so not round-trip tested")
            .hint("run `zesty-backup doctor --identity key.txt` on a machine with the private key to prove it matches");
        return;
    }
    let sample = format!("zesty-doctor {}", Utc::now());
    let result = (|| -> Result<()> {
        let mut encrypted = Vec::new();
        let mut writer = encryption.wrap(&mut encrypted)?;
        writer.write_all(sample.as_bytes())?;
        writer.finish()?;
        let mut decrypted = String::new();
        keys.decrypt(std::io::Cursor::new(encrypted))?
            .read_to_string(&mut decrypted)?;
        anyhow::ensure!(decrypted == sample, "decrypted text does not match");
        Ok(())
    })();
    match result {
        Ok(()) if is_passphrase => {
            report.pass(
                "Encryption",
                "on (passphrase); encrypt/decrypt round trip OK",
            );
        }
        Ok(()) => {
            report.pass(
                "Encryption",
                "on (public key); your private key decrypts it",
            );
        }
        Err(e) => {
            report
                .fail("Encryption", format!("round trip failed: {:#}", e))
                .hint("the private key does not match the configured recipients: backups would not be restorable with it");
        }
    }
}

fn check_dependencies(config: &AppConfig, report: &mut Report) {
    if let Some(db) = config
        .database
        .as_ref()
        .filter(|d| d.enabled.unwrap_or(false))
    {
        match database::connection_test(db, &config.backup.project_path) {
            Ok(detail) => {
                report.pass("Database", detail);
            }
            Err(e) => {
                report
                    .fail("Database", format!("{:#}", e))
                    .hint("check [database] host/port/user and the password (config, DB_PASSWORD or .env)");
            }
        }
    }

    if let Some(system) = &config.system {
        for cmd in system.command_outputs.iter().flatten() {
            if cmd.enabled.unwrap_or(true) && which::which(&cmd.command).is_err() {
                report.warn(
                    "Command outputs",
                    format!("{} not found in PATH", cmd.command),
                );
            }
        }
        for unit in system
            .systemd_services
            .iter()
            .flatten()
            .chain(system.systemd_timers.iter().flatten())
        {
            if !Path::new("/etc/systemd/system").join(unit).exists() {
                report.warn(
                    "Systemd units",
                    format!("{} not in /etc/systemd/system", unit),
                );
            }
        }
    }

    if let Some(drill) = &config.drill {
        let db_kind = config
            .database
            .as_ref()
            .filter(|d| d.enabled.unwrap_or(false))
            .and_then(|d| DbKind::from_config(d).ok());
        let wants_container = drill.database_restore.unwrap_or(true)
            && matches!(
                db_kind,
                Some(DbKind::Postgres | DbKind::MySql | DbKind::MariaDb)
            );
        if wants_container {
            let runtime = drill.runtime();
            let ok = std::process::Command::new(runtime)
                .arg("info")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                report.pass("Drill containers", format!("{} is available", runtime));
            } else {
                report
                    .warn("Drill containers", format!("{} is not available: drills cannot test the database dump", runtime))
                    .hint("install Docker (or set drill.container_runtime = \"podman\") and make sure this user may use it");
            }
        }
    }
}

async fn check_remote_backups(
    provider: &dyn StorageProvider,
    config: &AppConfig,
    report: &mut Report,
) {
    let items = match backup::remote_backups(provider).await {
        Ok(items) => items,
        Err(e) => {
            let text = format!("{:#}", e);
            let hint = storage_hint(&text);
            let check = report.fail("Remote backups", text);
            if let Some(h) = hint {
                check.hint(h);
            }
            return;
        }
    };
    let Some(newest) = items.last() else {
        report
            .warn("Remote backups", "none yet")
            .hint("run `zesty-backup backup && zesty-backup upload`");
        return;
    };
    let name = &newest.key[REMOTE_PREFIX.len()..];
    let created = newest
        .last_modified
        .or_else(|| backup::timestamp_from_name(name));
    let age_hours = created.map(|c| (Utc::now() - c).num_hours());
    let limit = 2 * config.backup.upload_interval_hours.unwrap_or(24) as i64;
    let detail = format!(
        "{} backup(s), newest {} ({})",
        items.len(),
        name,
        age_hours.map_or("age unknown".into(), |h| format!("{}h old", h))
    );
    match age_hours {
        Some(h) if h > limit => {
            report.warn("Remote backups", detail).hint(format!(
                "newer than {}h expected: is the daemon or cron job running?",
                limit
            ));
        }
        _ => {
            report.pass("Remote backups", detail);
        }
    }
}

/// Upload a small test file, list it, download it, compare, delete it.
pub async fn check_storage(provider: &dyn StorageProvider, report: &mut Report) {
    let key = format!(
        "{}zesty-doctor-{}.txt",
        REMOTE_PREFIX,
        Utc::now().format("%Y%m%d-%H%M%S")
    );
    let content = test_content(256 * 1024);
    let dir = match tempfile::tempdir() {
        Ok(d) => d,
        Err(e) => {
            report.fail(
                "Storage round trip",
                format!("cannot create temp dir: {}", e),
            );
            return;
        }
    };
    let local = dir.path().join("upload.txt");
    if let Err(e) = fs::write(&local, &content) {
        report.fail(
            "Storage round trip",
            format!("cannot write temp file: {}", e),
        );
        return;
    }

    let t = Timer::start();
    if let Err(e) = provider.upload(&key, &local).await {
        let text = format!("{:#}", e);
        let hint = storage_hint(&text);
        let check = report.fail("Storage upload", text);
        if let Some(h) = hint {
            check.hint(h);
        }
        return;
    }
    report.pass(
        "Storage upload",
        format!(
            "{} ({}) in {}",
            key,
            human_size(content.len() as u64),
            t.elapsed()
        ),
    );

    // Some services list new files with a small delay.
    let listed = retry(|| async {
        let items = provider.list(REMOTE_PREFIX).await?;
        Ok(items.into_iter().find(|i| i.key == key))
    })
    .await;
    match listed {
        Ok(Some(item)) if item.size == content.len() as u64 => {
            report.pass("Storage list", "test file listed with the right size");
        }
        Ok(Some(item)) => {
            report.fail(
                "Storage list",
                format!(
                    "test file listed with size {} instead of {}",
                    item.size,
                    content.len()
                ),
            );
        }
        Ok(None) => {
            report
                .fail("Storage list", "uploaded test file does not appear in the listing")
                .hint("list/clean/upload-skipping will not work: check the folder (bucket_id) settings");
        }
        Err(e) => {
            report.fail("Storage list", format!("{:#}", e));
        }
    }

    let downloaded = dir.path().join("download.txt");
    let t = Timer::start();
    match provider.download(&key, &downloaded).await {
        Ok(()) => match fs::read(&downloaded) {
            Ok(data) if data == content => {
                report.pass(
                    "Storage download",
                    format!("identical content, {}", t.elapsed()),
                );
            }
            Ok(data) => {
                report.fail(
                    "Storage download",
                    format!(
                        "content differs ({} bytes instead of {})",
                        data.len(),
                        content.len()
                    ),
                );
            }
            Err(e) => {
                report.fail("Storage download", e.to_string());
            }
        },
        Err(e) => {
            let text = format!("{:#}", e);
            let hint = storage_hint(&text);
            let check = report.fail("Storage download", text);
            if let Some(h) = hint {
                check.hint(h);
            }
        }
    }

    match provider.delete(&key).await {
        Ok(()) => {
            let gone = retry(|| async {
                let items = provider.list(REMOTE_PREFIX).await?;
                Ok(items.iter().all(|i| i.key != key).then_some(()))
            })
            .await;
            match gone {
                Ok(Some(())) => {
                    report.pass("Storage delete", "test file deleted");
                }
                _ => {
                    report.warn(
                        "Storage delete",
                        "delete succeeded but the file is still listed",
                    );
                }
            }
        }
        Err(e) => {
            report
                .fail("Storage delete", format!("{:#}", e))
                .hint(format!(
                "retention (`clean`) will not work without delete permission; remove {} by hand",
                key
            ));
        }
    }
}

/// Try up to 3 times, 2 s apart, until `f` returns `Some`.
async fn retry<T, F, Fut>(f: F) -> Result<Option<T>>
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<Option<T>>>,
{
    for attempt in 0..3 {
        if let Some(v) = f().await? {
            return Ok(Some(v));
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }
    Ok(None)
}

fn test_content(len: usize) -> Vec<u8> {
    // Not compressible, so a proxy that compresses cannot hide size problems.
    let mut x: u64 = Utc::now().timestamp_nanos_opt().unwrap_or(1) as u64 | 1;
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.extend_from_slice(&x.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// A likely fix for common storage error messages.
pub fn storage_hint(error: &str) -> Option<&'static str> {
    let e = error.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| e.contains(w));
    if has(&[
        "signaturedoesnotmatch",
        "invalidaccesskeyid",
        "401",
        "unauthorized",
        "invalid_grant",
        "invalid_token",
        "expired",
    ]) {
        Some("credentials are wrong or expired (for S3-compatible services also check `region`)")
    } else if has(&["403", "accessdenied", "forbidden", "insufficient"]) {
        Some("the credentials work but lack permissions: allow list, read, write and delete on the bucket/folder")
    } else if has(&[
        "nosuchbucket",
        "containernotfound",
        "bucket not found",
        "404",
    ]) {
        Some("the bucket/container/folder was not found: check its name (and region)")
    } else if has(&[
        "dns",
        "connect",
        "timed out",
        "timeout",
        "connection refused",
    ]) {
        Some("network problem: check `endpoint`, DNS and firewall")
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        assert!(storage_hint("HTTP 403 Forbidden")
            .unwrap()
            .contains("permissions"));
        assert!(storage_hint("NoSuchBucket: nope")
            .unwrap()
            .contains("not found"));
        assert!(storage_hint("all good?").is_none());
        let c = test_content(1000);
        assert_eq!(c.len(), 1000);
    }
}

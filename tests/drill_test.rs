//! Tests for `doctor` and `drill`, with a local folder as "cloud storage".

mod common;

use anyhow::Result;
use async_trait::async_trait;
use common::{DirProvider, Fixture};
use std::fs;
use std::path::Path;
use zesty_backup::backup;
use zesty_backup::config::AppConfig;
use zesty_backup::doctor::{self, DoctorOptions};
use zesty_backup::drill::{self, DrillOptions};
use zesty_backup::providers::{BackupItem, StorageProvider};
use zesty_backup::report::{Report, Status};

/// A config for the fixture project with extra TOML appended.
fn config(fx: &Fixture, extra: &str) -> AppConfig {
    AppConfig::parse(&format!(
        r#"
        [storage]
        provider = "s3"
        bucket = "unused"
        [backup]
        local_backup_dir = "{backups}"
        project_path = "{project}"
        {extra}
        "#,
        backups = fx.path("drill-backups").display(),
        project = fx.path("myapp").display(),
    ))
    .unwrap()
}

fn status_of(report: &Report, name: &str) -> Vec<Status> {
    report
        .checks
        .iter()
        .filter(|c| c.name == name)
        .map(|c| c.status)
        .collect()
}

async fn backup_and_upload(config: &AppConfig, remote: &DirProvider) -> String {
    let path = backup::create_backup(config, false).unwrap();
    backup::upload(remote, config, None).await.unwrap();
    path.file_name().unwrap().to_string_lossy().to_string()
}

#[tokio::test]
async fn drill_passes_on_a_good_backup() {
    let fx = Fixture::new();
    let config = config(
        &fx,
        r#"
        [drill]
        max_age_hours = 24
        files = ["project/myapp/src/*.rs", "project/myapp/data.bin"]
        commands = [
            { name = "main.rs not empty", run = "test -s project/myapp/src/main.rs" },
            { run = "test -d \"$ZESTY_RESTORE_DIR/project\"" },
        ]
        "#,
    );
    let remote = DirProvider(fx.path("remote"));
    let name = backup_and_upload(&config, &remote).await;

    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    let report = &outcome.report;
    assert!(report.passed(), "{}", report);
    assert_eq!(outcome.backup.as_deref(), Some(name.as_str()));
    for check in [
        "Find backup",
        "Freshness",
        "Download",
        "Extract",
        "Project files",
    ] {
        assert_eq!(
            status_of(report, check),
            vec![Status::Pass],
            "{}\n{}",
            check,
            report
        );
    }
    assert_eq!(
        status_of(report, "File check"),
        vec![Status::Pass, Status::Pass]
    );
    assert_eq!(
        status_of(report, "Command"),
        vec![Status::Pass, Status::Pass]
    );
    assert!(outcome.kept_dir.is_none());

    // The result is remembered for `status`.
    let last = drill::load_last(&config).expect("last drill saved");
    assert!(last.passed);
    assert_eq!(last.backup.as_deref(), Some(name.as_str()));
}

#[tokio::test]
async fn drill_reports_missing_files_and_failing_commands() {
    let fx = Fixture::new();
    let config = config(
        &fx,
        r#"
        [drill]
        files = ["project/myapp/does-not-exist.txt"]
        commands = [{ name = "always fails", run = "echo broken >&2; exit 3" }]
        "#,
    );
    let remote = DirProvider(fx.path("remote"));
    backup_and_upload(&config, &remote).await;

    let outcome = drill::run(
        &config,
        Some(&remote),
        &DrillOptions {
            keep: true,
            ..Default::default()
        },
    )
    .await;
    let report = &outcome.report;
    assert!(!report.passed());
    assert_eq!(status_of(report, "File check"), vec![Status::Fail]);
    let cmd = report.checks.iter().find(|c| c.name == "Command").unwrap();
    assert_eq!(cmd.status, Status::Fail);
    assert!(cmd.detail.contains("broken"), "{}", cmd.detail);
    let kept = outcome.kept_dir.expect("--keep keeps the files");
    assert!(kept.join("restored/project/myapp/src/main.rs").exists());
    fs::remove_dir_all(kept).unwrap();
    assert!(!drill::load_last(&config).unwrap().passed);
}

#[tokio::test]
async fn drill_fails_without_backups_or_when_too_old() {
    let fx = Fixture::new();
    let config = config(&fx, "[drill]\nmax_age_hours = 24");
    let remote = DirProvider(fx.path("remote"));

    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    assert_eq!(
        status_of(&outcome.report, "Find backup"),
        vec![Status::Fail]
    );

    // A real archive, but with a name from 2020.
    let path = backup::create_backup(&config, false).unwrap();
    let old = path.with_file_name("backup-incr-20200101-000000.tar.zst");
    fs::rename(&path, &old).unwrap();
    backup::upload(&remote, &config, None).await.unwrap();
    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    assert_eq!(
        status_of(&outcome.report, "Freshness"),
        vec![Status::Fail],
        "{}",
        outcome.report
    );
    // ...but the content itself is still checked.
    assert_eq!(status_of(&outcome.report, "Extract"), vec![Status::Pass]);
}

#[tokio::test]
async fn drill_detects_a_corrupted_remote_backup() {
    let fx = Fixture::new();
    let config = config(&fx, "");
    let remote = DirProvider(fx.path("remote"));
    let name = backup_and_upload(&config, &remote).await;

    let remote_file = fx.path("remote/backups").join(&name);
    let data = fs::read(&remote_file).unwrap();
    fs::write(&remote_file, &data[..data.len() / 2]).unwrap();

    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    assert_eq!(
        status_of(&outcome.report, "Extract"),
        vec![Status::Fail],
        "{}",
        outcome.report
    );
    assert!(!outcome.report.passed());
}

#[tokio::test]
async fn drill_local_backup() {
    let fx = Fixture::new();
    let config = config(&fx, "");
    backup::create_backup(&config, false).unwrap();
    let outcome = drill::run(
        &config,
        None,
        &DrillOptions {
            local: true,
            ..Default::default()
        },
    )
    .await;
    assert!(outcome.report.passed(), "{}", outcome.report);
    assert!(status_of(&outcome.report, "Download").is_empty());
}

#[tokio::test]
async fn drill_encrypted_backup() {
    use age::secrecy::ExposeSecret;
    let fx = Fixture::new();
    let identity = age::x25519::Identity::generate();
    let key = fx.path("key.txt");
    fs::write(&key, identity.to_string().expose_secret()).unwrap();
    let config = config(
        &fx,
        &format!("[encryption]\nrecipients = [\"{}\"]", identity.to_public()),
    );
    let remote = DirProvider(fx.path("remote"));
    backup_and_upload(&config, &remote).await;

    // Without the private key: the drill fails, saying it could not look inside.
    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    assert_eq!(
        status_of(&outcome.report, "Decrypt"),
        vec![Status::Fail],
        "{}",
        outcome.report
    );
    assert!(status_of(&outcome.report, "Project files").is_empty());

    // With it: full check.
    let options = DrillOptions {
        identity: Some(&key),
        ..Default::default()
    };
    let outcome = drill::run(&config, Some(&remote), &options).await;
    assert!(outcome.report.passed(), "{}", outcome.report);
    assert_eq!(
        status_of(&outcome.report, "Decrypt & extract"),
        vec![Status::Pass]
    );
}

#[tokio::test]
async fn drill_sqlite_dump_is_found() {
    let fx = Fixture::new();
    let db = fx.path("app.db");
    // Minimal valid header; the integrity check only runs when sqlite3 is installed.
    let mut bytes = b"SQLite format 3\0".to_vec();
    bytes.resize(4096, 0);
    fs::write(&db, bytes).unwrap();
    let config = config(
        &fx,
        &format!(
            "[database]\nenabled = true\ntype = \"sqlite\"\ndatabase = \"{}\"",
            db.display()
        ),
    );
    let remote = DirProvider(fx.path("remote"));
    backup_and_upload(&config, &remote).await;
    let outcome = drill::run(&config, Some(&remote), &DrillOptions::default()).await;
    assert_eq!(
        status_of(&outcome.report, "Database dump"),
        vec![Status::Pass],
        "{}",
        outcome.report
    );
}

// ───────────────────────── doctor ─────────────────────────

#[tokio::test]
async fn doctor_storage_round_trip() {
    let fx = Fixture::new();
    let remote = DirProvider(fx.path("remote"));
    let mut report = Report::new();
    doctor::check_storage(&remote, &mut report).await;
    assert!(report.passed(), "{}", report);
    for check in [
        "Storage upload",
        "Storage list",
        "Storage download",
        "Storage delete",
    ] {
        assert_eq!(
            status_of(&report, check),
            vec![Status::Pass],
            "{}\n{}",
            check,
            report
        );
    }
    // The test file is gone afterwards.
    let left: Vec<_> = fs::read_dir(fx.path("remote/backups")).unwrap().collect();
    assert!(left.is_empty());
}

/// Uploads work, but deleting is not allowed.
struct NoDelete(DirProvider);

#[async_trait]
impl StorageProvider for NoDelete {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        self.0.upload(key, file_path).await
    }
    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        self.0.download(key, output_path).await
    }
    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        self.0.list(prefix).await
    }
    async fn delete(&self, _key: &str) -> Result<()> {
        anyhow::bail!("HTTP 403 Forbidden: AccessDenied")
    }
}

#[tokio::test]
async fn doctor_explains_missing_delete_permission() {
    let fx = Fixture::new();
    let remote = NoDelete(DirProvider(fx.path("remote")));
    let mut report = Report::new();
    doctor::check_storage(&remote, &mut report).await;
    let delete = report
        .checks
        .iter()
        .find(|c| c.name == "Storage delete")
        .unwrap();
    assert_eq!(delete.status, Status::Fail);
    assert!(
        delete.hint.as_deref().unwrap().contains("clean"),
        "{:?}",
        delete.hint
    );
    assert_eq!(status_of(&report, "Storage download"), vec![Status::Pass]);
}

#[tokio::test]
async fn doctor_local_checks() {
    use age::secrecy::ExposeSecret;
    let fx = Fixture::new();
    let options = DoctorOptions {
        skip_storage: true,
        ..Default::default()
    };

    let report = doctor::run(&config(&fx, ""), &options).await;
    assert!(report.passed(), "{}", report);
    assert_eq!(status_of(&report, "Project path"), vec![Status::Pass]);
    assert_eq!(status_of(&report, "Backup directory"), vec![Status::Pass]);
    assert_eq!(
        status_of(&report, "Encryption"),
        vec![Status::Warn],
        "unencrypted is a warning"
    );

    // A private key that does not match the configured public key is caught.
    let configured = age::x25519::Identity::generate();
    let other = age::x25519::Identity::generate();
    let wrong_key = fx.path("wrong.txt");
    let right_key = fx.path("right.txt");
    fs::write(&wrong_key, other.to_string().expose_secret()).unwrap();
    fs::write(&right_key, configured.to_string().expose_secret()).unwrap();
    let enc = config(
        &fx,
        &format!(
            "[encryption]\nrecipients = [\"{}\"]",
            configured.to_public()
        ),
    );
    let report = doctor::run(
        &enc,
        &DoctorOptions {
            identity: Some(&wrong_key),
            skip_storage: true,
        },
    )
    .await;
    assert_eq!(
        status_of(&report, "Encryption"),
        vec![Status::Fail],
        "{}",
        report
    );
    let report = doctor::run(
        &enc,
        &DoctorOptions {
            identity: Some(&right_key),
            skip_storage: true,
        },
    )
    .await;
    assert_eq!(
        status_of(&report, "Encryption"),
        vec![Status::Pass],
        "{}",
        report
    );

    // Broken paths fail.
    let bad = AppConfig::parse(
        r#"
        [storage]
        provider = "s3"
        [backup]
        local_backup_dir = "/proc/zesty-cannot-write-here"
        project_path = "/does/not/exist"
        "#,
    )
    .unwrap();
    let report = doctor::run(&bad, &options).await;
    assert_eq!(status_of(&report, "Project path"), vec![Status::Fail]);
    assert_eq!(status_of(&report, "Backup directory"), vec![Status::Fail]);
}

//! Anomaly checks on real backups of the fixture project.

mod common;

use common::Fixture;
use std::fs;
use std::path::Path;
use zesty_backup::{anomaly, backup};

fn backups(fx: &Fixture) -> usize {
    backup::local_backups(Path::new(&fx.config.backup.local_backup_dir))
        .unwrap()
        .len()
}

/// Backups made in the same second share a name; space them out.
fn next_second() {
    std::thread::sleep(std::time::Duration::from_millis(1100));
}

#[tokio::test]
async fn deleted_files_pause_retention_until_accepted() {
    let fx = Fixture::new();
    let dir = Path::new(&fx.config.backup.local_backup_dir).to_path_buf();
    fs::create_dir_all(&dir).unwrap();
    // Two old backups that retention would normally delete.
    for name in [
        "backup-incr-20200101-000000.tar.zst",
        "backup-incr-20200102-000000.tar.zst",
    ] {
        fs::write(dir.join(name), name).unwrap();
    }

    let first = backup::create_backup_checked(&fx.config, false).unwrap();
    assert!(first.anomalies.is_empty(), "nothing to compare with yet");
    next_second();
    let second = backup::create_backup_checked(&fx.config, false).unwrap();
    assert!(second.anomalies.is_empty(), "{:?}", second.anomalies);
    assert!(
        second.stats.files >= 4 && second.stats.bytes > 2_000_000,
        "{:?}",
        second.stats
    );

    // Most of the project disappears.
    fs::remove_file(fx.path("myapp/data.bin")).unwrap();
    next_second();
    let third = backup::create_backup_checked(&fx.config, false).unwrap();
    assert!(
        third
            .anomalies
            .iter()
            .any(|a| a.starts_with("total size dropped")),
        "{:?}",
        third.anomalies
    );
    assert!(anomaly::retention_pause_reason(&fx.config).is_some());

    // Retention is paused: the old backups survive.
    let before = backups(&fx);
    backup::clean(None, &fx.config, false).await.unwrap();
    assert_eq!(backups(&fx), before);

    // The user confirms the deletion was intended: retention resumes.
    assert_eq!(
        anomaly::accept(&fx.config).unwrap(),
        Some(third.stats.name.clone())
    );
    assert!(anomaly::retention_pause_reason(&fx.config).is_none());
    backup::clean(None, &fx.config, false).await.unwrap();
    assert_eq!(backups(&fx), before - 2, "the two 2020 backups are gone");

    // And the smaller project is the new normal.
    next_second();
    let fourth = backup::create_backup_checked(&fx.config, false).unwrap();
    assert!(fourth.anomalies.is_empty(), "{:?}", fourth.anomalies);
}

#[test]
fn encrypted_looking_data_is_flagged() {
    let fx = Fixture::new();
    backup::create_backup_checked(&fx.config, false).unwrap();
    next_second();

    // "Ransomware" rewrites the (very compressible) data file with random bytes.
    let len = fs::metadata(fx.path("myapp/data.bin")).unwrap().len() as usize;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    let noise: Vec<u8> = (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect();
    fs::write(fx.path("myapp/data.bin"), noise).unwrap();

    let outcome = backup::create_backup_checked(&fx.config, false).unwrap();
    assert_eq!(outcome.anomalies.len(), 1, "{:?}", outcome.anomalies);
    assert!(outcome.anomalies[0].contains("stopped compressing"));
}

#[test]
fn anomaly_checks_can_be_disabled() {
    let fx = Fixture::new();
    let mut config = fx.config;
    config.anomaly = Some(zesty_backup::config::AnomalyConfig {
        enabled: Some(false),
        ..Default::default()
    });
    backup::create_backup_checked(&config, false).unwrap();
    next_second();
    fs::remove_file(fx.dir.path().join("myapp/data.bin")).unwrap();
    let outcome = backup::create_backup_checked(&config, false).unwrap();
    assert!(outcome.anomalies.is_empty());
    assert!(anomaly::retention_pause_reason(&config).is_none());
}

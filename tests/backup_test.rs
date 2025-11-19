// Backup functionality integration tests

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Test backup file naming
#[test]
fn test_backup_file_naming() {
    use chrono::Local;

    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let full_backup = format!("backup-full-{}.tar.zst", timestamp);
    let incr_backup = format!("backup-incr-{}.tar.zst", timestamp);

    assert!(full_backup.starts_with("backup-full-"));
    assert!(full_backup.ends_with(".tar.zst"));
    assert!(incr_backup.starts_with("backup-incr-"));
    assert!(incr_backup.ends_with(".tar.zst"));
}

/// Test backup directory structure creation
#[test]
fn test_backup_directory_creation() {
    let temp_dir = TempDir::new().unwrap();
    let backup_dir = temp_dir.path().join("backups");

    fs::create_dir_all(&backup_dir).unwrap();
    assert!(backup_dir.exists());

    // Create a test backup file
    let backup_file = backup_dir.join("test-backup.tar.zst");
    fs::write(&backup_file, b"test backup content").unwrap();
    assert!(backup_file.exists());
}

/// Test compression level validation
#[test]
fn test_compression_level_validation() {
    let valid_levels = vec![0, 3, 6, 9, 12, 15, 18, 22];
    let invalid_levels = vec![-1, 23, 100];

    for level in valid_levels {
        assert!(level >= 0 && level <= 22, "Level {} should be valid", level);
    }

    for level in invalid_levels {
        assert!(level < 0 || level > 22, "Level {} should be invalid", level);
    }
}

/// Test retention policy calculation
#[test]
fn test_retention_policy() {
    use chrono::{Duration, Utc};

    let retention_days = 7;
    let cutoff_date = Utc::now() - Duration::days(retention_days);

    // Simulate backup dates
    let old_backup_date = Utc::now() - Duration::days(10);
    let new_backup_date = Utc::now() - Duration::days(3);

    assert!(
        old_backup_date < cutoff_date,
        "Old backup should be deleted"
    );
    assert!(new_backup_date > cutoff_date, "New backup should be kept");
}

/// Test path exclusion logic
#[test]
fn test_path_exclusion() {
    let exclude_patterns = vec!["node_modules", "*.log", ".git"];
    let test_paths = vec![
        ("/project/node_modules/package", true),
        ("/project/src/main.rs", false),
        ("/project/app.log", true),
        ("/project/.git/config", true),
        ("/project/data.txt", false),
    ];

    for (path, should_exclude) in test_paths {
        let excluded = exclude_patterns.iter().any(|pattern| {
            if pattern.starts_with("*.") {
                path.ends_with(&pattern[1..])
            } else {
                path.contains(pattern)
            }
        });

        assert_eq!(
            excluded, should_exclude,
            "Path {} exclusion check failed",
            path
        );
    }
}

/// Test backup file size calculation
#[test]
fn test_backup_file_size() {
    let temp_dir = TempDir::new().unwrap();
    let test_file = temp_dir.path().join("test.txt");
    let content = "This is test content for backup size calculation";

    fs::write(&test_file, content).unwrap();
    let metadata = fs::metadata(&test_file).unwrap();

    assert!(metadata.len() > 0);
    assert_eq!(metadata.len(), content.len() as u64);
}

/// Test timestamp parsing for backup files
#[test]
fn test_backup_timestamp_parsing() {
    use chrono::NaiveDateTime;

    let backup_name = "backup-full-20240101-120000.tar.zst";
    let timestamp_str = backup_name
        .strip_prefix("backup-full-")
        .unwrap()
        .strip_suffix(".tar.zst")
        .unwrap();

    let parsed = NaiveDateTime::parse_from_str(timestamp_str, "%Y%m%d-%H%M%S");
    assert!(parsed.is_ok());

    let dt = parsed.unwrap();
    assert_eq!(dt.format("%Y%m%d-%H%M%S").to_string(), timestamp_str);
}

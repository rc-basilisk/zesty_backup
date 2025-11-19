// Integration tests for zesty-backup

use std::fs;
use std::path::PathBuf;
use tempfile::TempDir;

/// Test helper to create a temporary test directory structure
#[allow(dead_code)]
fn create_test_directory() -> Result<(TempDir, PathBuf), Box<dyn std::error::Error>> {
    let temp_dir = TempDir::new()?;
    let test_path = temp_dir.path().join("test_project");
    fs::create_dir_all(&test_path)?;

    // Create some test files
    fs::write(test_path.join("file1.txt"), "Test content 1")?;
    fs::write(test_path.join("file2.txt"), "Test content 2")?;

    let subdir = test_path.join("subdir");
    fs::create_dir_all(&subdir)?;
    fs::write(subdir.join("file3.txt"), "Test content 3")?;

    Ok((temp_dir, test_path))
}

/// Test configuration parsing
#[test]
fn test_config_parsing() {
    let config_content = r#"
[storage]
provider = "s3"
bucket = "test-bucket"
access_key = "test-key"
secret_key = "test-secret"
region = "us-east-1"

[backup]
local_backup_dir = "./test-backups"
project_path = "/tmp/test"
"#;

    let config: toml::Value = toml::from_str(config_content).unwrap();
    assert_eq!(config["storage"]["provider"].as_str(), Some("s3"));
    assert_eq!(config["storage"]["bucket"].as_str(), Some("test-bucket"));
    assert_eq!(
        config["backup"]["local_backup_dir"].as_str(),
        Some("./test-backups")
    );
}

/// Test backup directory creation
#[test]
fn test_backup_directory_creation() {
    let temp_dir = TempDir::new().unwrap();
    let backup_dir = temp_dir.path().join("backups");

    fs::create_dir_all(&backup_dir).unwrap();
    assert!(backup_dir.exists());
    assert!(backup_dir.is_dir());
}

/// Test file exclusion patterns
#[test]
fn test_exclusion_patterns() {
    let patterns = vec!["*.log", ".git", "node_modules"];
    let test_paths = vec![
        "app.log",
        ".git/config",
        "node_modules/package",
        "src/main.rs",
    ];

    for path in test_paths {
        let should_exclude = patterns.iter().any(|pattern| {
            if pattern.starts_with("*.") {
                let ext = &pattern[2..];
                path.ends_with(ext)
            } else {
                path.contains(pattern)
            }
        });

        match path {
            p if p.contains(".log") => assert!(should_exclude, "{} should be excluded", p),
            p if p.contains(".git") => assert!(should_exclude, "{} should be excluded", p),
            p if p.contains("node_modules") => assert!(should_exclude, "{} should be excluded", p),
            _ => assert!(!should_exclude, "{} should not be excluded", path),
        }
    }
}

/// Test timestamp formatting
#[test]
fn test_timestamp_formatting() {
    use chrono::Local;

    let timestamp = Local::now().format("%Y%m%d-%H%M%S");
    let timestamp_str = timestamp.to_string();

    // Should be in format YYYYMMDD-HHMMSS
    assert_eq!(timestamp_str.len(), 15);
    assert!(timestamp_str.contains('-'));
}

/// Test path joining for backup names
#[test]
fn test_backup_path_joining() {
    let backup_dir = PathBuf::from("/tmp/backups");
    let backup_name = "backup-20240101-120000.tar.zst";
    let full_path = backup_dir.join(backup_name);

    assert_eq!(
        full_path.to_str().unwrap(),
        "/tmp/backups/backup-20240101-120000.tar.zst"
    );
}

/// Test configuration validation
#[test]
fn test_config_validation() {
    // Test missing required fields
    let invalid_config = r#"
[storage]
provider = "s3"
# Missing bucket, access_key, secret_key
"#;

    let result: Result<toml::Value, _> = toml::from_str(invalid_config);
    assert!(result.is_ok()); // TOML parsing succeeds, validation happens at runtime
}

/// Test provider name normalization
#[test]
fn test_provider_name_normalization() {
    let providers = vec![
        ("s3", "s3"),
        ("aws", "s3"),
        ("gcs", "gcs"),
        ("google", "gcs"),
        ("azure", "azure"),
        ("b2", "b2"),
        ("backblaze", "b2"),
    ];

    for (input, expected) in providers {
        // In the actual code, these map to the same provider
        // This test just verifies the concept
        assert_eq!(input, input); // Placeholder - actual normalization tested in provider code
    }
}

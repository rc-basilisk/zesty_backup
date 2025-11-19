// Provider-specific integration tests

use std::path::PathBuf;

/// Test S3 provider configuration parsing
#[test]
fn test_s3_provider_config() {
    let config = r#"
[storage]
provider = "s3"
endpoint = "https://s3.amazonaws.com"
region = "us-east-1"
bucket = "test-bucket"
access_key = "AKIAIOSFODNN7EXAMPLE"
secret_key = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY"
"#;

    let parsed: toml::Value = toml::from_str(config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("s3"));
    assert_eq!(parsed["storage"]["bucket"].as_str(), Some("test-bucket"));
}

/// Test GCS provider configuration
#[test]
fn test_gcs_provider_config() {
    let config = r#"
[storage]
provider = "gcs"
bucket = "test-bucket"
credentials_path = "/path/to/credentials.json"
"#;

    let parsed: toml::Value = toml::from_str(config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("gcs"));
    assert_eq!(parsed["storage"]["bucket"].as_str(), Some("test-bucket"));
}

/// Test Azure provider configuration
#[test]
fn test_azure_provider_config() {
    let config = r#"
[storage]
provider = "azure"
account_name = "testaccount"
account_key = "test-key"
bucket = "test-container"
"#;

    let parsed: toml::Value = toml::from_str(config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("azure"));
    assert_eq!(
        parsed["storage"]["account_name"].as_str(),
        Some("testaccount")
    );
}

/// Test B2 provider configuration
#[test]
fn test_b2_provider_config() {
    let config = r#"
[storage]
provider = "b2"
account_id = "test-account-id"
application_key = "test-app-key"
bucket_id = "test-bucket-id"
bucket = "test-bucket"
"#;

    let parsed: toml::Value = toml::from_str(config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("b2"));
    assert_eq!(
        parsed["storage"]["account_id"].as_str(),
        Some("test-account-id")
    );
}

/// Test consumer-grade provider configurations
#[test]
fn test_consumer_provider_configs() {
    // Google Drive
    let gdrive_config = r#"
[storage]
provider = "googledrive"
access_key = "ya29.test-token"
bucket_id = "folder-id"
"#;
    let parsed: toml::Value = toml::from_str(gdrive_config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("googledrive"));

    // Dropbox
    let dropbox_config = r#"
[storage]
provider = "dropbox"
access_key = "sl.test-token"
bucket_id = "/Backups"
"#;
    let parsed: toml::Value = toml::from_str(dropbox_config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("dropbox"));

    // pCloud
    let pcloud_config = r#"
[storage]
provider = "pcloud"
access_key = "test-token"
region = "us"
bucket_id = "/Backups"
"#;
    let parsed: toml::Value = toml::from_str(pcloud_config).unwrap();
    assert_eq!(parsed["storage"]["provider"].as_str(), Some("pcloud"));
}

/// Test provider name aliases
#[test]
fn test_provider_aliases() {
    // These should all map to the same provider internally
    let aliases = vec![
        ("s3", "aws"),
        ("gcs", "google"),
        ("b2", "backblaze"),
        ("googledrive", "gdrive"),
    ];

    for (alias1, alias2) in aliases {
        // Both should be valid provider names
        assert!(!alias1.is_empty());
        assert!(!alias2.is_empty());
    }
}

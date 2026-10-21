//! End-to-end tests of the backup workflow, using a local directory as "cloud storage".

mod common;

use common::{DirProvider, Fixture};
use std::fs;
use std::path::{Path, PathBuf};
use zesty_backup::archive;
use zesty_backup::backup;
use zesty_backup::config::AppConfig;

#[test]
fn backup_restore_roundtrip() {
    let fx = Fixture::new();
    let archive_path = backup::create_backup(&fx.config, true).unwrap();
    let name = archive_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        name.starts_with("backup-full-") && name.ends_with(".tar.zst"),
        "{}",
        name
    );
    assert!(!archive_path.with_extension("zst.partial").exists());

    assert!(archive::verify(&archive_path, &Default::default()).unwrap() > 0);

    let out = fx.path("restored");
    archive::extract(&archive_path, &out, &Default::default()).unwrap();
    let root = out.join("project/myapp");
    assert_eq!(
        fs::read_to_string(root.join("src/main.rs")).unwrap(),
        "fn main() {}"
    );
    assert_eq!(fs::read(root.join("data.bin")).unwrap().len(), 2_000_000);
    assert!(root.join("empty").is_dir(), "empty directories are kept");
    assert!(!root.join("node_modules").exists(), "excluded directory");
    assert!(!root.join("debug.log").exists(), "excluded glob");
    assert!(
        !root.join("backups").exists(),
        "backup dir must not be in the backup"
    );
    assert_eq!(
        fs::read_to_string(out.join("system/nginx.conf")).unwrap(),
        "events {}"
    );
    assert_eq!(
        fs::read_to_string(out.join("commands/echo.txt")).unwrap(),
        "hello\n"
    );

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(root.join("run.sh"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o750, "permissions are preserved");
        let archive_mode = fs::metadata(&archive_path).unwrap().permissions().mode();
        assert_eq!(archive_mode & 0o777, 0o600, "archives are private");
        let link = fs::read_link(root.join("link.rs")).unwrap();
        assert_eq!(
            link,
            Path::new("src/main.rs"),
            "symlinks are kept as symlinks"
        );
    }
}

#[test]
fn truncated_archive_fails_verification() {
    let fx = Fixture::new();
    let archive_path = backup::create_backup(&fx.config, false).unwrap();
    let data = fs::read(&archive_path).unwrap();
    fs::write(&archive_path, &data[..data.len() / 2]).unwrap();
    assert!(archive::verify(&archive_path, &Default::default()).is_err());
}

#[tokio::test]
async fn upload_list_download_clean() {
    let fx = Fixture::new();
    let remote = DirProvider(fx.path("remote"));
    let backup_dir = PathBuf::from(&fx.config.backup.local_backup_dir);
    fs::create_dir_all(&backup_dir).unwrap();

    // Two old backups and a current one.
    for name in [
        "backup-incr-20200101-000000.tar.zst",
        "backup-full-20200102-000000.tar.zst",
    ] {
        fs::write(backup_dir.join(name), name).unwrap();
    }
    fs::write(backup_dir.join("unrelated.txt"), "keep me").unwrap();
    let current = backup::create_backup(&fx.config, false).unwrap();
    let current_name = current.file_name().unwrap().to_string_lossy().to_string();

    assert_eq!(backup::upload(&remote, &fx.config, None).await.unwrap(), 3);
    // Already uploaded: nothing to do the second time.
    assert_eq!(backup::upload(&remote, &fx.config, None).await.unwrap(), 0);

    let listed = backup::remote_backups(&remote).await.unwrap();
    assert_eq!(listed.len(), 3);
    assert_eq!(
        listed[0].key, "backups/backup-incr-20200101-000000.tar.zst",
        "oldest first"
    );
    assert_eq!(listed[2].key, format!("backups/{}", current_name));

    let downloaded = backup::download(&remote, &current_name, &fx.path("dl"))
        .await
        .unwrap();
    assert_eq!(fs::read(&downloaded).unwrap(), fs::read(&current).unwrap());
    assert!(
        backup::download(&remote, "../../etc/passwd", &fx.path("dl"))
            .await
            .is_err()
    );

    // Dry run deletes nothing.
    backup::clean(Some(&remote), &fx.config, true)
        .await
        .unwrap();
    assert_eq!(backup::local_backups(&backup_dir).unwrap().len(), 3);

    // Old local backups go (by name timestamp); unrelated files stay.
    backup::clean(Some(&remote), &fx.config, false)
        .await
        .unwrap();
    let local = backup::local_backups(&backup_dir).unwrap();
    assert_eq!(local, vec![current.clone()]);
    assert!(backup_dir.join("unrelated.txt").exists());
    // Remote files were all just written, so they are within the retention period.
    assert_eq!(backup::remote_backups(&remote).await.unwrap().len(), 3);
}

#[tokio::test]
async fn clean_always_keeps_newest_backup() {
    let fx = Fixture::new();
    let backup_dir = PathBuf::from(&fx.config.backup.local_backup_dir);
    fs::create_dir_all(&backup_dir).unwrap();
    for name in [
        "backup-incr-20200101-000000.tar.zst",
        "backup-incr-20200102-000000.tar.zst",
    ] {
        fs::write(backup_dir.join(name), name).unwrap();
    }
    backup::clean(None, &fx.config, false).await.unwrap();
    let left = backup::local_backups(&backup_dir).unwrap();
    assert_eq!(left.len(), 1);
    assert!(left[0].ends_with("backup-incr-20200102-000000.tar.zst"));
}

fn config_with_encryption(fx: &Fixture, section: &str) -> AppConfig {
    let project = fx.path("myapp");
    AppConfig::parse(&format!(
        r#"
        [storage]
        provider = "s3"
        bucket = "unused"
        [backup]
        local_backup_dir = "{backups}"
        project_path = "{project}"
        {section}
        "#,
        backups = fx.path("enc-backups").display(),
        project = project.display(),
    ))
    .unwrap()
}

#[test]
fn encrypted_backup_with_public_key() {
    use age::secrecy::ExposeSecret;
    use zesty_backup::encryption::DecryptionKeys;

    let fx = Fixture::new();
    let identity = age::x25519::Identity::generate();
    let key_file = fx.path("key.txt");
    fs::write(&key_file, identity.to_string().expose_secret()).unwrap();
    let config = config_with_encryption(
        &fx,
        &format!("[encryption]\nrecipients = [\"{}\"]", identity.to_public()),
    );

    let archive_path = backup::create_backup(&config, false).unwrap();
    let name = archive_path
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(name.ends_with(".tar.zst.age"), "{}", name);
    assert!(backup::is_backup_file_name(&name));
    assert_eq!(
        backup::local_backups(&fx.path("enc-backups"))
            .unwrap()
            .len(),
        1
    );

    // The file is an age file, not readable as zstd.
    let raw = fs::read(&archive_path).unwrap();
    assert!(raw.starts_with(b"age-encryption.org/v1"));
    assert!(!raw.windows(12).any(|w| w == b"fn main() {}"));

    // No key: clear error.
    let err = archive::verify(&archive_path, &DecryptionKeys::default()).unwrap_err();
    assert!(format!("{:#}", err).contains("--identity"), "{:#}", err);

    // Wrong key: fails.
    let other = fx.path("other.txt");
    fs::write(
        &other,
        age::x25519::Identity::generate()
            .to_string()
            .expose_secret(),
    )
    .unwrap();
    let wrong = DecryptionKeys::new(Some(&other), None);
    assert!(archive::verify(&archive_path, &wrong).is_err());

    // Right key: works.
    let keys = DecryptionKeys::new(Some(&key_file), None);
    assert!(archive::verify(&archive_path, &keys).unwrap() > 0);
    let out = fx.path("dec");
    archive::extract(&archive_path, &out, &keys).unwrap();
    assert_eq!(
        fs::read_to_string(out.join("project/myapp/src/main.rs")).unwrap(),
        "fn main() {}"
    );
}

#[test]
fn encrypted_backup_with_passphrase() {
    let fx = Fixture::new();
    let config = config_with_encryption(&fx, "[encryption]\npassphrase = \"correct horse\"");
    let archive_path = backup::create_backup(&config, false).unwrap();

    let keys = zesty_backup::encryption::DecryptionKeys::new(None, config.encryption.as_ref());
    let out = fx.path("dec");
    archive::extract(&archive_path, &out, &keys).unwrap();
    assert!(out.join("project/myapp/src/main.rs").exists());

    let wrong_config = config_with_encryption(&fx, "[encryption]\npassphrase = \"wrong\"");
    let wrong =
        zesty_backup::encryption::DecryptionKeys::new(None, wrong_config.encryption.as_ref());
    assert!(archive::verify(&archive_path, &wrong).is_err());
}

#[test]
fn encryption_config_errors() {
    let fx = Fixture::new();
    let no_keys = config_with_encryption(&fx, "[encryption]\nenabled = true");
    assert!(backup::create_backup(&no_keys, false).is_err());
    let bad_key = config_with_encryption(&fx, "[encryption]\nrecipients = [\"not-a-key\"]");
    let err = backup::create_backup(&bad_key, false).unwrap_err();
    assert!(err.to_string().contains("age1"), "{}", err);
    let disabled =
        config_with_encryption(&fx, "[encryption]\nenabled = false\nrecipients = [\"x\"]");
    let path = backup::create_backup(&disabled, false).unwrap();
    assert!(path.to_string_lossy().ends_with(".tar.zst"));
}

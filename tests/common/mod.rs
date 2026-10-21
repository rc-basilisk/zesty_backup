//! Shared test helpers: a local folder standing in for cloud storage, and a sample project.
#![allow(dead_code)]

use anyhow::Result;
use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use tempfile::TempDir;
use zesty_backup::config::AppConfig;
use zesty_backup::providers::{BackupItem, StorageProvider};

/// A StorageProvider that stores objects as files in a directory.
pub struct DirProvider(pub PathBuf);

#[async_trait]
impl StorageProvider for DirProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let dest = self.0.join(key);
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::copy(file_path, dest)?;
        Ok(())
    }
    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        fs::copy(self.0.join(key), output_path)?;
        Ok(())
    }
    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let dir = self.0.join(prefix);
        let mut items = Vec::new();
        if dir.exists() {
            for entry in fs::read_dir(dir)? {
                let entry = entry?;
                let meta = entry.metadata()?;
                items.push(BackupItem {
                    key: format!("{}{}", prefix, entry.file_name().to_string_lossy()),
                    size: meta.len(),
                    last_modified: Some(meta.modified()?.into()),
                });
            }
        }
        Ok(items)
    }
    async fn delete(&self, key: &str) -> Result<()> {
        fs::remove_file(self.0.join(key))?;
        Ok(())
    }
}

pub struct Fixture {
    pub dir: TempDir,
    pub config: AppConfig,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("myapp");
        fs::create_dir_all(project.join("src")).unwrap();
        fs::create_dir_all(project.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(project.join("empty")).unwrap();
        fs::write(project.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(project.join("node_modules/pkg/index.js"), "x").unwrap();
        fs::write(project.join("debug.log"), "noise").unwrap();
        // A file big enough to span several zstd blocks.
        fs::write(
            project.join("data.bin"),
            (0..2_000_000u32)
                .map(|i| (i % 251) as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(project.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
            fs::set_permissions(project.join("run.sh"), fs::Permissions::from_mode(0o750)).unwrap();
            std::os::unix::fs::symlink("src/main.rs", project.join("link.rs")).unwrap();
        }
        let extra = dir.path().join("nginx.conf");
        fs::write(&extra, "events {}").unwrap();

        let config = AppConfig::parse(&format!(
            r#"
            [storage]
            provider = "s3"
            bucket = "unused"
            [backup]
            # inside the project on purpose: must not be backed up into itself
            local_backup_dir = "{project}/backups"
            project_path = "{project}"
            additional_paths = ["{extra}", "/does/not/exist"]
            retention_days = 7
            exclude = ["node_modules", "*.log"]
            [system]
            command_outputs = [{{ command = "echo", args = ["hello"], output_file = "echo.txt" }}]
            "#,
            project = project.display(),
            extra = extra.display(),
        ))
        .unwrap();
        Self { dir, config }
    }

    pub fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }
}

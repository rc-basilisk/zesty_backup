//! Google Cloud Storage and Azure Blob Storage, through the `object_store` crate.

use super::{non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use object_store::path::Path as StorePath;
use object_store::{ObjectStore, WriteMultipart};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const CHUNK_SIZE: usize = 16 * 1024 * 1024;

pub struct ObjectStoreProvider {
    store: Arc<dyn ObjectStore>,
    name: &'static str,
}

impl ObjectStoreProvider {
    /// Credentials, in order: `credentials_path` (service account or `gcloud auth
    /// application-default login` JSON), the `GOOGLE_*` env vars (incl.
    /// `GOOGLE_APPLICATION_CREDENTIALS`), gcloud's default credentials file, then the GCE
    /// metadata server.
    pub fn gcs(config: &StorageConfig) -> Result<Self> {
        use object_store::gcp::GoogleCloudStorageBuilder;
        if config.bucket.trim().is_empty() {
            bail!("storage.bucket is required for GCS");
        }
        let mut builder = GoogleCloudStorageBuilder::from_env().with_bucket_name(&config.bucket);
        if let Some(path) = non_empty(&config.credentials_path) {
            anyhow::ensure!(
                Path::new(path).is_file(),
                "credentials_path does not exist: {}",
                path
            );
            builder = builder.with_application_credentials(path);
        }
        let store = builder
            .build()
            .map_err(flatten)
            .context("Failed to set up Google Cloud Storage client (check credentials_path / GOOGLE_APPLICATION_CREDENTIALS)")?;
        Ok(Self {
            store: Arc::new(store),
            name: "GCS",
        })
    }

    /// Credentials: `account_name` + `account_key`, or any of the `AZURE_STORAGE_*` env vars
    /// that `object_store` understands (account key, SAS token, client secret, managed identity...).
    pub fn azure(config: &StorageConfig) -> Result<Self> {
        use object_store::azure::MicrosoftAzureBuilder;
        if config.bucket.trim().is_empty() {
            bail!("storage.bucket (the container name) is required for Azure");
        }
        let mut builder = MicrosoftAzureBuilder::from_env().with_container_name(&config.bucket);
        if let Some(account) = non_empty(&config.account_name) {
            builder = builder.with_account(account);
        }
        if let Some(key) = non_empty(&config.account_key) {
            builder = builder.with_access_key(key);
        }
        let store = builder
            .build()
            .map_err(flatten)
            .context("Failed to set up Azure client (check account_name and account_key / AZURE_STORAGE_ACCOUNT_KEY)")?;
        Ok(Self {
            store: Arc::new(store),
            name: "Azure",
        })
    }
}

#[async_trait]
impl StorageProvider for ObjectStoreProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let mut file = tokio::fs::File::open(file_path)
            .await
            .with_context(|| format!("Failed to open {}", file_path.display()))?;
        let upload = self
            .store
            .put_multipart(&StorePath::from(key))
            .await
            .map_err(flatten)
            .with_context(|| format!("Failed to start {} upload", self.name))?;
        let mut writer = WriteMultipart::new_with_chunk_size(upload, CHUNK_SIZE);
        let mut buf = vec![0u8; 1024 * 1024];
        let result = async {
            loop {
                let n = file.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                writer.wait_for_capacity(4).await.map_err(flatten)?;
                writer.write(&buf[..n]);
            }
            anyhow::Ok(())
        }
        .await;
        match result {
            Ok(()) => {
                writer
                    .finish()
                    .await
                    .map_err(flatten)
                    .with_context(|| format!("Failed to upload {} to {}", key, self.name))?;
                Ok(())
            }
            Err(e) => {
                let _ = writer.abort().await;
                Err(e).with_context(|| format!("Failed to upload {} to {}", key, self.name))
            }
        }
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let result = self
            .store
            .get(&StorePath::from(key))
            .await
            .map_err(flatten)
            .with_context(|| format!("Failed to download {} from {}", key, self.name))?;
        let mut file = tokio::fs::File::create(output_path)
            .await
            .with_context(|| format!("Failed to create {}", output_path.display()))?;
        let mut stream = result.into_stream();
        while let Some(chunk) = stream.next().await {
            file.write_all(&chunk.map_err(flatten)?).await?;
        }
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        // object_store list prefixes are whole path segments, so list the parent
        // "directory" and filter on the full prefix ourselves.
        let dir = prefix.rfind('/').map(|i| &prefix[..i]).unwrap_or("");
        let dir = (!dir.is_empty()).then(|| StorePath::from(dir));
        let mut stream = self.store.list(dir.as_ref());
        let mut items = Vec::new();
        while let Some(meta) = stream.next().await {
            let meta = meta
                .map_err(flatten)
                .with_context(|| format!("Failed to list {} objects", self.name))?;
            let key = meta.location.to_string();
            if key.starts_with(prefix) {
                items.push(BackupItem {
                    key,
                    size: meta.size,
                    last_modified: Some(meta.last_modified),
                });
            }
        }
        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.store
            .delete(&StorePath::from(key))
            .await
            .map_err(flatten)
            .with_context(|| format!("Failed to delete {} from {}", key, self.name))?;
        Ok(())
    }
}

/// object_store errors already include their causes in their message; keep only that
/// so `{:#}` does not print the same text several times.
fn flatten(err: object_store::Error) -> anyhow::Error {
    anyhow::anyhow!("{}", err)
}

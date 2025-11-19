use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::{primitives::ByteStream, Client as S3Client, Config};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use std::path::Path;
use tracing::{info, warn};

#[async_trait]
pub trait StorageProvider: Send + Sync {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()>;
    async fn download(&self, key: &str, output_path: &Path) -> Result<()>;
    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>>;
    async fn delete(&self, key: &str) -> Result<()>;
    #[allow(dead_code)]
    fn get_bucket(&self) -> &str;
}

#[derive(Debug, Clone)]
pub struct BackupItem {
    pub key: String,
    pub size: u64,
    pub last_modified: Option<DateTime<Utc>>,
}

// S3-compatible provider (AWS S3, Contabo, DigitalOcean Spaces, Wasabi, etc.)
pub struct S3Provider {
    client: S3Client,
    bucket: String,
}

impl S3Provider {
    pub async fn new(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key: &str,
        secret_key: &str,
    ) -> Result<Self> {
        use aws_credential_types::Credentials;
        let credentials = Credentials::new(access_key, secret_key, None, None, "zesty-backup");

        let s3_config = Config::builder()
            .endpoint_url(endpoint)
            .region(aws_sdk_s3::config::Region::new(region.to_string()))
            .credentials_provider(credentials)
            .build();

        let client = S3Client::from_conf(s3_config);

        Ok(Self {
            client,
            bucket: bucket.to_string(),
        })
    }
}

#[async_trait]
impl StorageProvider for S3Provider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        info!("Uploading {} to S3...", key);
        let body = ByteStream::from_path(file_path)
            .await
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .with_context(|| format!("Failed to upload to S3: {}", key))?;

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        info!("Downloading {} from S3...", key);
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("Failed to download from S3")?;

        use std::fs::File;
        use std::io::Write;
        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;

        let mut stream = response.body;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("Failed to read S3 stream")?;
            file.write_all(&chunk).context("Failed to write to file")?;
        }

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let mut items = Vec::new();
        let mut continuation_token: Option<String> = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = continuation_token.take() {
                request = request.continuation_token(token.as_str());
            }

            let response = request.send().await.context("Failed to list S3 objects")?;

            for obj in response.contents() {
                if let Some(key) = obj.key() {
                    let item = BackupItem {
                        key: key.to_string(),
                        size: obj.size().unwrap_or(0) as u64,
                        last_modified: obj.last_modified().map(|dt| {
                            // Convert AWS DateTime to chrono DateTime
                            let secs = dt.secs();
                            DateTime::from_timestamp(secs, 0).unwrap_or_else(Utc::now)
                        }),
                    };
                    items.push(item);
                }
            }

            continuation_token = response.next_continuation_token().map(|s| s.to_string());
            if response.is_truncated() != Some(true) {
                break;
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.client
            .delete_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .context("Failed to delete S3 object")?;
        info!("Deleted from S3: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        &self.bucket
    }
}

// Google Cloud Storage provider using object_store crate
// Documentation: https://docs.cloud.google.com/storage/docs/apis
pub struct GCSProvider {
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    #[allow(dead_code)]
    bucket: String,
}

impl GCSProvider {
    pub async fn new(bucket: &str, credentials_path: Option<&str>) -> Result<Self> {
        use object_store::gcp::GoogleCloudStorageBuilder;

        // Set credentials path if provided
        if let Some(cred_path) = credentials_path {
            std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", cred_path);
        }

        // Build GCS client
        let builder = GoogleCloudStorageBuilder::new().with_bucket_name(bucket);

        let store = builder
            .build()
            .context("Failed to build GCS client. Ensure GOOGLE_APPLICATION_CREDENTIALS is set or credentials_path is provided.")?;

        Ok(Self {
            store: std::sync::Arc::new(store),
            bucket: bucket.to_string(),
        })
    }
}

#[async_trait]
impl StorageProvider for GCSProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;
        use std::fs;

        info!("Uploading {} to GCS...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let path = ObjectStorePath::from(key);
        self.store
            .put(&path, data.into())
            .await
            .with_context(|| format!("Failed to upload to GCS: {}", key))?;

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from GCS...", key);
        let path = ObjectStorePath::from(key);
        let data = self
            .store
            .get(&path)
            .await
            .context("Failed to download from GCS")?
            .bytes()
            .await
            .context("Failed to read GCS object data")?;

        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        file.write_all(&data).context("Failed to write to file")?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        use object_store::path::Path as ObjectStorePath;

        let prefix_path = if prefix.is_empty() {
            None
        } else {
            Some(ObjectStorePath::from(prefix))
        };

        let mut stream = self.store.list(prefix_path.as_ref());
        let mut items = Vec::new();

        while let Some(meta) = stream.next().await {
            let meta = meta.context("Failed to list GCS objects")?;
            items.push(BackupItem {
                key: meta.location.to_string(),
                size: meta.size,
                last_modified: Some(meta.last_modified),
            });
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;

        let path = ObjectStorePath::from(key);
        self.store
            .delete(&path)
            .await
            .context("Failed to delete GCS object")?;

        info!("Deleted from GCS: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        &self.bucket
    }
}

// Azure Blob Storage provider using object_store crate
// Documentation: https://docs.azure.cn/en-us/storage/common/storage-introduction
pub struct AzureProvider {
    store: std::sync::Arc<dyn object_store::ObjectStore>,
    #[allow(dead_code)]
    container: String,
}

impl AzureProvider {
    pub async fn new(
        account_name: &str,
        account_key: Option<&str>,
        container: &str,
    ) -> Result<Self> {
        use object_store::azure::MicrosoftAzureBuilder;

        // Build Azure client
        let mut builder = MicrosoftAzureBuilder::new()
            .with_account(account_name)
            .with_container_name(container);

        // Set account key if provided, otherwise try environment variable
        let access_key = if let Some(key) = account_key {
            key.to_string()
        } else if let Ok(env_key) = std::env::var("AZURE_STORAGE_ACCOUNT_KEY") {
            env_key
        } else {
            return Err(anyhow::anyhow!(
                "Azure account_key required. Set it in config (as account_key) or use AZURE_STORAGE_ACCOUNT_KEY env var. \
                For managed identity or SAS tokens, additional implementation may be required."
            ));
        };

        builder = builder.with_access_key(&access_key);

        let store = builder.build().context(
            "Failed to build Azure client. Ensure account_name and account_key are correct.",
        )?;

        Ok(Self {
            store: std::sync::Arc::new(store),
            container: container.to_string(),
        })
    }
}

#[async_trait]
impl StorageProvider for AzureProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;
        use std::fs;

        info!("Uploading {} to Azure...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let path = ObjectStorePath::from(key);
        self.store
            .put(&path, data.into())
            .await
            .with_context(|| format!("Failed to upload to Azure: {}", key))?;

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from Azure...", key);
        let path = ObjectStorePath::from(key);
        let data = self
            .store
            .get(&path)
            .await
            .context("Failed to download from Azure")?
            .bytes()
            .await
            .context("Failed to read Azure blob data")?;

        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        file.write_all(&data).context("Failed to write to file")?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        use object_store::path::Path as ObjectStorePath;

        let prefix_path = if prefix.is_empty() {
            None
        } else {
            Some(ObjectStorePath::from(prefix))
        };

        let mut stream = self.store.list(prefix_path.as_ref());
        let mut items = Vec::new();

        while let Some(meta) = stream.next().await {
            let meta = meta.context("Failed to list Azure blobs")?;
            items.push(BackupItem {
                key: meta.location.to_string(),
                size: meta.size,
                last_modified: Some(meta.last_modified),
            });
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        use object_store::path::Path as ObjectStorePath;

        let path = ObjectStorePath::from(key);
        self.store
            .delete(&path)
            .await
            .context("Failed to delete Azure blob")?;

        info!("Deleted from Azure: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        &self.container
    }
}

// Backblaze B2 provider
pub struct B2Provider {
    account_id: String,
    application_key: String,
    bucket_id: String,
    bucket_name: String,
    api_url: String,
    download_url: String,
    auth_token: Option<String>,
}

impl B2Provider {
    pub async fn new(
        account_id: &str,
        application_key: &str,
        bucket_id: &str,
        bucket_name: &str,
    ) -> Result<Self> {
        let mut provider = Self {
            account_id: account_id.to_string(),
            application_key: application_key.to_string(),
            bucket_id: bucket_id.to_string(),
            bucket_name: bucket_name.to_string(),
            api_url: String::new(),
            download_url: String::new(),
            auth_token: None,
        };

        provider.authenticate().await?;
        Ok(provider)
    }

    async fn authenticate(&mut self) -> Result<()> {
        use base64::Engine;
        let credentials = format!("{}:{}", self.account_id, self.application_key);
        let encoded = base64::engine::general_purpose::STANDARD.encode(credentials);

        let client = reqwest::Client::new();
        let response = client
            .get("https://api.backblazeb2.com/b2api/v2/b2_authorize_account")
            .header("Authorization", format!("Basic {}", encoded))
            .send()
            .await
            .context("Failed to authenticate with B2")?;

        let json: serde_json::Value = response.json().await?;
        self.api_url = json["apiUrl"]
            .as_str()
            .context("Missing apiUrl in B2 response")?
            .to_string();
        self.download_url = json["downloadUrl"]
            .as_str()
            .context("Missing downloadUrl in B2 response")?
            .to_string();
        self.auth_token = Some(
            json["authorizationToken"]
                .as_str()
                .context("Missing authorizationToken in B2 response")?
                .to_string(),
        );

        Ok(())
    }

    async fn get_upload_url(&self) -> Result<(String, String)> {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/b2api/v2/b2_get_upload_url", self.api_url))
            .header("Authorization", self.auth_token.as_ref().unwrap())
            .json(&serde_json::json!({
                "bucketId": self.bucket_id
            }))
            .send()
            .await
            .context("Failed to get B2 upload URL")?;

        let json: serde_json::Value = response.json().await?;
        let upload_url = json["uploadUrl"]
            .as_str()
            .context("Missing uploadUrl")?
            .to_string();
        let upload_auth_token = json["authorizationToken"]
            .as_str()
            .context("Missing authorizationToken")?
            .to_string();

        Ok((upload_url, upload_auth_token))
    }
}

#[async_trait]
impl StorageProvider for B2Provider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use base64::Engine;
        use sha1::{Digest, Sha1};
        use std::fs;

        info!("Uploading {} to B2...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let (upload_url, upload_auth_token) = self.get_upload_url().await?;

        // Calculate SHA1
        let mut hasher = Sha1::new();
        hasher.update(&data);
        let sha1 = hasher.finalize();
        let sha1_hex = format!("{:x}", sha1);

        let client = reqwest::Client::new();
        let response = client
            .post(&upload_url)
            .header("Authorization", upload_auth_token)
            .header(
                "X-Bz-File-Name",
                base64::engine::general_purpose::STANDARD.encode(key),
            )
            .header("Content-Type", "b2/x-auto")
            .header("X-Bz-Content-Sha1", sha1_hex)
            .header("X-Bz-Info-Author", "zesty-backup")
            .body(data)
            .send()
            .await
            .context("Failed to upload to B2")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("B2 upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from B2...", key);
        let url = format!("{}/file/{}/{}", self.download_url, self.bucket_name, key);

        let client = reqwest::Client::new();
        let response = client
            .get(&url)
            .header("Authorization", self.auth_token.as_ref().unwrap())
            .send()
            .await
            .context("Failed to download from B2")?;

        let data = response
            .bytes()
            .await
            .context("Failed to read B2 response")?;

        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        file.write_all(&data).context("Failed to write to file")?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let client = reqwest::Client::new();
        let mut items = Vec::new();
        let mut start_file_name: Option<String> = None;

        loop {
            let mut json = serde_json::json!({
                "bucketId": self.bucket_id,
                "maxFileCount": 1000,
            });

            if let Some(prefix) = prefix.strip_suffix('/') {
                json["prefix"] = serde_json::Value::String(prefix.to_string());
            } else if !prefix.is_empty() {
                json["prefix"] = serde_json::Value::String(prefix.to_string());
            }

            if let Some(start) = start_file_name.take() {
                json["startFileName"] = serde_json::Value::String(start);
            }

            let response = client
                .post(format!("{}/b2api/v2/b2_list_file_names", self.api_url))
                .header("Authorization", self.auth_token.as_ref().unwrap())
                .json(&json)
                .send()
                .await
                .context("Failed to list B2 files")?;

            let json: serde_json::Value = response.json().await?;
            let files = json["files"]
                .as_array()
                .context("Missing files array in B2 response")?;

            if files.is_empty() {
                break;
            }

            for file in files {
                let file_name = file["fileName"]
                    .as_str()
                    .context("Missing fileName")?
                    .to_string();
                let size = file["contentLength"].as_u64().unwrap_or(0);
                let timestamp_ms = file["uploadTimestamp"].as_u64().unwrap_or(0) / 1000;

                items.push(BackupItem {
                    key: file_name,
                    size,
                    last_modified: DateTime::from_timestamp(timestamp_ms as i64, 0),
                });
            }

            if json["nextFileName"].is_null() {
                break;
            }
            start_file_name = json["nextFileName"].as_str().map(|s| s.to_string());
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // First get file info
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{}/b2api/v2/b2_list_file_versions", self.api_url))
            .header("Authorization", self.auth_token.as_ref().unwrap())
            .json(&serde_json::json!({
                "bucketId": self.bucket_id,
                "startFileName": key,
                "maxFileCount": 1,
            }))
            .send()
            .await
            .context("Failed to get file info from B2")?;

        let json: serde_json::Value = response.json().await?;
        let files = json["files"].as_array().context("Missing files array")?;

        if let Some(file) = files.first() {
            let file_id = file["fileId"].as_str().context("Missing fileId")?;
            let file_name = file["fileName"].as_str().context("Missing fileName")?;

            let delete_response = client
                .post(format!("{}/b2api/v2/b2_delete_file_version", self.api_url))
                .header("Authorization", self.auth_token.as_ref().unwrap())
                .json(&serde_json::json!({
                    "fileId": file_id,
                    "fileName": file_name,
                }))
                .send()
                .await
                .context("Failed to delete from B2")?;

            if !delete_response.status().is_success() {
                let error = delete_response.text().await.unwrap_or_default();
                return Err(anyhow::anyhow!("B2 delete failed: {}", error));
            }

            info!("Deleted from B2: {}", key);
        }

        Ok(())
    }

    fn get_bucket(&self) -> &str {
        &self.bucket_name
    }
}

// Google Drive provider
pub struct GoogleDriveProvider {
    access_token: String,
    folder_id: Option<String>,
}

impl GoogleDriveProvider {
    pub async fn new(access_token: &str, folder_id: Option<&str>) -> Result<Self> {
        Ok(Self {
            access_token: access_token.to_string(),
            folder_id: folder_id.map(|s| s.to_string()),
        })
    }

    async fn get_folder_id(&self) -> Result<String> {
        if let Some(ref folder_id) = self.folder_id {
            return Ok(folder_id.clone());
        }
        // Default to root folder
        Ok("root".to_string())
    }
}

#[async_trait]
impl StorageProvider for GoogleDriveProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::fs;

        info!("Uploading {} to Google Drive...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        // Create file metadata
        let metadata = serde_json::json!({
            "name": file_name,
            "parents": [folder_id]
        });

        // Upload file using multipart upload
        let client = reqwest::Client::new();
        let form = reqwest::multipart::Form::new()
            .text("metadata", serde_json::to_string(&metadata)?)
            .part(
                "file",
                reqwest::multipart::Part::bytes(data).file_name(file_name.to_string()),
            );

        let response = client
            .post("https://www.googleapis.com/upload/drive/v3/files?uploadType=multipart")
            .bearer_auth(&self.access_token)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload to Google Drive")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Google Drive upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from Google Drive...", key);

        // First, find the file by name
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        let query = format!(
            "name='{}' and '{}' in parents and trashed=false",
            file_name.replace("'", "\\'"),
            folder_id
        );
        let url = format!(
            "https://www.googleapis.com/drive/v3/files?q={}",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );

        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to search Google Drive")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["files"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|f| f["id"].as_str())
            .context("File not found in Google Drive")?;

        // Download the file
        let download_url = format!(
            "https://www.googleapis.com/drive/v3/files/{}?alt=media",
            file_id
        );
        let file_response = client
            .get(&download_url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to download from Google Drive")?;

        let data = file_response.bytes().await?;
        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        file.write_all(&data)?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let folder_id = self.get_folder_id().await?;
        let client = reqwest::Client::new();
        let query = format!("'{}' in parents and trashed=false", folder_id);
        let url = format!("https://www.googleapis.com/drive/v3/files?q={}&fields=files(id,name,size,modifiedTime)", 
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>());

        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list Google Drive files")?;

        let files: serde_json::Value = response.json().await?;
        let mut items = Vec::new();

        if let Some(files_array) = files["files"].as_array() {
            for file in files_array {
                if let Some(name) = file["name"].as_str() {
                    if name.starts_with(prefix) {
                        items.push(BackupItem {
                            key: name.to_string(),
                            size: file["size"]
                                .as_str()
                                .and_then(|s| s.parse::<u64>().ok())
                                .unwrap_or(0),
                            last_modified: file["modifiedTime"]
                                .as_str()
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                .map(|dt| dt.with_timezone(&Utc)),
                        });
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // Find and delete file
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        let query = format!(
            "name='{}' and '{}' in parents and trashed=false",
            file_name.replace("'", "\\'"),
            folder_id
        );
        let url = format!(
            "https://www.googleapis.com/drive/v3/files?q={}",
            url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>()
        );

        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to search Google Drive")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["files"]
            .as_array()
            .and_then(|arr| arr.first())
            .and_then(|f| f["id"].as_str())
            .context("File not found in Google Drive")?;

        client
            .delete(format!(
                "https://www.googleapis.com/drive/v3/files/{}",
                file_id
            ))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to delete from Google Drive")?;

        info!("Deleted from Google Drive: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "Google Drive"
    }
}

// OneDrive provider
pub struct OneDriveProvider {
    access_token: String,
    folder_path: Option<String>,
}

impl OneDriveProvider {
    pub async fn new(access_token: &str, folder_path: Option<&str>) -> Result<Self> {
        Ok(Self {
            access_token: access_token.to_string(),
            folder_path: folder_path.map(|s| s.to_string()),
        })
    }

    async fn get_folder_id(&self) -> Result<String> {
        let client = reqwest::Client::new();
        let path = self.folder_path.as_deref().unwrap_or("/drive/root:");

        let url = format!("https://graph.microsoft.com/v1.0/me{}", path);
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to get OneDrive folder")?;

        let folder: serde_json::Value = response.json().await?;
        folder["id"]
            .as_str()
            .map(|s| s.to_string())
            .context("Failed to get folder ID")
    }
}

#[async_trait]
impl StorageProvider for OneDriveProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::fs;

        info!("Uploading {} to OneDrive...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/children/{}:/content",
            folder_id, file_name
        );
        let client = reqwest::Client::new();
        let response = client
            .put(&url)
            .bearer_auth(&self.access_token)
            .body(data)
            .send()
            .await
            .context("Failed to upload to OneDrive")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("OneDrive upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from OneDrive...", key);
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/children",
            folder_id
        );
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list OneDrive files")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["value"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["name"].as_str() == Some(file_name)))
            .and_then(|f| f["id"].as_str())
            .context("File not found in OneDrive")?;

        let download_url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/content",
            file_id
        );
        let file_response = client
            .get(&download_url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to download from OneDrive")?;

        let data = file_response.bytes().await?;
        let mut file = File::create(output_path)?;
        file.write_all(&data)?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let folder_id = self.get_folder_id().await?;
        let client = reqwest::Client::new();
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/children",
            folder_id
        );
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list OneDrive files")?;

        let files: serde_json::Value = response.json().await?;
        let mut items = Vec::new();

        if let Some(files_array) = files["value"].as_array() {
            for file in files_array {
                if let Some(name) = file["name"].as_str() {
                    if name.starts_with(prefix) {
                        items.push(BackupItem {
                            key: name.to_string(),
                            size: file["size"].as_u64().unwrap_or(0),
                            last_modified: file["lastModifiedDateTime"]
                                .as_str()
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                .map(|dt| dt.with_timezone(&Utc)),
                        });
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        let url = format!(
            "https://graph.microsoft.com/v1.0/me/drive/items/{}/children",
            folder_id
        );
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list OneDrive files")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["value"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["name"].as_str() == Some(file_name)))
            .and_then(|f| f["id"].as_str())
            .context("File not found in OneDrive")?;

        client
            .delete(format!(
                "https://graph.microsoft.com/v1.0/me/drive/items/{}",
                file_id
            ))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to delete from OneDrive")?;

        info!("Deleted from OneDrive: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "OneDrive"
    }
}

// Dropbox provider
pub struct DropboxProvider {
    access_token: String,
    folder_path: Option<String>,
}

impl DropboxProvider {
    pub async fn new(access_token: &str, folder_path: Option<&str>) -> Result<Self> {
        Ok(Self {
            access_token: access_token.to_string(),
            folder_path: folder_path.map(|s| s.to_string()),
        })
    }

    fn get_path(&self, key: &str) -> String {
        let base = self.folder_path.as_deref().unwrap_or("");
        if base.is_empty() {
            format!("/{}", key)
        } else {
            format!("{}/{}", base, key)
        }
    }
}

#[async_trait]
impl StorageProvider for DropboxProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::fs;

        info!("Uploading {} to Dropbox...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let path = self.get_path(key);
        let client = reqwest::Client::new();
        let response = client
            .post("https://content.dropboxapi.com/2/files/upload")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header(
                "Dropbox-API-Arg",
                serde_json::json!({
                    "path": path,
                    "mode": "overwrite"
                })
                .to_string(),
            )
            .header("Content-Type", "application/octet-stream")
            .body(data)
            .send()
            .await
            .context("Failed to upload to Dropbox")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Dropbox upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from Dropbox...", key);
        let path = self.get_path(key);
        let client = reqwest::Client::new();
        let response = client
            .post("https://content.dropboxapi.com/2/files/download")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .header(
                "Dropbox-API-Arg",
                serde_json::json!({ "path": path }).to_string(),
            )
            .send()
            .await
            .context("Failed to download from Dropbox")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Dropbox download failed: {}", error));
        }

        let data = response.bytes().await?;
        let mut file = File::create(output_path)?;
        file.write_all(&data)?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let base_path = self.folder_path.as_deref().unwrap_or("");
        let client = reqwest::Client::new();
        let response = client
            .post("https://api.dropboxapi.com/2/files/list_folder")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .json(&serde_json::json!({
                "path": base_path,
                "recursive": false
            }))
            .send()
            .await
            .context("Failed to list Dropbox files")?;

        let files: serde_json::Value = response.json().await?;
        let mut items = Vec::new();

        if let Some(entries) = files["entries"].as_array() {
            for entry in entries {
                if let Some(name) = entry["name"].as_str() {
                    if name.starts_with(prefix) && entry[".tag"].as_str() == Some("file") {
                        items.push(BackupItem {
                            key: name.to_string(),
                            size: entry["size"].as_u64().unwrap_or(0),
                            last_modified: entry["client_modified"]
                                .as_str()
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                .map(|dt| dt.with_timezone(&Utc)),
                        });
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let path = self.get_path(key);
        let client = reqwest::Client::new();
        let response = client
            .post("https://api.dropboxapi.com/2/files/delete_v2")
            .header("Authorization", format!("Bearer {}", self.access_token))
            .json(&serde_json::json!({ "path": path }))
            .send()
            .await
            .context("Failed to delete from Dropbox")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Dropbox delete failed: {}", error));
        }

        info!("Deleted from Dropbox: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "Dropbox"
    }
}

// Box provider
pub struct BoxProvider {
    access_token: String,
    folder_id: Option<String>,
}

impl BoxProvider {
    pub async fn new(access_token: &str, folder_id: Option<&str>) -> Result<Self> {
        Ok(Self {
            access_token: access_token.to_string(),
            folder_id: folder_id.map(|s| s.to_string()),
        })
    }

    async fn get_folder_id(&self) -> Result<String> {
        if let Some(ref folder_id) = self.folder_id {
            return Ok(folder_id.clone());
        }
        // Default to root folder (0)
        Ok("0".to_string())
    }
}

#[async_trait]
impl StorageProvider for BoxProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::fs;

        info!("Uploading {} to Box...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        // Box uses multipart upload
        let client = reqwest::Client::new();
        let attributes = serde_json::json!({
            "name": file_name,
            "parent": { "id": folder_id }
        });

        let form = reqwest::multipart::Form::new()
            .text("attributes", attributes.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(data).file_name(file_name.to_string()),
            );

        let response = client
            .post("https://upload.box.com/api/2.0/files/content")
            .bearer_auth(&self.access_token)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload to Box")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Box upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from Box...", key);
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        // First, find the file
        let url = format!("https://api.box.com/2.0/folders/{}/items", folder_id);
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list Box files")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["entries"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["name"].as_str() == Some(file_name)))
            .and_then(|f| f["id"].as_str())
            .context("File not found in Box")?;

        // Download the file
        let download_url = format!("https://api.box.com/2.0/files/{}/content", file_id);
        let file_response = client
            .get(&download_url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to download from Box")?;

        let data = file_response.bytes().await?;
        let mut file = File::create(output_path)?;
        file.write_all(&data)?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let folder_id = self.get_folder_id().await?;
        let client = reqwest::Client::new();
        let url = format!("https://api.box.com/2.0/folders/{}/items", folder_id);
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list Box files")?;

        let files: serde_json::Value = response.json().await?;
        let mut items = Vec::new();

        if let Some(entries) = files["entries"].as_array() {
            for entry in entries {
                if let Some(name) = entry["name"].as_str() {
                    if name.starts_with(prefix) && entry["type"].as_str() == Some("file") {
                        items.push(BackupItem {
                            key: name.to_string(),
                            size: entry["size"].as_u64().unwrap_or(0),
                            last_modified: entry["modified_at"]
                                .as_str()
                                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                                .map(|dt| dt.with_timezone(&Utc)),
                        });
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let folder_id = self.get_folder_id().await?;
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        let client = reqwest::Client::new();
        let url = format!("https://api.box.com/2.0/folders/{}/items", folder_id);
        let response = client
            .get(&url)
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to list Box files")?;

        let files: serde_json::Value = response.json().await?;
        let file_id = files["entries"]
            .as_array()
            .and_then(|arr| arr.iter().find(|f| f["name"].as_str() == Some(file_name)))
            .and_then(|f| f["id"].as_str())
            .context("File not found in Box")?;

        client
            .delete(format!("https://api.box.com/2.0/files/{}", file_id))
            .bearer_auth(&self.access_token)
            .send()
            .await
            .context("Failed to delete from Box")?;

        info!("Deleted from Box: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "Box"
    }
}

// MEGA provider using MEGAcmd (official MEGA command-line tool)
// Documentation: https://github.com/meganz/MEGAcmd
// MEGA uses client-side encryption, which MEGAcmd handles automatically
pub struct MegaProvider {
    email: String,
    password: String,
    folder_path: Option<String>,
    mega_cmd_path: Option<String>,
}

impl MegaProvider {
    pub async fn new(email: &str, password: &str, folder_path: Option<&str>) -> Result<Self> {
        // Check if MEGAcmd is available
        let mega_cmd_path = which::which("mega-cmd")
            .or_else(|_| which::which("megacmd"))
            .or_else(|_| which::which("mega-cmd-server"))
            .ok()
            .and_then(|p| p.to_str().map(|s| s.to_string()));

        if mega_cmd_path.is_none() {
            warn!("MEGAcmd not found in PATH. Please install MEGAcmd from https://mega.nz/cmd");
            warn!("MEGA provider will attempt to use 'mega-cmd' but may fail if not installed.");
        }

        Ok(Self {
            email: email.to_string(),
            password: password.to_string(),
            folder_path: folder_path.map(|s| s.to_string()),
            mega_cmd_path,
        })
    }

    fn get_mega_cmd(&self) -> &str {
        self.mega_cmd_path.as_deref().unwrap_or("mega-cmd")
    }

    fn get_remote_path(&self, key: &str) -> String {
        let base = self.folder_path.as_deref().unwrap_or("/");
        if base == "/" {
            format!("/{}", key)
        } else {
            format!("{}/{}", base.trim_end_matches('/'), key)
        }
    }

    async fn ensure_logged_in(&self) -> Result<()> {
        use std::process::Command;

        // Check if already logged in by trying to get user info
        let check_cmd = Command::new(self.get_mega_cmd())
            .arg("whoami")
            .output()
            .context("Failed to execute MEGAcmd. Is MEGAcmd installed?")?;

        if check_cmd.status.success() {
            // Already logged in
            return Ok(());
        }

        // Need to login - MEGAcmd requires interactive login or session file
        // We'll use the login command with credentials
        info!("Logging into MEGA...");
        let login_cmd = Command::new(self.get_mega_cmd())
            .arg("login")
            .arg(&self.email)
            .arg(&self.password)
            .output()
            .context("Failed to login to MEGA")?;

        if !login_cmd.status.success() {
            let error = String::from_utf8_lossy(&login_cmd.stderr);
            return Err(anyhow::anyhow!("MEGA login failed: {}", error));
        }

        info!("Successfully logged into MEGA");
        Ok(())
    }
}

#[async_trait]
impl StorageProvider for MegaProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::process::Command;

        info!("Uploading {} to MEGA...", key);

        // Ensure we're logged in
        self.ensure_logged_in().await?;

        // Ensure remote folder exists
        if let Some(ref folder_path) = self.folder_path {
            if folder_path != "/" {
                let mkdir_cmd = Command::new(self.get_mega_cmd())
                    .arg("mkdir")
                    .arg("-p")
                    .arg(folder_path)
                    .output();
                // Ignore errors - folder might already exist
                let _ = mkdir_cmd;
            }
        }

        let remote_path = self.get_remote_path(key);
        let remote_dir = Path::new(&remote_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");

        // Upload file using mega-put
        let upload_cmd = Command::new(self.get_mega_cmd())
            .arg("put")
            .arg(file_path.as_os_str())
            .arg(remote_dir)
            .output()
            .context("Failed to execute MEGAcmd upload")?;

        if !upload_cmd.status.success() {
            let error = String::from_utf8_lossy(&upload_cmd.stderr);
            return Err(anyhow::anyhow!("MEGA upload failed: {}", error));
        }

        // Rename if needed (mega-put uses the original filename)
        let uploaded_path = format!(
            "{}/{}",
            remote_dir,
            file_path.file_name().and_then(|n| n.to_str()).unwrap_or("")
        );

        if uploaded_path != remote_path {
            let rename_cmd = Command::new(self.get_mega_cmd())
                .arg("mv")
                .arg(&uploaded_path)
                .arg(&remote_path)
                .output();

            if let Ok(output) = rename_cmd {
                if !output.status.success() {
                    warn!("Failed to rename uploaded file, but upload succeeded");
                }
            }
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::process::Command;

        info!("Downloading {} from MEGA...", key);

        // Ensure we're logged in
        self.ensure_logged_in().await?;

        let remote_path = self.get_remote_path(key);
        let output_dir = output_path.parent().context("Invalid output path")?;

        // Create output directory if needed
        if let Err(e) = std::fs::create_dir_all(output_dir) {
            if !output_dir.exists() {
                return Err(anyhow::anyhow!("Failed to create output directory: {}", e));
            }
        }

        // Download file using mega-get
        let download_cmd = Command::new(self.get_mega_cmd())
            .arg("get")
            .arg(&remote_path)
            .arg(output_dir.as_os_str())
            .output()
            .context("Failed to execute MEGAcmd download")?;

        if !download_cmd.status.success() {
            let error = String::from_utf8_lossy(&download_cmd.stderr);
            return Err(anyhow::anyhow!("MEGA download failed: {}", error));
        }

        // Rename if needed (mega-get uses the remote filename)
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);
        let downloaded_path = output_dir.join(file_name);

        if downloaded_path != *output_path && downloaded_path.exists() {
            std::fs::rename(&downloaded_path, output_path)
                .context("Failed to rename downloaded file")?;
        }

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        use std::process::Command;

        // Ensure we're logged in
        self.ensure_logged_in().await?;

        let folder_path = self.folder_path.as_deref().unwrap_or("/");

        // List files using mega-ls
        let list_cmd = Command::new(self.get_mega_cmd())
            .arg("ls")
            .arg("-l")
            .arg(folder_path)
            .output()
            .context("Failed to execute MEGAcmd list")?;

        if !list_cmd.status.success() {
            let error = String::from_utf8_lossy(&list_cmd.stderr);
            return Err(anyhow::anyhow!("MEGA list failed: {}", error));
        }

        let output = String::from_utf8_lossy(&list_cmd.stdout);
        let mut items = Vec::new();

        // Parse MEGAcmd ls output (format: permissions size date time name)
        for line in output.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() >= 5 {
                // Check if it's a file (not a directory)
                let perms = parts[0];
                if perms.starts_with('-') {
                    let size_str = parts[4];
                    let name = parts[5..].join(" ");

                    if name.starts_with(prefix) {
                        if let Ok(size) = size_str.parse::<u64>() {
                            items.push(BackupItem {
                                key: name,
                                size,
                                last_modified: None, // MEGAcmd ls doesn't provide timestamps in simple format
                            });
                        }
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        use std::process::Command;

        info!("Deleting {} from MEGA...", key);

        // Ensure we're logged in
        self.ensure_logged_in().await?;

        let remote_path = self.get_remote_path(key);

        // Delete file using mega-rm
        let delete_cmd = Command::new(self.get_mega_cmd())
            .arg("rm")
            .arg(&remote_path)
            .output()
            .context("Failed to execute MEGAcmd delete")?;

        if !delete_cmd.status.success() {
            let error = String::from_utf8_lossy(&delete_cmd.stderr);
            return Err(anyhow::anyhow!("MEGA delete failed: {}", error));
        }

        info!("Deleted from MEGA: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "MEGA"
    }
}

// pCloud provider
// Documentation: https://docs.pcloud.com/
pub struct PCloudProvider {
    access_token: String,
    api_host: String, // api.pcloud.com (US) or eapi.pcloud.com (EU)
    folder_path: Option<String>,
}

impl PCloudProvider {
    pub async fn new(
        access_token: &str,
        region: Option<&str>, // "us" or "eu", defaults to "us"
        folder_path: Option<&str>,
    ) -> Result<Self> {
        let api_host = match region {
            Some("eu") | Some("europe") => "https://eapi.pcloud.com",
            _ => "https://api.pcloud.com",
        };

        Ok(Self {
            access_token: access_token.to_string(),
            api_host: api_host.to_string(),
            folder_path: folder_path.map(|s| s.to_string()),
        })
    }

    fn get_folder_path(&self) -> String {
        self.folder_path.as_deref().unwrap_or("/").to_string()
    }

    fn get_full_path(&self, key: &str) -> String {
        let base = self.get_folder_path();
        if base == "/" {
            format!("/{}", key)
        } else {
            format!("{}/{}", base.trim_end_matches('/'), key)
        }
    }

    async fn get_digest(&self) -> Result<String> {
        // pCloud requires a digest for authentication
        let client = reqwest::Client::new();
        let url = format!("{}/getdigest", self.api_host);
        let response = client
            .get(&url)
            .send()
            .await
            .context("Failed to get pCloud digest")?;

        let json: serde_json::Value = response.json().await?;
        if json["result"].as_i64() != Some(0) {
            let error = json["error"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("pCloud digest error: {}", error));
        }

        json["digest"]
            .as_str()
            .map(|s| s.to_string())
            .context("Missing digest in pCloud response")
    }
}

#[async_trait]
impl StorageProvider for PCloudProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        use std::fs;

        info!("Uploading {} to pCloud...", key);
        let data = fs::read(file_path)
            .with_context(|| format!("Failed to read file: {}", file_path.display()))?;

        let digest = self.get_digest().await?;
        let full_path = self.get_full_path(key);
        let file_name = Path::new(key)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(key);

        // First, ensure the folder exists
        let folder_path = Path::new(&full_path)
            .parent()
            .and_then(|p| p.to_str())
            .unwrap_or("/");

        if folder_path != "/" {
            // Create folder if it doesn't exist (pCloud will ignore if it exists)
            let client = reqwest::Client::new();
            let create_url = format!("{}/createfolder", self.api_host);
            let _ = client
                .get(&create_url)
                .query(&[
                    ("auth", self.access_token.as_str()),
                    ("digest", digest.as_str()),
                    ("path", folder_path),
                ])
                .send()
                .await;
        }

        // Upload file using multipart
        let client = reqwest::Client::new();
        let upload_url = format!("{}/uploadfile", self.api_host);

        let form = reqwest::multipart::Form::new()
            .text("auth", self.access_token.clone())
            .text("digest", digest)
            .text("path", folder_path.to_string())
            .text("filename", file_name.to_string())
            .part(
                "file",
                reqwest::multipart::Part::bytes(data).file_name(file_name.to_string()),
            );

        let response = client
            .post(&upload_url)
            .multipart(form)
            .send()
            .await
            .context("Failed to upload to pCloud")?;

        let json: serde_json::Value = response.json().await?;
        if json["result"].as_i64() != Some(0) {
            let error = json["error"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("pCloud upload failed: {}", error));
        }

        info!("Successfully uploaded: {}", key);
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        use std::fs::File;
        use std::io::Write;

        info!("Downloading {} from pCloud...", key);
        let digest = self.get_digest().await?;
        let full_path = self.get_full_path(key);

        let client = reqwest::Client::new();
        let url = format!("{}/downloadfile", self.api_host);
        let response = client
            .get(&url)
            .query(&[
                ("auth", &self.access_token),
                ("digest", &digest),
                ("path", &full_path),
            ])
            .send()
            .await
            .context("Failed to download from pCloud")?;

        if !response.status().is_success() {
            let error = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("pCloud download failed: {}", error));
        }

        let data = response.bytes().await?;
        let mut file = File::create(output_path)
            .with_context(|| format!("Failed to create output file: {}", output_path.display()))?;
        file.write_all(&data)?;

        info!("Downloaded to: {}", output_path.display());
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let digest = self.get_digest().await?;
        let folder_path = self.get_folder_path();

        let client = reqwest::Client::new();
        let url = format!("{}/listfolder", self.api_host);
        let response = client
            .get(&url)
            .query(&[
                ("auth", &self.access_token),
                ("digest", &digest),
                ("path", &folder_path),
            ])
            .send()
            .await
            .context("Failed to list pCloud files")?;

        let json: serde_json::Value = response.json().await?;
        if json["result"].as_i64() != Some(0) {
            let error = json["error"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("pCloud list failed: {}", error));
        }

        let mut items = Vec::new();
        if let Some(metadata) = json.get("metadata") {
            if let Some(files) = metadata.get("contents").and_then(|c| c.as_array()) {
                for file in files {
                    if let Some(name) = file["name"].as_str() {
                        if name.starts_with(prefix) && file["isfolder"].as_i64() == Some(0) {
                            items.push(BackupItem {
                                key: name.to_string(),
                                size: file["size"].as_u64().unwrap_or(0),
                                last_modified: file["modified"].as_str().and_then(|s| {
                                    // pCloud uses Unix timestamp
                                    s.parse::<i64>()
                                        .ok()
                                        .and_then(|ts| DateTime::from_timestamp(ts, 0))
                                }),
                            });
                        }
                    }
                }
            }
        }

        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let digest = self.get_digest().await?;
        let full_path = self.get_full_path(key);

        let client = reqwest::Client::new();
        let url = format!("{}/deletefile", self.api_host);
        let response = client
            .get(&url)
            .query(&[
                ("auth", &self.access_token),
                ("digest", &digest),
                ("path", &full_path),
            ])
            .send()
            .await
            .context("Failed to delete from pCloud")?;

        let json: serde_json::Value = response.json().await?;
        if json["result"].as_i64() != Some(0) {
            let error = json["error"].as_str().unwrap_or("Unknown error");
            return Err(anyhow::anyhow!("pCloud delete failed: {}", error));
        }

        info!("Deleted from pCloud: {}", key);
        Ok(())
    }

    fn get_bucket(&self) -> &str {
        "pCloud"
    }
}

pub enum Provider {
    S3(S3Provider),
    Gcs(GCSProvider),
    Azure(AzureProvider),
    B2(B2Provider),
    GoogleDrive(GoogleDriveProvider),
    OneDrive(OneDriveProvider),
    Dropbox(DropboxProvider),
    Box(BoxProvider),
    Mega(MegaProvider),
    PCloud(PCloudProvider),
}

impl Provider {
    pub async fn from_config(config: &StorageConfig) -> Result<Self> {
        match config.provider.as_str() {
            "s3" | "aws" | "contabo" | "digitalocean" | "wasabi" | "minio" | "r2" => {
                let endpoint = match config.provider.as_str() {
                    "aws" => format!("https://s3.{}.amazonaws.com", config.region),
                    "digitalocean" => format!("https://{}.digitaloceanspaces.com", config.region),
                    "wasabi" => format!("https://s3.{}.wasabisys.com", config.region),
                    "r2" => format!(
                        "https://{}.r2.cloudflarestorage.com",
                        config.account_id.as_ref().unwrap_or(&"".to_string())
                    ),
                    _ => config.endpoint.clone(),
                };

                let provider = S3Provider::new(
                    &endpoint,
                    &config.region,
                    &config.bucket,
                    &config.access_key,
                    &config.secret_key,
                )
                .await?;
                Ok(Provider::S3(provider))
            }
            "gcs" | "google" => {
                let provider =
                    GCSProvider::new(&config.bucket, config.credentials_path.as_deref()).await?;
                Ok(Provider::Gcs(provider))
            }
            "azure" => {
                let provider = AzureProvider::new(
                    config
                        .account_name
                        .as_ref()
                        .context("Azure account_name required")?,
                    config.account_key.as_deref(),
                    &config.bucket,
                )
                .await?;
                Ok(Provider::Azure(provider))
            }
            "googledrive" | "gdrive" => {
                if config.access_key.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Google Drive access_token required (set as access_key)"
                    ));
                }
                let provider = GoogleDriveProvider::new(
                    &config.access_key,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_id
                )
                .await?;
                Ok(Provider::GoogleDrive(provider))
            }
            "onedrive" => {
                if config.access_key.is_empty() {
                    return Err(anyhow::anyhow!(
                        "OneDrive access_token required (set as access_key)"
                    ));
                }
                let provider = OneDriveProvider::new(
                    &config.access_key,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_path
                )
                .await?;
                Ok(Provider::OneDrive(provider))
            }
            "dropbox" => {
                if config.access_key.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Dropbox access_token required (set as access_key)"
                    ));
                }
                let provider = DropboxProvider::new(
                    &config.access_key,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_path
                )
                .await?;
                Ok(Provider::Dropbox(provider))
            }
            "box" => {
                if config.access_key.is_empty() {
                    return Err(anyhow::anyhow!(
                        "Box access_token required (set as access_key)"
                    ));
                }
                let provider = BoxProvider::new(
                    &config.access_key,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_id
                )
                .await?;
                Ok(Provider::Box(provider))
            }
            "mega" => {
                let email = config
                    .account_name
                    .as_ref()
                    .context("MEGA email required (set as account_name)")?;
                let password = config
                    .account_key
                    .as_ref()
                    .context("MEGA password required (set as account_key)")?;
                let provider = MegaProvider::new(
                    email,
                    password,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_path
                )
                .await?;
                Ok(Provider::Mega(provider))
            }
            "pcloud" => {
                if config.access_key.is_empty() {
                    return Err(anyhow::anyhow!("pCloud access_token required (set as access_key). Get it from https://my.pcloud.com/#page=apikeys"));
                }
                // Use region to determine US/EU data center (defaults to "us")
                let region = if config.region == "eu" || config.region == "europe" {
                    Some("eu")
                } else {
                    None
                };
                let provider = PCloudProvider::new(
                    &config.access_key,
                    region,
                    config.bucket_id.as_deref(), // Use bucket_id for folder_path
                )
                .await?;
                Ok(Provider::PCloud(provider))
            }
            "b2" | "backblaze" => {
                let provider = B2Provider::new(
                    config
                        .account_id
                        .as_ref()
                        .context("B2 account_id required")?,
                    config
                        .application_key
                        .as_ref()
                        .context("B2 application_key required")?,
                    config.bucket_id.as_ref().context("B2 bucket_id required")?,
                    &config.bucket,
                )
                .await?;
                Ok(Provider::B2(provider))
            }
            _ => Err(anyhow::anyhow!("Unknown provider: {}", config.provider)),
        }
    }
}

#[async_trait]
impl StorageProvider for Provider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        match self {
            Provider::S3(p) => p.upload(key, file_path).await,
            Provider::Gcs(p) => p.upload(key, file_path).await,
            Provider::Azure(p) => p.upload(key, file_path).await,
            Provider::B2(p) => p.upload(key, file_path).await,
            Provider::GoogleDrive(p) => p.upload(key, file_path).await,
            Provider::OneDrive(p) => p.upload(key, file_path).await,
            Provider::Dropbox(p) => p.upload(key, file_path).await,
            Provider::Box(p) => p.upload(key, file_path).await,
            Provider::Mega(p) => p.upload(key, file_path).await,
            Provider::PCloud(p) => p.upload(key, file_path).await,
        }
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        match self {
            Provider::S3(p) => p.download(key, output_path).await,
            Provider::Gcs(p) => p.download(key, output_path).await,
            Provider::Azure(p) => p.download(key, output_path).await,
            Provider::B2(p) => p.download(key, output_path).await,
            Provider::GoogleDrive(p) => p.download(key, output_path).await,
            Provider::OneDrive(p) => p.download(key, output_path).await,
            Provider::Dropbox(p) => p.download(key, output_path).await,
            Provider::Box(p) => p.download(key, output_path).await,
            Provider::Mega(p) => p.download(key, output_path).await,
            Provider::PCloud(p) => p.download(key, output_path).await,
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        match self {
            Provider::S3(p) => p.list(prefix).await,
            Provider::Gcs(p) => p.list(prefix).await,
            Provider::Azure(p) => p.list(prefix).await,
            Provider::B2(p) => p.list(prefix).await,
            Provider::GoogleDrive(p) => p.list(prefix).await,
            Provider::OneDrive(p) => p.list(prefix).await,
            Provider::Dropbox(p) => p.list(prefix).await,
            Provider::Box(p) => p.list(prefix).await,
            Provider::Mega(p) => p.list(prefix).await,
            Provider::PCloud(p) => p.list(prefix).await,
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        match self {
            Provider::S3(p) => p.delete(key).await,
            Provider::Gcs(p) => p.delete(key).await,
            Provider::Azure(p) => p.delete(key).await,
            Provider::B2(p) => p.delete(key).await,
            Provider::GoogleDrive(p) => p.delete(key).await,
            Provider::OneDrive(p) => p.delete(key).await,
            Provider::Dropbox(p) => p.delete(key).await,
            Provider::Box(p) => p.delete(key).await,
            Provider::Mega(p) => p.delete(key).await,
            Provider::PCloud(p) => p.delete(key).await,
        }
    }

    fn get_bucket(&self) -> &str {
        match self {
            Provider::S3(p) => p.get_bucket(),
            Provider::Gcs(p) => p.get_bucket(),
            Provider::Azure(p) => p.get_bucket(),
            Provider::B2(p) => p.get_bucket(),
            Provider::GoogleDrive(p) => p.get_bucket(),
            Provider::OneDrive(p) => p.get_bucket(),
            Provider::Dropbox(p) => p.get_bucket(),
            Provider::Box(p) => p.get_bucket(),
            Provider::Mega(p) => p.get_bucket(),
            Provider::PCloud(p) => p.get_bucket(),
        }
    }
}

// Storage configuration structure
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub provider: String,
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    // Optional fields for different providers
    pub account_id: Option<String>,
    pub account_name: Option<String>,
    pub account_key: Option<String>,
    pub application_key: Option<String>,
    pub bucket_id: Option<String>,
    pub credentials_path: Option<String>,
    #[allow(dead_code)]
    pub tenant_id: Option<String>,
}

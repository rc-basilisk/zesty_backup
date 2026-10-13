//! AWS S3 and S3-compatible services (Contabo, DigitalOcean Spaces, Wasabi, MinIO, Cloudflare R2,
//! Backblaze B2's S3 endpoint, ...).

use super::{non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::config::{Region, RequestChecksumCalculation, ResponseChecksumValidation};
use aws_sdk_s3::error::ProvideErrorMetadata;
use aws_sdk_s3::primitives::{ByteStream, Length};
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart};
use aws_sdk_s3::Client;
use chrono::DateTime;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tracing::{info, warn};

/// Files larger than this are sent with multipart upload (a single PUT is capped at 5 GiB).
const MULTIPART_THRESHOLD: u64 = 100 * 1024 * 1024;
const MIN_PART_SIZE: u64 = 64 * 1024 * 1024;
const MAX_PARTS: u64 = 10_000;

pub struct S3Provider {
    client: Client,
    bucket: String,
}

impl S3Provider {
    pub async fn new(kind: &str, config: &StorageConfig) -> Result<Self> {
        if config.bucket.trim().is_empty() {
            bail!("storage.bucket is required for provider {:?}", kind);
        }
        let region = non_empty(&config.region).unwrap_or(match kind {
            "r2" => "auto",
            _ => "us-east-1",
        });

        // An explicit endpoint always wins; otherwise derive it for well-known services.
        let endpoint = match non_empty(&config.endpoint) {
            Some(e) => Some(e.to_string()),
            None => match kind {
                "aws" | "s3" => None, // the SDK picks the right AWS endpoint
                "digitalocean" => Some(format!("https://{}.digitaloceanspaces.com", region)),
                "wasabi" => Some(format!("https://s3.{}.wasabisys.com", region)),
                "r2" => Some(format!(
                    "https://{}.r2.cloudflarestorage.com",
                    non_empty(&config.account_id).context(
                        "Cloudflare R2 needs storage.account_id (or an explicit endpoint)"
                    )?
                )),
                "contabo" => {
                    bail!("Contabo needs storage.endpoint, e.g. https://eu2.contabostorage.com")
                }
                "minio" => bail!("MinIO needs storage.endpoint, e.g. http://localhost:9000"),
                _ => None,
            },
        };

        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .region(Region::new(region.to_string()));
        // Without keys in the config, use the standard AWS credential chain
        // (env vars, ~/.aws/credentials, instance/IAM role, ...).
        match (non_empty(&config.access_key), non_empty(&config.secret_key)) {
            (Some(access), Some(secret)) => {
                loader = loader.credentials_provider(aws_credential_types::Credentials::new(
                    access,
                    secret,
                    None,
                    None,
                    "zesty-backup-config",
                ));
            }
            (None, None) => {
                info!("No access_key/secret_key in config, using the default AWS credential chain")
            }
            _ => bail!("Set both storage.access_key and storage.secret_key (or neither)"),
        }
        let sdk_config = loader.load().await;

        let mut builder = aws_sdk_s3::config::Builder::from(&sdk_config).force_path_style(
            config
                .force_path_style
                .unwrap_or(matches!(kind, "minio" | "contabo")),
        );
        if let Some(endpoint) = endpoint {
            builder = builder
                .endpoint_url(endpoint)
                // Recent SDK versions add CRC checksums to every request by default, which
                // many S3-compatible services reject. Only send them when an API requires it.
                .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
                .response_checksum_validation(ResponseChecksumValidation::WhenRequired);
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: config.bucket.clone(),
        })
    }

    async fn upload_multipart(&self, key: &str, file_path: &Path, size: u64) -> Result<()> {
        let part_size = MIN_PART_SIZE.max(size.div_ceil(MAX_PARTS));
        let created = self
            .client
            .create_multipart_upload()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)
            .context("Failed to start S3 multipart upload")?;
        let upload_id = created
            .upload_id()
            .context("S3 did not return an upload ID")?
            .to_string();

        let result = async {
            let mut parts = Vec::new();
            let mut offset = 0;
            let mut number = 1;
            while offset < size {
                let len = part_size.min(size - offset);
                let body = ByteStream::read_from()
                    .path(file_path)
                    .offset(offset)
                    .length(Length::Exact(len))
                    .build()
                    .await?;
                let part = self
                    .client
                    .upload_part()
                    .bucket(&self.bucket)
                    .key(key)
                    .upload_id(&upload_id)
                    .part_number(number)
                    .content_length(len as i64)
                    .body(body)
                    .send()
                    .await
                    .map_err(s3_error)
                    .with_context(|| format!("Failed to upload part {}", number))?;
                parts.push(
                    CompletedPart::builder()
                        .part_number(number)
                        .set_e_tag(part.e_tag().map(str::to_string))
                        .build(),
                );
                info!(
                    "  part {} uploaded ({}/{} MiB)",
                    number,
                    (offset + len) >> 20,
                    size >> 20
                );
                offset += len;
                number += 1;
            }
            self.client
                .complete_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .multipart_upload(
                    CompletedMultipartUpload::builder()
                        .set_parts(Some(parts))
                        .build(),
                )
                .send()
                .await
                .map_err(s3_error)
                .context("Failed to complete S3 multipart upload")?;
            anyhow::Ok(())
        }
        .await;

        if result.is_err() {
            // Otherwise the uploaded parts keep costing storage.
            if let Err(e) = self
                .client
                .abort_multipart_upload()
                .bucket(&self.bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await
            {
                warn!("Failed to abort multipart upload {}: {}", upload_id, e);
            }
        }
        result
    }
}

/// A readable error: the S3 error code and message when the service sent one,
/// otherwise the full error chain (connection errors, timeouts, ...).
fn s3_error<E, R>(err: aws_sdk_s3::error::SdkError<E, R>) -> anyhow::Error
where
    E: aws_sdk_s3::error::ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
    R: std::fmt::Debug + Send + Sync + 'static,
{
    match (err.code(), err.message()) {
        (Some(code), Some(message)) => anyhow::anyhow!("{}: {}", code, message),
        (Some(code), None) => anyhow::anyhow!("{}", code),
        _ => anyhow::anyhow!("{}", aws_sdk_s3::error::DisplayErrorContext(err)),
    }
}

#[async_trait]
impl StorageProvider for S3Provider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let size = super::http::file_len(file_path).await?;
        if size > MULTIPART_THRESHOLD {
            return self.upload_multipart(key, file_path, size).await;
        }
        let body = ByteStream::from_path(file_path)
            .await
            .with_context(|| format!("Failed to read {}", file_path.display()))?;
        self.client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .map_err(s3_error)
            .with_context(|| format!("Failed to upload {} to S3", key))?;
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(s3_error)
            .with_context(|| format!("Failed to download {} from S3", key))?;
        let mut file = tokio::fs::File::create(output_path)
            .await
            .with_context(|| format!("Failed to create {}", output_path.display()))?;
        let mut body = response.body.into_async_read();
        tokio::io::copy(&mut body, &mut file)
            .await
            .context("Failed while downloading from S3")?;
        file.flush().await?;
        file.sync_all().await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let mut items = Vec::new();
        let mut pages = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .into_paginator()
            .send();
        while let Some(page) = pages.next().await {
            let page = page
                .map_err(s3_error)
                .context("Failed to list S3 objects")?;
            for obj in page.contents() {
                if let Some(key) = obj.key() {
                    items.push(BackupItem {
                        key: key.to_string(),
                        size: obj.size().unwrap_or(0).max(0) as u64,
                        last_modified: obj
                            .last_modified()
                            .and_then(|dt| DateTime::from_timestamp(dt.secs(), 0)),
                    });
                }
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
            .map_err(s3_error)
            .with_context(|| format!("Failed to delete {} from S3", key))?;
        Ok(())
    }
}

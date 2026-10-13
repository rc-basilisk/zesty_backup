//! OneDrive through Microsoft Graph.
//! Docs: https://learn.microsoft.com/en-us/graph/api/driveitem-createuploadsession

use super::oauth::OAuthToken;
use super::{file_name, folder_path, http, key_for, non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::path::Path;

const GRAPH: &str = "https://graph.microsoft.com/v1.0";
/// Upload session chunks must be a multiple of 320 KiB and below 60 MiB.
const CHUNK_SIZE: usize = 32 * 320 * 1024; // 10 MiB

pub struct OneDriveProvider {
    token: OAuthToken,
    /// e.g. "/Backups", or "/" for the drive root
    folder: String,
    client: reqwest::Client,
}

impl OneDriveProvider {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        let tenant = non_empty(&config.tenant_id).unwrap_or("common");
        // Older configs used Graph-style paths such as "/drive/root:/Backups".
        let folder = config.bucket_id.as_deref().map(|f| {
            f.trim()
                .trim_start_matches("/drive/root:")
                .trim_end_matches(':')
        });
        Ok(Self {
            token: OAuthToken::new(
                "OneDrive",
                config,
                &format!(
                    "https://login.microsoftonline.com/{}/oauth2/v2.0/token",
                    tenant
                ),
                Some("offline_access Files.ReadWrite"),
            )?,
            folder: folder_path(folder),
            client: http::client(),
        })
    }

    /// Graph URL addressing `name` inside the folder (`name` empty = the folder itself).
    fn item_url(&self, name: &str) -> String {
        let path = format!("{}/{}", self.folder.trim_end_matches('/'), name);
        let path = path.trim_end_matches('/');
        if path.is_empty() {
            format!("{}/me/drive/root", GRAPH)
        } else {
            format!("{}/me/drive/root:{}:", GRAPH, http::encode_path(path))
        }
    }
}

#[async_trait]
impl StorageProvider for OneDriveProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        // Upload sessions work for any size (simple PUT is capped at 250 MB).
        let response = self
            .client
            .post(format!(
                "{}/createUploadSession",
                self.item_url(file_name(key))
            ))
            .bearer_auth(self.token.get().await?)
            .json(&json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } }))
            .send()
            .await
            .context("OneDrive createUploadSession request failed")?;
        let session = http::json(response, "OneDrive createUploadSession").await?;
        let upload_url = session["uploadUrl"]
            .as_str()
            .context("OneDrive did not return an uploadUrl")?
            .to_string();

        let size = http::file_len(file_path).await?;
        let mut file = tokio::fs::File::open(file_path).await?;
        let mut offset = 0u64;
        let result = async {
            loop {
                let chunk = http::read_chunk(&mut file, offset, CHUNK_SIZE).await?;
                let len = chunk.len() as u64;
                if len == 0 && size > 0 {
                    break;
                }
                // No Authorization header here: the upload URL is pre-authenticated.
                let response = self
                    .client
                    .put(&upload_url)
                    .header("Content-Length", len.to_string())
                    .header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", offset, (offset + len).max(1) - 1, size),
                    )
                    .body(chunk)
                    .send()
                    .await
                    .context("OneDrive chunk upload failed")?;
                http::check(response, "OneDrive chunk upload").await?;
                offset += len;
                if offset >= size {
                    break;
                }
            }
            anyhow::Ok(())
        }
        .await;
        if result.is_err() {
            let _ = self.client.delete(&upload_url).send().await;
        }
        result
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let response = self
            .client
            .get(format!("{}/content", self.item_url(file_name(key))))
            .bearer_auth(self.token.get().await?)
            .send()
            .await
            .context("OneDrive download request failed")?;
        http::save(response, output_path, "OneDrive download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let mut url = format!(
            "{}/children?$top=1000&$select=name,size,lastModifiedDateTime,file",
            self.item_url("")
        );
        let mut items = Vec::new();
        loop {
            let response = self
                .client
                .get(&url)
                .bearer_auth(self.token.get().await?)
                .send()
                .await
                .context("OneDrive list request failed")?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(items); // folder not created yet: nothing uploaded
            }
            let page = http::json(response, "OneDrive list").await?;
            for entry in page["value"].as_array().into_iter().flatten() {
                if entry.get("file").is_none() {
                    continue;
                }
                let Some(key) = entry["name"].as_str().and_then(|n| key_for(prefix, n)) else {
                    continue;
                };
                items.push(BackupItem {
                    key,
                    size: entry["size"].as_u64().unwrap_or(0),
                    last_modified: entry["lastModifiedDateTime"]
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                });
            }
            match page["@odata.nextLink"].as_str() {
                Some(next) => url = next.to_string(),
                None => break,
            }
        }
        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let response = self
            .client
            .delete(self.item_url(file_name(key)))
            .bearer_auth(self.token.get().await?)
            .send()
            .await
            .context("OneDrive delete request failed")?;
        http::check(response, "OneDrive delete").await?;
        Ok(())
    }
}

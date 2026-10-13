//! Box (API 2.0).
//! Docs: https://developer.box.com/reference/

use super::oauth::OAuthToken;
use super::{file_name, http, key_for, non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;

const API: &str = "https://api.box.com/2.0";
const UPLOAD: &str = "https://upload.box.com/api/2.0";
/// Direct uploads are limited to 50 MB; chunked uploads need at least 20 MB.
const CHUNKED_THRESHOLD: u64 = 50 * 1024 * 1024;

pub struct BoxProvider {
    token: OAuthToken,
    folder_id: String,
    client: reqwest::Client,
}

impl BoxProvider {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        Ok(Self {
            token: OAuthToken::new("Box", config, "https://api.box.com/oauth2/token", None)?,
            folder_id: non_empty(&config.bucket_id).unwrap_or("0").to_string(),
            client: http::client(),
        })
    }

    /// All files in the folder.
    async fn files(&self) -> Result<Vec<Value>> {
        let mut files = Vec::new();
        let mut offset = 0u64;
        loop {
            let response = self
                .client
                .get(format!("{}/folders/{}/items", API, self.folder_id))
                .bearer_auth(self.token.get().await?)
                // size and modified_at are not returned unless asked for
                .query(&[
                    ("fields", "type,name,size,modified_at"),
                    ("limit", "1000"),
                    ("offset", &offset.to_string()),
                ])
                .send()
                .await
                .context("Box list request failed")?;
            let page = http::json(response, "Box list").await?;
            let entries = page["entries"].as_array().cloned().unwrap_or_default();
            offset += entries.len() as u64;
            files.extend(entries.into_iter().filter(|e| e["type"] == "file"));
            let total = page["total_count"].as_u64().unwrap_or(0);
            if offset >= total || page["entries"].as_array().is_none_or(|e| e.is_empty()) {
                break;
            }
        }
        Ok(files)
    }

    async fn find(&self, key: &str) -> Result<Option<String>> {
        let name = file_name(key);
        Ok(self
            .files()
            .await?
            .iter()
            .find(|f| f["name"].as_str() == Some(name))
            .and_then(|f| f["id"].as_str())
            .map(str::to_string))
    }

    async fn upload_direct(
        &self,
        name: &str,
        existing: Option<&str>,
        file_path: &Path,
    ) -> Result<()> {
        let (body, len) = http::file_body(file_path).await?;
        let (url, attributes) = match existing {
            Some(id) => (
                format!("{}/files/{}/content", UPLOAD, id),
                json!({ "name": name }),
            ),
            None => (
                format!("{}/files/content", UPLOAD),
                json!({ "name": name, "parent": { "id": self.folder_id } }),
            ),
        };
        // `attributes` must come before `file`.
        let form = reqwest::multipart::Form::new()
            .text("attributes", attributes.to_string())
            .part(
                "file",
                reqwest::multipart::Part::stream_with_length(body, len).file_name(name.to_string()),
            );
        let response = self
            .client
            .post(url)
            .bearer_auth(self.token.get().await?)
            .multipart(form)
            .send()
            .await
            .context("Box upload request failed")?;
        http::check(response, "Box upload").await?;
        Ok(())
    }

    async fn upload_chunked(
        &self,
        name: &str,
        existing: Option<&str>,
        file_path: &Path,
        size: u64,
    ) -> Result<()> {
        let (url, body) = match existing {
            Some(id) => (
                format!("{}/files/{}/upload_sessions", UPLOAD, id),
                json!({ "file_size": size, "file_name": name }),
            ),
            None => (
                format!("{}/files/upload_sessions", UPLOAD),
                json!({ "folder_id": self.folder_id, "file_size": size, "file_name": name }),
            ),
        };
        let response = self
            .client
            .post(url)
            .bearer_auth(self.token.get().await?)
            .json(&body)
            .send()
            .await
            .context("Box upload session request failed")?;
        let session = http::json(response, "Box upload session").await?;
        let part_size = session["part_size"].as_u64().context("Box: no part_size")?;
        let upload_url = session["session_endpoints"]["upload_part"]
            .as_str()
            .context("Box: no upload_part endpoint")?
            .to_string();
        let commit_url = session["session_endpoints"]["commit"]
            .as_str()
            .context("Box: no commit endpoint")?
            .to_string();
        let abort_url = session["session_endpoints"]["abort"]
            .as_str()
            .map(str::to_string);

        let result = async {
            let mut file = tokio::fs::File::open(file_path).await?;
            let mut parts = Vec::new();
            let mut offset = 0u64;
            while offset < size {
                let chunk = http::read_chunk(&mut file, offset, part_size as usize).await?;
                let len = chunk.len() as u64;
                let digest = base64::engine::general_purpose::STANDARD.encode(http::sha1(&chunk));
                let response = self
                    .client
                    .put(&upload_url)
                    .bearer_auth(self.token.get().await?)
                    .header("Content-Type", "application/octet-stream")
                    .header(
                        "Content-Range",
                        format!("bytes {}-{}/{}", offset, offset + len - 1, size),
                    )
                    .header("Digest", format!("sha={}", digest))
                    .body(chunk)
                    .send()
                    .await
                    .context("Box part upload failed")?;
                let part = http::json(response, "Box part upload").await?;
                parts.push(part["part"].clone());
                offset += len;
            }

            let digest =
                base64::engine::general_purpose::STANDARD.encode(http::sha1_file(file_path).await?);
            for _ in 0..30 {
                let response = self
                    .client
                    .post(&commit_url)
                    .bearer_auth(self.token.get().await?)
                    .header("Digest", format!("sha={}", digest))
                    .json(&json!({ "parts": parts }))
                    .send()
                    .await
                    .context("Box commit request failed")?;
                // 202: parts still being processed, retry later
                if response.status() == reqwest::StatusCode::ACCEPTED {
                    let wait = response
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(5);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
                http::check(response, "Box commit").await?;
                return Ok(());
            }
            bail!("Box commit did not complete")
        }
        .await;

        if result.is_err() {
            if let Some(abort) = abort_url {
                if let Ok(token) = self.token.get().await {
                    let _ = self.client.delete(abort).bearer_auth(token).send().await;
                }
            }
        }
        result
    }
}

#[async_trait]
impl StorageProvider for BoxProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let name = file_name(key);
        // Box refuses a second file with the same name; upload a new version instead.
        let existing = self.find(key).await?;
        let size = http::file_len(file_path).await?;
        if size > CHUNKED_THRESHOLD {
            self.upload_chunked(name, existing.as_deref(), file_path, size)
                .await
        } else {
            self.upload_direct(name, existing.as_deref(), file_path)
                .await
        }
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let id = self
            .find(key)
            .await?
            .with_context(|| format!("{} not found in Box", file_name(key)))?;
        let response = self
            .client
            .get(format!("{}/files/{}/content", API, id))
            .bearer_auth(self.token.get().await?)
            .send()
            .await
            .context("Box download request failed")?;
        http::save(response, output_path, "Box download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        Ok(self
            .files()
            .await?
            .iter()
            .filter_map(|f| {
                Some(BackupItem {
                    key: key_for(prefix, f["name"].as_str()?)?,
                    size: f["size"].as_u64().unwrap_or(0),
                    last_modified: f["modified_at"]
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                })
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let Some(id) = self.find(key).await? else {
            bail!("{} not found in Box", file_name(key));
        };
        let response = self
            .client
            .delete(format!("{}/files/{}", API, id))
            .bearer_auth(self.token.get().await?)
            .send()
            .await
            .context("Box delete request failed")?;
        http::check(response, "Box delete").await?;
        Ok(())
    }
}

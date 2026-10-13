//! Dropbox (API v2).
//! Docs: https://www.dropbox.com/developers/documentation/http/documentation

use super::oauth::OAuthToken;
use super::{file_name, folder_path, http, key_for, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::path::Path;

const API: &str = "https://api.dropboxapi.com/2";
const CONTENT: &str = "https://content.dropboxapi.com/2";
/// `/files/upload` accepts at most 150 MB; bigger files go through an upload session.
const SIMPLE_UPLOAD_LIMIT: u64 = 150 * 1024 * 1024;
const CHUNK_SIZE: usize = 64 * 1024 * 1024;

pub struct DropboxProvider {
    token: OAuthToken,
    folder: String,
    client: reqwest::Client,
}

impl DropboxProvider {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        Ok(Self {
            token: OAuthToken::new(
                "Dropbox",
                config,
                "https://api.dropboxapi.com/oauth2/token",
                None,
            )?,
            folder: folder_path(config.bucket_id.as_deref()),
            client: http::client(),
        })
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{}", self.folder.trim_end_matches('/'), file_name(key))
    }

    async fn content_call(
        &self,
        endpoint: &str,
        arg: serde_json::Value,
        body: impl Into<reqwest::Body>,
    ) -> Result<reqwest::Response> {
        let response = self
            .client
            .post(format!("{}/{}", CONTENT, endpoint))
            .bearer_auth(self.token.get().await?)
            .header("Dropbox-API-Arg", api_arg(&arg))
            .header("Content-Type", "application/octet-stream")
            .body(body)
            .send()
            .await
            .with_context(|| format!("Dropbox {} request failed", endpoint))?;
        http::check(response, &format!("Dropbox {}", endpoint)).await
    }

    async fn api_call(&self, endpoint: &str, body: serde_json::Value) -> Result<reqwest::Response> {
        self.client
            .post(format!("{}/{}", API, endpoint))
            .bearer_auth(self.token.get().await?)
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Dropbox {} request failed", endpoint))
    }
}

/// The `Dropbox-API-Arg` header must be ASCII: escape everything else as `\uXXXX`.
fn api_arg(value: &serde_json::Value) -> String {
    let mut out = String::new();
    for c in value.to_string().chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut buf = [0u16; 2];
            for unit in c.encode_utf16(&mut buf) {
                out.push_str(&format!("\\u{:04x}", unit));
            }
        }
    }
    out
}

#[async_trait]
impl StorageProvider for DropboxProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let path = self.path(key);
        let size = http::file_len(file_path).await?;
        let commit = json!({ "path": path, "mode": "overwrite", "mute": true });

        if size <= SIMPLE_UPLOAD_LIMIT {
            let (body, _) = http::file_body(file_path).await?;
            self.content_call("files/upload", commit, body).await?;
            return Ok(());
        }

        let mut file = tokio::fs::File::open(file_path).await?;
        let first = http::read_chunk(&mut file, 0, CHUNK_SIZE).await?;
        let mut offset = first.len() as u64;
        let started: serde_json::Value = self
            .content_call(
                "files/upload_session/start",
                json!({ "close": false }),
                first,
            )
            .await?
            .json()
            .await?;
        let session_id = started["session_id"]
            .as_str()
            .context("Dropbox did not return a session_id")?
            .to_string();

        loop {
            let chunk = http::read_chunk(&mut file, offset, CHUNK_SIZE).await?;
            let len = chunk.len() as u64;
            let cursor = json!({ "session_id": session_id, "offset": offset });
            if offset + len >= size {
                self.content_call(
                    "files/upload_session/finish",
                    json!({ "cursor": cursor, "commit": commit }),
                    chunk,
                )
                .await?;
                return Ok(());
            }
            self.content_call(
                "files/upload_session/append_v2",
                json!({ "cursor": cursor, "close": false }),
                chunk,
            )
            .await?;
            offset += len;
        }
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let response = self
            .client
            .post(format!("{}/files/download", CONTENT))
            .bearer_auth(self.token.get().await?)
            .header(
                "Dropbox-API-Arg",
                api_arg(&json!({ "path": self.path(key) })),
            )
            .send()
            .await
            .context("Dropbox download request failed")?;
        http::save(response, output_path, "Dropbox download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        // The API wants "" for the root folder.
        let folder = if self.folder == "/" { "" } else { &self.folder };
        let response = self
            .api_call(
                "files/list_folder",
                json!({ "path": folder, "limit": 2000 }),
            )
            .await?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            let body = response.text().await.unwrap_or_default();
            if body.contains("not_found") {
                return Ok(Vec::new()); // folder not created yet: nothing uploaded
            }
            anyhow::bail!("Dropbox list_folder failed: {}", body);
        }
        let mut page = http::json(response, "Dropbox list_folder").await?;
        let mut items = Vec::new();
        loop {
            for entry in page["entries"].as_array().into_iter().flatten() {
                if entry[".tag"] != "file" {
                    continue;
                }
                let Some(key) = entry["name"].as_str().and_then(|n| key_for(prefix, n)) else {
                    continue;
                };
                items.push(BackupItem {
                    key,
                    size: entry["size"].as_u64().unwrap_or(0),
                    last_modified: entry["server_modified"]
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                });
            }
            if page["has_more"].as_bool() != Some(true) {
                break;
            }
            let cursor = page["cursor"].as_str().unwrap_or_default().to_string();
            let response = self
                .api_call("files/list_folder/continue", json!({ "cursor": cursor }))
                .await?;
            page = http::json(response, "Dropbox list_folder/continue").await?;
        }
        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let response = self
            .api_call("files/delete_v2", json!({ "path": self.path(key) }))
            .await?;
        http::check(response, "Dropbox delete").await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn escapes_non_ascii() {
        let arg = super::api_arg(&serde_json::json!({ "path": "/Sauvegardes/é" }));
        assert_eq!(arg, r#"{"path":"/Sauvegardes/\u00e9"}"#);
    }
}

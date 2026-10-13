//! Google Drive (API v3).
//! Docs: https://developers.google.com/workspace/drive/api/guides/manage-uploads

use super::oauth::OAuthToken;
use super::{file_name, http, key_for, non_empty, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::json;
use std::path::Path;

const API: &str = "https://www.googleapis.com/drive/v3";
const UPLOAD: &str = "https://www.googleapis.com/upload/drive/v3";

pub struct GoogleDriveProvider {
    token: OAuthToken,
    folder_id: String,
    client: reqwest::Client,
}

impl GoogleDriveProvider {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        Ok(Self {
            token: OAuthToken::new(
                "Google Drive",
                config,
                "https://oauth2.googleapis.com/token",
                None,
            )?,
            folder_id: non_empty(&config.bucket_id).unwrap_or("root").to_string(),
            client: http::client(),
        })
    }

    /// One page of files in the folder matching an optional extra query.
    async fn query(&self, extra: Option<String>) -> Result<Vec<serde_json::Value>> {
        let mut q = format!(
            "'{}' in parents and trashed = false",
            escape(&self.folder_id)
        );
        if let Some(extra) = extra {
            q = format!("{} and {}", q, extra);
        }
        let mut files = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut request = self
                .client
                .get(format!("{}/files", API))
                .bearer_auth(self.token.get().await?)
                .query(&[
                    ("q", q.as_str()),
                    ("pageSize", "1000"),
                    (
                        "fields",
                        "nextPageToken,files(id,name,size,modifiedTime,mimeType)",
                    ),
                    ("supportsAllDrives", "true"),
                    ("includeItemsFromAllDrives", "true"),
                ]);
            if let Some(token) = &page_token {
                request = request.query(&[("pageToken", token)]);
            }
            let json = http::json(
                request
                    .send()
                    .await
                    .context("Google Drive list request failed")?,
                "Google Drive list",
            )
            .await?;
            files.extend(json["files"].as_array().cloned().unwrap_or_default());
            match json["nextPageToken"].as_str() {
                Some(t) => page_token = Some(t.to_string()),
                None => break,
            }
        }
        Ok(files)
    }

    async fn find(&self, key: &str) -> Result<Option<String>> {
        let files = self
            .query(Some(format!("name = '{}'", escape(file_name(key)))))
            .await?;
        Ok(files
            .first()
            .and_then(|f| f["id"].as_str())
            .map(str::to_string))
    }
}

/// Escape a value for a Drive query string literal.
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

#[async_trait]
impl StorageProvider for GoogleDriveProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let name = file_name(key);
        let (body, len) = http::file_body(file_path).await?;

        // Resumable upload: one request to open a session, one PUT with the whole file.
        // An existing file with the same name gets a new revision instead of a duplicate.
        let start = match self.find(key).await? {
            Some(id) => self
                .client
                .patch(format!("{}/files/{}", UPLOAD, id))
                .json(&json!({})),
            None => self
                .client
                .post(format!("{}/files", UPLOAD))
                .json(&json!({ "name": name, "parents": [self.folder_id] })),
        };
        let response = start
            .bearer_auth(self.token.get().await?)
            .query(&[("uploadType", "resumable"), ("supportsAllDrives", "true")])
            .header("X-Upload-Content-Type", "application/octet-stream")
            .header("X-Upload-Content-Length", len.to_string())
            .send()
            .await
            .context("Google Drive upload request failed")?;
        let response = http::check(response, "Google Drive upload session").await?;
        let session = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .context("Google Drive did not return an upload session URL")?
            .to_string();

        let response = self
            .client
            .put(session)
            .header("Content-Length", len.to_string())
            .body(body)
            .send()
            .await
            .context("Google Drive upload failed")?;
        http::check(response, "Google Drive upload").await?;
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let id = self
            .find(key)
            .await?
            .with_context(|| format!("{} not found in Google Drive", file_name(key)))?;
        let response = self
            .client
            .get(format!("{}/files/{}", API, id))
            .bearer_auth(self.token.get().await?)
            .query(&[("alt", "media"), ("supportsAllDrives", "true")])
            .send()
            .await
            .context("Google Drive download request failed")?;
        http::save(response, output_path, "Google Drive download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let files = self
            .query(Some(
                "mimeType != 'application/vnd.google-apps.folder'".into(),
            ))
            .await?;
        Ok(files
            .iter()
            .filter_map(|f| {
                let key = key_for(prefix, f["name"].as_str()?)?;
                Some(BackupItem {
                    key,
                    // Drive returns int64 values as strings
                    size: f["size"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0),
                    last_modified: f["modifiedTime"]
                        .as_str()
                        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                })
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let Some(id) = self.find(key).await? else {
            bail!("{} not found in Google Drive", file_name(key));
        };
        let response = self
            .client
            .delete(format!("{}/files/{}", API, id))
            .bearer_auth(self.token.get().await?)
            .query(&[("supportsAllDrives", "true")])
            .send()
            .await
            .context("Google Drive delete request failed")?;
        http::check(response, "Google Drive delete").await?;
        Ok(())
    }
}

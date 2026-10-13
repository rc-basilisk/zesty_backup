//! Backblaze B2 native API (v4).
//! Docs: https://www.backblaze.com/apidocs
//!
//! Tip: B2 also has an S3-compatible endpoint (`provider = "s3"`,
//! `endpoint = "https://s3.<region>.backblazeb2.com"`), which works just as well.

use super::{http, non_empty, required, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use chrono::DateTime;
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::info;

const AUTH_URL: &str = "https://api.backblazeb2.com/b2api/v4/b2_authorize_account";
/// Auth tokens are valid for 24 hours; re-authorize well before that.
const AUTH_MAX_AGE: Duration = Duration::from_secs(20 * 3600);
/// Single uploads are limited to 5 GB; above this we use the large file API.
const LARGE_FILE_THRESHOLD: u64 = 200 * 1024 * 1024;

pub struct B2Provider {
    key_id: String,
    application_key: String,
    bucket_name: String,
    bucket_id: Mutex<Option<String>>,
    auth: Mutex<Option<Auth>>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct Auth {
    account_id: String,
    token: String,
    api_url: String,
    download_url: String,
    part_size: u64,
    obtained: Instant,
}

impl B2Provider {
    pub async fn new(config: &StorageConfig) -> Result<Self> {
        if config.bucket.trim().is_empty() {
            bail!("storage.bucket (the bucket name) is required for B2");
        }
        let provider = Self {
            key_id: required(&config.account_id, "storage.account_id (B2 keyID)")?.to_string(),
            application_key: required(&config.application_key, "storage.application_key")?
                .to_string(),
            bucket_name: config.bucket.clone(),
            bucket_id: Mutex::new(non_empty(&config.bucket_id).map(str::to_string)),
            auth: Mutex::new(None),
            client: http::client(),
        };
        // Fail early on bad credentials.
        provider.auth().await?;
        Ok(provider)
    }

    async fn auth(&self) -> Result<Auth> {
        let mut guard = self.auth.lock().await;
        if let Some(auth) = guard.as_ref() {
            if auth.obtained.elapsed() < AUTH_MAX_AGE {
                return Ok(auth.clone());
            }
        }
        let credentials = base64::engine::general_purpose::STANDARD
            .encode(format!("{}:{}", self.key_id, self.application_key));
        let response = self
            .client
            .get(AUTH_URL)
            .header("Authorization", format!("Basic {}", credentials))
            .send()
            .await
            .context("B2 authorization request failed")?;
        let json = http::json(
            response,
            "B2 authorization (check account_id/application_key)",
        )
        .await?;
        let storage = &json["apiInfo"]["storageApi"];
        let auth = Auth {
            account_id: json["accountId"]
                .as_str()
                .context("B2: no accountId")?
                .to_string(),
            token: json["authorizationToken"]
                .as_str()
                .context("B2: no authorizationToken")?
                .to_string(),
            api_url: storage["apiUrl"]
                .as_str()
                .context("B2: no apiUrl")?
                .to_string(),
            download_url: storage["downloadUrl"]
                .as_str()
                .context("B2: no downloadUrl")?
                .to_string(),
            part_size: storage["recommendedPartSize"]
                .as_u64()
                .unwrap_or(100_000_000),
            obtained: Instant::now(),
        };
        *guard = Some(auth.clone());
        Ok(auth)
    }

    /// POST a JSON API call, re-authorizing once if the token expired.
    async fn call(&self, method: &str, body: Value) -> Result<Value> {
        for attempt in 0..2 {
            let auth = self.auth().await?;
            let response = self
                .client
                .post(format!("{}/b2api/v4/{}", auth.api_url, method))
                .header("Authorization", &auth.token)
                .json(&body)
                .send()
                .await
                .with_context(|| format!("B2 {} request failed", method))?;
            if response.status() == reqwest::StatusCode::UNAUTHORIZED && attempt == 0 {
                *self.auth.lock().await = None;
                continue;
            }
            return http::json(response, &format!("B2 {}", method)).await;
        }
        unreachable!()
    }

    async fn bucket_id(&self) -> Result<String> {
        let mut guard = self.bucket_id.lock().await;
        if let Some(id) = guard.as_ref() {
            return Ok(id.clone());
        }
        let account_id = self.auth().await?.account_id;
        let json = self
            .call(
                "b2_list_buckets",
                json!({ "accountId": account_id, "bucketName": self.bucket_name }),
            )
            .await?;
        let id = json["buckets"][0]["bucketId"]
            .as_str()
            .with_context(|| {
                format!(
                    "B2 bucket {:?} not found (or not accessible with this key)",
                    self.bucket_name
                )
            })?
            .to_string();
        *guard = Some(id.clone());
        Ok(id)
    }

    async fn upload_large(&self, key: &str, file_path: &Path, size: u64) -> Result<()> {
        let bucket_id = self.bucket_id().await?;
        let part_size = self
            .auth()
            .await?
            .part_size
            .max(5 * 1024 * 1024)
            .max(size.div_ceil(10_000));
        let started = self
            .call(
                "b2_start_large_file",
                json!({ "bucketId": bucket_id, "fileName": key, "contentType": "application/octet-stream" }),
            )
            .await?;
        let file_id = started["fileId"]
            .as_str()
            .context("B2: no fileId")?
            .to_string();

        let result = async {
            let mut file = tokio::fs::File::open(file_path).await?;
            let mut sha1s = Vec::new();
            let mut offset = 0;
            let mut part = 1;
            let mut target: Option<(String, String)> = None;
            while offset < size {
                let chunk = http::read_chunk(&mut file, offset, part_size as usize).await?;
                let sha1 = http::hex(&http::sha1(&chunk));
                let mut uploaded = false;
                for _attempt in 0..3 {
                    let (url, token) = match &target {
                        Some(t) => t.clone(),
                        None => {
                            let r = self
                                .call("b2_get_upload_part_url", json!({ "fileId": file_id }))
                                .await?;
                            let t = (
                                r["uploadUrl"]
                                    .as_str()
                                    .context("B2: no uploadUrl")?
                                    .to_string(),
                                r["authorizationToken"]
                                    .as_str()
                                    .context("B2: no authorizationToken")?
                                    .to_string(),
                            );
                            target = Some(t.clone());
                            t
                        }
                    };
                    let response = self
                        .client
                        .post(&url)
                        .header("Authorization", token)
                        .header("X-Bz-Part-Number", part.to_string())
                        .header("X-Bz-Content-Sha1", &sha1)
                        .header("Content-Length", chunk.len().to_string())
                        .body(chunk.clone())
                        .send()
                        .await;
                    match response {
                        Ok(r) if r.status().is_success() => {
                            uploaded = true;
                            break;
                        }
                        // 401/503 or a connection error: get a fresh upload URL and retry.
                        _ => target = None,
                    }
                }
                anyhow::ensure!(uploaded, "B2: uploading part {} failed 3 times", part);
                sha1s.push(sha1);
                info!(
                    "  part {} uploaded ({}/{} MiB)",
                    part,
                    (offset + chunk.len() as u64) >> 20,
                    size >> 20
                );
                offset += chunk.len() as u64;
                part += 1;
            }
            self.call(
                "b2_finish_large_file",
                json!({ "fileId": file_id, "partSha1Array": sha1s }),
            )
            .await?;
            anyhow::Ok(())
        }
        .await;

        if result.is_err() {
            let _ = self
                .call("b2_cancel_large_file", json!({ "fileId": file_id }))
                .await;
        }
        result
    }
}

#[async_trait]
impl StorageProvider for B2Provider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        let size = http::file_len(file_path).await?;
        if size > LARGE_FILE_THRESHOLD {
            return self.upload_large(key, file_path, size).await;
        }
        let bucket_id = self.bucket_id().await?;
        let sha1 = http::hex(&http::sha1_file(file_path).await?);
        let target = self
            .call("b2_get_upload_url", json!({ "bucketId": bucket_id }))
            .await?;
        let (body, len) = http::file_body(file_path).await?;
        let response = self
            .client
            .post(target["uploadUrl"].as_str().context("B2: no uploadUrl")?)
            .header(
                "Authorization",
                target["authorizationToken"]
                    .as_str()
                    .context("B2: no authorizationToken")?,
            )
            // File names must be percent-encoded (not base64).
            .header("X-Bz-File-Name", http::encode_path(key))
            .header("Content-Type", "b2/x-auto")
            .header("Content-Length", len.to_string())
            .header("X-Bz-Content-Sha1", sha1)
            .body(body)
            .send()
            .await
            .context("B2 upload request failed")?;
        http::check(response, "B2 upload").await?;
        Ok(())
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        let auth = self.auth().await?;
        let url = format!(
            "{}/file/{}/{}",
            auth.download_url,
            http::encode_path(&self.bucket_name),
            http::encode_path(key)
        );
        let response = self
            .client
            .get(url)
            .header("Authorization", &auth.token)
            .send()
            .await
            .context("B2 download request failed")?;
        http::save(response, output_path, "B2 download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let bucket_id = self.bucket_id().await?;
        let mut items = Vec::new();
        let mut start: Option<String> = None;
        loop {
            let mut body = json!({ "bucketId": bucket_id, "maxFileCount": 1000, "prefix": prefix });
            if let Some(s) = start.take() {
                body["startFileName"] = Value::String(s);
            }
            let json = self.call("b2_list_file_names", body).await?;
            for file in json["files"].as_array().into_iter().flatten() {
                if file["action"].as_str().is_some_and(|a| a != "upload") {
                    continue; // unfinished large files, hide markers, folders
                }
                let Some(name) = file["fileName"].as_str() else {
                    continue;
                };
                items.push(BackupItem {
                    key: name.to_string(),
                    size: file["contentLength"].as_u64().unwrap_or(0),
                    last_modified: file["uploadTimestamp"]
                        .as_i64()
                        .and_then(DateTime::from_timestamp_millis),
                });
            }
            match json["nextFileName"].as_str() {
                Some(next) => start = Some(next.to_string()),
                None => break,
            }
        }
        Ok(items)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        // Delete every version of exactly this file name.
        let bucket_id = self.bucket_id().await?;
        let json = self
            .call(
                "b2_list_file_versions",
                json!({ "bucketId": bucket_id, "startFileName": key, "prefix": key, "maxFileCount": 100 }),
            )
            .await?;
        let mut found = false;
        for file in json["files"].as_array().into_iter().flatten() {
            if file["fileName"].as_str() != Some(key) {
                continue;
            }
            found = true;
            self.call(
                "b2_delete_file_version",
                json!({ "fileName": key, "fileId": file["fileId"] }),
            )
            .await?;
        }
        if !found {
            bail!("B2: {} not found", key);
        }
        Ok(())
    }
}

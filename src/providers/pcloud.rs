//! pCloud.
//! Docs: https://docs.pcloud.com/
//!
//! Authenticates with an OAuth2 access token (pCloud tokens do not expire), sent as a
//! Bearer header. Accounts live either in the US (api.pcloud.com) or the EU (eapi.pcloud.com)
//! data center; set `region = "eu"` for the latter.

use super::{file_name, folder_path, http, key_for, required, BackupItem, StorageProvider};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::DateTime;
use serde_json::Value;
use std::path::Path;

/// pCloud error code for "Directory does not exist."
const DIRECTORY_NOT_FOUND: i64 = 2005;

pub struct PCloudProvider {
    access_token: String,
    api_host: String,
    folder: String,
    client: reqwest::Client,
}

impl PCloudProvider {
    pub async fn new(config: &StorageConfig) -> Result<Self> {
        let api_host = match config
            .region
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("eu") | Some("europe") => "https://eapi.pcloud.com",
            _ => "https://api.pcloud.com",
        };
        Ok(Self {
            access_token: required(
                &config.access_key,
                "storage.access_key (pCloud OAuth access token)",
            )?
            .to_string(),
            api_host: api_host.to_string(),
            folder: folder_path(config.bucket_id.as_deref()),
            client: http::client(),
        })
    }

    fn path(&self, key: &str) -> String {
        format!("{}/{}", self.folder.trim_end_matches('/'), file_name(key))
    }

    /// Call a method; pCloud reports errors as HTTP 200 with a non-zero `result`.
    async fn call_raw(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let response = self
            .client
            .get(format!("{}/{}", self.api_host, method))
            .bearer_auth(&self.access_token)
            .query(params)
            .send()
            .await
            .with_context(|| format!("pCloud {} request failed", method))?;
        http::json(response, &format!("pCloud {}", method)).await
    }

    async fn call(&self, method: &str, params: &[(&str, &str)]) -> Result<Value> {
        let json = self.call_raw(method, params).await?;
        check_result(&json, method)?;
        Ok(json)
    }
}

fn check_result(json: &Value, method: &str) -> Result<()> {
    match json["result"].as_i64() {
        Some(0) => Ok(()),
        code => {
            let hint = if json["result"] == 2000 || json["result"] == 1000 {
                " (wrong token, or wrong region: EU accounts need region = \"eu\")"
            } else {
                ""
            };
            bail!(
                "pCloud {} failed: {} (code {:?}){}",
                method,
                json["error"].as_str().unwrap_or("unknown error"),
                code,
                hint
            )
        }
    }
}

#[async_trait]
impl StorageProvider for PCloudProvider {
    async fn upload(&self, key: &str, file_path: &Path) -> Result<()> {
        if self.folder != "/" {
            self.call("createfolderifnotexists", &[("path", &self.folder)])
                .await?;
        }
        let name = file_name(key);
        let (body, len) = http::file_body(file_path).await?;
        let form = reqwest::multipart::Form::new().part(
            "file",
            reqwest::multipart::Part::stream_with_length(body, len).file_name(name.to_string()),
        );
        // Parameters go in the query string: pCloud ignores form fields sent after the file.
        // An existing file with the same name is overwritten (the old one is kept as a revision).
        let response = self
            .client
            .post(format!("{}/uploadfile", self.api_host))
            .bearer_auth(&self.access_token)
            .query(&[
                ("path", self.folder.as_str()),
                ("filename", name),
                ("nopartial", "1"),
            ])
            .multipart(form)
            .send()
            .await
            .context("pCloud upload request failed")?;
        let json = http::json(response, "pCloud upload").await?;
        check_result(&json, "uploadfile")
    }

    async fn download(&self, key: &str, output_path: &Path) -> Result<()> {
        // `downloadfile` is for making pCloud fetch a URL; to read a file, get a link first.
        let link = self
            .call("getfilelink", &[("path", &self.path(key))])
            .await?;
        let host = link["hosts"][0]
            .as_str()
            .context("pCloud: no download host")?;
        let path = link["path"].as_str().context("pCloud: no download path")?;
        let response = self
            .client
            .get(format!("https://{}{}", host, path))
            .send()
            .await
            .context("pCloud download request failed")?;
        http::save(response, output_path, "pCloud download").await?;
        Ok(())
    }

    async fn list(&self, prefix: &str) -> Result<Vec<BackupItem>> {
        let json = self
            .call_raw(
                "listfolder",
                &[("path", &self.folder), ("timeformat", "timestamp")],
            )
            .await?;
        if json["result"].as_i64() == Some(DIRECTORY_NOT_FOUND) {
            return Ok(Vec::new()); // nothing uploaded yet
        }
        check_result(&json, "listfolder")?;
        Ok(json["metadata"]["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["isfolder"].as_bool() == Some(false))
            .filter_map(|f| {
                Some(BackupItem {
                    key: key_for(prefix, f["name"].as_str()?)?,
                    size: f["size"].as_u64().unwrap_or(0),
                    last_modified: f["modified"]
                        .as_i64()
                        .and_then(|t| DateTime::from_timestamp(t, 0)),
                })
            })
            .collect())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.call("deletefile", &[("path", &self.path(key))])
            .await?;
        Ok(())
    }
}

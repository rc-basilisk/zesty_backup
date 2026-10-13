//! Small helpers shared by the REST-based providers.

use anyhow::{bail, Context, Result};
use futures::StreamExt;
use std::path::Path;
use std::time::Duration;
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

pub fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("zesty-backup/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(30))
        // No overall timeout: uploads of large archives can legitimately take hours.
        .read_timeout(Duration::from_secs(300))
        .build()
        .expect("failed to build HTTP client")
}

/// Turn a non-2xx response into an error that includes the body (APIs put the reason there).
pub async fn check(response: reqwest::Response, what: &str) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = response.text().await.unwrap_or_default();
    bail!("{} failed: HTTP {}: {}", what, status, body.trim());
}

pub async fn json(response: reqwest::Response, what: &str) -> Result<serde_json::Value> {
    check(response, what)
        .await?
        .json()
        .await
        .with_context(|| format!("{}: invalid JSON response", what))
}

/// Stream a response body into a file.
pub async fn save(response: reqwest::Response, output_path: &Path, what: &str) -> Result<u64> {
    let response = check(response, what).await?;
    let mut file = File::create(output_path)
        .await
        .with_context(|| format!("Failed to create {}", output_path.display()))?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.with_context(|| format!("{}: connection error", what))?;
        file.write_all(&chunk).await?;
        written += chunk.len() as u64;
    }
    file.flush().await?;
    file.sync_all().await?;
    Ok(written)
}

/// A streaming request body for a whole file.
pub async fn file_body(path: &Path) -> Result<(reqwest::Body, u64)> {
    let file = File::open(path)
        .await
        .with_context(|| format!("Failed to open {}", path.display()))?;
    let len = file.metadata().await?.len();
    let stream = tokio_util::io::ReaderStream::with_capacity(file, 256 * 1024);
    Ok((reqwest::Body::wrap_stream(stream), len))
}

pub async fn file_len(path: &Path) -> Result<u64> {
    Ok(tokio::fs::metadata(path)
        .await
        .with_context(|| format!("Failed to read {}", path.display()))?
        .len())
}

/// Read `len` bytes at `offset` (or fewer at the end of the file).
pub async fn read_chunk(file: &mut File, offset: u64, len: usize) -> Result<Vec<u8>> {
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let mut buf = Vec::with_capacity(len);
    file.take(len as u64).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// Hex SHA-1 of a whole file, computed without loading it into memory.
pub async fn sha1_file(path: &Path) -> Result<[u8; 20]> {
    use sha1::{Digest, Sha1};
    let mut file = File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize().into())
}

pub fn sha1(data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    Sha1::digest(data).into()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Percent-encode a string for use in a URL path segment or header, keeping `/`.
pub fn encode_path(s: &str) -> String {
    s.split('/')
        .map(|seg| url::form_urlencoded::byte_serialize(seg.as_bytes()).collect::<String>())
        .collect::<Vec<_>>()
        .join("/")
        // form_urlencoded uses '+' for spaces, paths need %20
        .replace('+', "%20")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(
            encode_path("backups/a b+c.tar.zst"),
            "backups/a%20b%2Bc.tar.zst"
        );
        assert_eq!(hex(&[0, 255, 16]), "00ff10");
    }
}

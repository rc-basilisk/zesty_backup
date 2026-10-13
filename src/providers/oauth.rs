//! OAuth2 access tokens for the consumer cloud providers.
//!
//! Access tokens from Google, Microsoft and Box expire after about an hour (Dropbox: 4 hours),
//! so a static `access_key` only works for quick one-off commands. With `refresh_token` and
//! `client_id` (plus `client_secret` where the provider needs it) configured, a fresh access
//! token is fetched whenever the current one is about to expire.

use super::{http, non_empty};
use crate::config::StorageConfig;
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};

pub struct OAuthToken {
    provider: &'static str,
    refresh: Option<RefreshSettings>,
    state: Mutex<TokenState>,
}

struct RefreshSettings {
    token_url: String,
    client_id: String,
    client_secret: Option<String>,
    scope: Option<String>,
    cache: Option<PathBuf>,
}

struct TokenState {
    access_token: Option<String>,
    expires_at: Option<Instant>,
    refresh_token: Option<String>,
}

impl OAuthToken {
    /// `token_url`/`scope` describe how to refresh tokens for this provider.
    pub fn new(
        provider: &'static str,
        config: &StorageConfig,
        token_url: &str,
        scope: Option<&str>,
    ) -> Result<Self> {
        let access_token = non_empty(&config.access_key).map(str::to_string);
        let cache = non_empty(&config.token_cache).map(PathBuf::from);

        // A refresh token saved by a previous run wins: some providers rotate them.
        let refresh_token = cache
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| non_empty(&config.refresh_token).map(str::to_string));

        let refresh = match (&refresh_token, non_empty(&config.client_id)) {
            (Some(_), Some(client_id)) => Some(RefreshSettings {
                token_url: token_url.to_string(),
                client_id: client_id.to_string(),
                client_secret: non_empty(&config.client_secret).map(str::to_string),
                scope: scope.map(str::to_string),
                cache,
            }),
            (Some(_), None) => bail!(
                "{}: refresh_token is set but client_id is missing",
                provider
            ),
            _ => None,
        };

        if access_token.is_none() && refresh.is_none() {
            bail!(
                "{}: set access_key (an OAuth access token) or refresh_token + client_id",
                provider
            );
        }
        if refresh.is_none() {
            warn!(
                "{}: using a static access token; it will stop working when it expires. \
                 Configure refresh_token + client_id for unattended use.",
                provider
            );
        }

        Ok(Self {
            provider,
            refresh,
            state: Mutex::new(TokenState {
                access_token,
                // Unknown expiry: refresh right away if we can.
                expires_at: None,
                refresh_token,
            }),
        })
    }

    /// A currently valid access token.
    pub async fn get(&self) -> Result<String> {
        let mut state = self.state.lock().await;
        let Some(refresh) = &self.refresh else {
            return Ok(state.access_token.clone().unwrap_or_default());
        };
        let fresh =
            matches!(state.expires_at, Some(t) if t > Instant::now() + Duration::from_secs(120));
        if fresh {
            if let Some(token) = &state.access_token {
                return Ok(token.clone());
            }
        }

        let refresh_token = state.refresh_token.clone().unwrap_or_default();
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token),
            ("client_id", refresh.client_id.clone()),
        ];
        if let Some(secret) = &refresh.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        if let Some(scope) = &refresh.scope {
            form.push(("scope", scope.clone()));
        }
        let response = http::client()
            .post(&refresh.token_url)
            .form(&form)
            .send()
            .await
            .with_context(|| format!("{}: token refresh request failed", self.provider))?;
        let json = http::json(response, &format!("{} token refresh", self.provider)).await?;

        let access_token = json["access_token"]
            .as_str()
            .with_context(|| format!("{}: no access_token in refresh response", self.provider))?
            .to_string();
        let expires_in = json["expires_in"].as_u64().unwrap_or(3600);
        state.access_token = Some(access_token.clone());
        state.expires_at = Some(Instant::now() + Duration::from_secs(expires_in));

        if let Some(new_refresh) = json["refresh_token"].as_str() {
            if state.refresh_token.as_deref() != Some(new_refresh) {
                state.refresh_token = Some(new_refresh.to_string());
                match &refresh.cache {
                    Some(path) => {
                        if let Err(e) = write_private(path, new_refresh) {
                            warn!("{}: could not save the new refresh token to {}: {}", self.provider, path.display(), e);
                        }
                    }
                    None => warn!(
                        "{}: the provider issued a new refresh token; set storage.token_cache so it survives restarts",
                        self.provider
                    ),
                }
            }
        }
        info!("{}: access token refreshed", self.provider);
        Ok(access_token)
    }
}

fn write_private(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(content.as_bytes())?;
    file.sync_all()
}

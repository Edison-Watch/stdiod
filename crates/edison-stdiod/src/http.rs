//! Thin HTTP client for the backend's `/api/v1/...` REST surface.
//!
//! Used by the ``server add | list | remove`` subcommands. The daemon's
//! WS tunnel uses [`crate::tunnel`] directly - this module exists only
//! for the CLI HTTP surface.
//!
//! Authentication is the same bearer-token model the dashboard uses; the
//! token comes from ``config.toml`` (written by ``edison-stdiod login``).

use anyhow::{anyhow, Context, Result};
use reqwest::{Client, Response, StatusCode};
use serde::Serialize;

use crate::config::PersistedConfig;

/// Load `config.toml`, validate it has both backend + api_key, and return
/// a configured [`BackendClient`]. Surfaces a clear "run login" hint when
/// either is missing.
pub fn from_persisted() -> Result<BackendClient> {
    let cfg = PersistedConfig::load()?;
    let backend = cfg.backend_url.ok_or_else(|| {
        anyhow!("no backend URL on disk. Run `edison-stdiod login --backend ...`.")
    })?;
    let token = cfg.api_key.ok_or_else(|| {
        anyhow!("no API key on disk. Run `edison-stdiod login --api-key ...`.")
    })?;
    BackendClient::new(backend, token)
}

pub struct BackendClient {
    base: String,
    token: String,
    http: Client,
}

impl BackendClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base: base.into(),
            token: token.into(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(&self, path: &str) -> Result<T> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("GET {path}"))?;
        decode_json(resp).await
    }

    pub async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await
            .with_context(|| format!("POST {path}"))?;
        decode_json(resp).await
    }

    /// DELETE expecting 204 (No Content). Returns `Ok(false)` on 404 so
    /// callers can render "nothing to remove" instead of an error.
    pub async fn delete(&self, path: &str) -> Result<bool> {
        let resp = self
            .http
            .delete(self.url(path))
            .bearer_auth(&self.token)
            .send()
            .await
            .with_context(|| format!("DELETE {path}"))?;
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            s => {
                let body = resp.text().await.unwrap_or_default();
                Err(anyhow!("DELETE {path} -> HTTP {s}: {body}"))
            }
        }
    }
}

async fn decode_json<T: serde::de::DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("HTTP {status}: {body}"));
    }
    serde_json::from_str(&body).with_context(|| format!("decoding response body: {body}"))
}

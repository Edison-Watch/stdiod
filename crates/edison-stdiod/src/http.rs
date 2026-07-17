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
use thiserror::Error;

use crate::config::{self, PersistedConfig};

/// Build a client from an already-loaded config so callers can also branch on
/// the selected credential kind without loading the file twice.
pub fn from_config(cfg: &PersistedConfig) -> Result<BackendClient> {
    let token = cfg
        .usable_credential()
        .map(|credential| credential.token().to_string())
        .ok_or_else(|| {
            anyhow!("no credential on disk. Run `edison-stdiod login --backend ...`.")
        })?;
    let backend = cfg.backend_url.clone().ok_or_else(|| {
        anyhow!("no backend URL on disk. Run `edison-stdiod login --backend ...`.")
    })?;
    BackendClient::new(backend, token)
}

#[derive(Error)]
pub enum HttpError {
    #[error("{operation} request failed: {source}")]
    Request {
        operation: String,
        #[source]
        source: reqwest::Error,
    },
    // Do not include the response body in Display: a broken or malicious
    // backend must not be able to reflect a bearer credential into logs.
    #[error("{operation} returned HTTP {status}")]
    Status {
        operation: String,
        status: StatusCode,
        body: String,
    },
    #[error("{operation} returned invalid JSON (HTTP {status}): {source}")]
    Decode {
        operation: String,
        status: StatusCode,
        #[source]
        source: serde_json::Error,
    },
}

impl std::fmt::Debug for HttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Request { operation, source } => formatter
                .debug_struct("Request")
                .field("operation", operation)
                .field("source", source)
                .finish(),
            Self::Status {
                operation, status, ..
            } => formatter
                .debug_struct("Status")
                .field("operation", operation)
                .field("status", status)
                .field("body", &"<redacted>")
                .finish(),
            Self::Decode {
                operation,
                status,
                source,
            } => formatter
                .debug_struct("Decode")
                .field("operation", operation)
                .field("status", status)
                .field("source", source)
                .finish(),
        }
    }
}

impl HttpError {
    pub fn status(&self) -> Option<StatusCode> {
        match self {
            Self::Status { status, .. } | Self::Decode { status, .. } => Some(*status),
            Self::Request { .. } => None,
        }
    }

    pub fn is_auth_rejection(&self) -> bool {
        matches!(
            self.status(),
            Some(StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
        )
    }
}

pub struct BackendClient {
    base: String,
    token: String,
    http: Client,
}

impl BackendClient {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let base = base.into();
        let base = config::normalize_backend_url(&base)?;
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            base,
            token: token.into(),
            http,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn authorize(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request.bearer_auth(&self.token)
    }

    pub async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> std::result::Result<T, HttpError> {
        let resp = self
            .authorize(self.http.get(self.url(path)))
            .send()
            .await
            .map_err(|source| HttpError::Request {
                operation: format!("GET {path}"),
                source,
            })?;
        decode_json(resp, format!("GET {path}")).await
    }

    pub async fn post_json<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> std::result::Result<T, HttpError> {
        let resp = self
            .authorize(self.http.post(self.url(path)))
            .json(body)
            .send()
            .await
            .map_err(|source| HttpError::Request {
                operation: format!("POST {path}"),
                source,
            })?;
        decode_json(resp, format!("POST {path}")).await
    }

    /// DELETE expecting 204 (No Content). Returns `Ok(false)` on 404 so
    /// callers can render "nothing to remove" instead of an error.
    pub async fn delete(&self, path: &str) -> std::result::Result<bool, HttpError> {
        let resp = self
            .authorize(self.http.delete(self.url(path)))
            .send()
            .await
            .map_err(|source| HttpError::Request {
                operation: format!("DELETE {path}"),
                source,
            })?;
        match resp.status() {
            StatusCode::NO_CONTENT => Ok(true),
            StatusCode::NOT_FOUND => Ok(false),
            _ => Err(status_error(resp, format!("DELETE {path}")).await),
        }
    }
}

pub(crate) async fn decode_json<T: serde::de::DeserializeOwned>(
    resp: Response,
    operation: String,
) -> std::result::Result<T, HttpError> {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(HttpError::Status {
            operation,
            status,
            body,
        });
    }
    serde_json::from_str(&body).map_err(|source| HttpError::Decode {
        operation,
        status,
        source,
    })
}

pub(crate) async fn expect_success(
    resp: Response,
    operation: String,
) -> std::result::Result<(), HttpError> {
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(status_error(resp, operation).await)
    }
}

async fn status_error(resp: Response, operation: String) -> HttpError {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    HttpError::Status {
        operation,
        status,
        body,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_client_applies_bearer_credential() {
        let client = BackendClient::new("https://example.test", "client-token").unwrap();
        let request = client
            .authorize(client.http.get(client.url("/api/v1/servers")))
            .build()
            .unwrap();
        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .unwrap(),
            "Bearer client-token"
        );
    }

    #[test]
    fn typed_status_classifies_auth_rejection_without_displaying_body() {
        let error = HttpError::Status {
            operation: "GET /private".into(),
            status: StatusCode::UNAUTHORIZED,
            body: "client-token".into(),
        };
        assert!(error.is_auth_rejection());
        assert!(!error.to_string().contains("client-token"));
    }
}

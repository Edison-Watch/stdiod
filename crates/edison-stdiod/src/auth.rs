//! OAuth-style browser/device authorization with PKCE for this stdiod install.

use std::future::Future;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::config;
use crate::http::{self, HttpError};

pub const CLIENT_ID: &str = "stdiod";
pub const CLIENT_SCOPES: &[&str] = &[
    "tunnel:connect",
    "mcp_requests:create",
    "mcp_requests:read",
    "servers:self:read",
];
const SLOW_DOWN_INCREMENT: Duration = Duration::from_secs(5);
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(5);

pub struct Pkce {
    verifier: String,
    challenge: String,
}

impl Pkce {
    pub fn generate() -> Result<Self, AuthError> {
        let mut entropy = [0_u8; 32];
        getrandom::fill(&mut entropy)
            .map_err(|_| AuthError::Protocol("secure random generation failed".into()))?;
        let verifier = URL_SAFE_NO_PAD.encode(entropy);
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }

    pub fn verifier(&self) -> &str {
        &self.verifier
    }

    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

#[derive(Serialize)]
struct DeviceCodeRequest<'a> {
    client_id: &'static str,
    scope: &'static [&'static str],
    code_challenge: &'a str,
    code_challenge_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_label: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    client_installation_id: Option<&'a str>,
    platform: &'static str,
    client_version: &'static str,
}

#[derive(Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
    pub expires_in: u64,
    pub interval: u64,
}

#[derive(Serialize)]
struct DeviceTokenRequest<'a> {
    client_id: &'static str,
    device_code: &'a str,
    code_verifier: &'a str,
}

#[derive(Deserialize)]
struct DeviceTokenError {
    error: String,
}

#[derive(Deserialize)]
pub struct DeviceTokenResponse {
    pub access_token: String,
    pub token_type: String,
    pub client_installation_id: String,
    pub device_id: String,
    #[serde(deserialize_with = "deserialize_scopes")]
    pub scope: Vec<String>,
    pub user_id: String,
    pub org_id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ScopeValue {
    List(Vec<String>),
    SpaceDelimited(String),
}

fn deserialize_scopes<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(match ScopeValue::deserialize(deserializer)? {
        ScopeValue::List(scopes) => scopes,
        ScopeValue::SpaceDelimited(scopes) => scopes
            .split_ascii_whitespace()
            .map(str::to_string)
            .collect(),
    })
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("device authorization was denied")]
    AccessDenied,
    #[error("device authorization expired; run `edison-stdiod login` again")]
    Expired,
    #[error("device authorization failed with an unrecognized backend error")]
    BackendRejected,
    #[error("invalid device authorization response: {0}")]
    Protocol(String),
}

pub struct AuthClient {
    base: String,
    http: Client,
}

impl AuthClient {
    pub fn new(base: impl Into<String>) -> Result<Self, AuthError> {
        let base = config::normalize_backend_url(&base.into())
            .map_err(|_| AuthError::Protocol("backend URL was invalid".into()))?;
        // Never follow redirects: a 307/308 would re-send the POST body -
        // including the PKCE verifier and tokens - to whatever location the
        // response names, so authorization material must stay bound to the
        // configured backend.
        let http = Client::builder()
            .timeout(Duration::from_secs(20))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|source| HttpError::Request {
                operation: "building auth HTTP client".into(),
                source,
            })?;
        Ok(Self { base, http })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    pub async fn initiate(
        &self,
        code_challenge: &str,
        device_label: Option<&str>,
        client_installation_id: Option<&str>,
    ) -> Result<DeviceCodeResponse, AuthError> {
        let path = "/api/v1/auth/device/code";
        let response = self
            .http
            .post(self.url(path))
            .json(&DeviceCodeRequest {
                client_id: CLIENT_ID,
                scope: CLIENT_SCOPES,
                code_challenge,
                code_challenge_method: "S256",
                device_label,
                client_installation_id,
                platform: std::env::consts::OS,
                client_version: env!("CARGO_PKG_VERSION"),
            })
            .send()
            .await
            .map_err(|source| HttpError::Request {
                operation: format!("POST {path}"),
                source,
            })?;
        let code: DeviceCodeResponse = http::decode_json(response, format!("POST {path}")).await?;
        validate_device_code(&code)?;
        Ok(code)
    }

    pub async fn poll(
        &self,
        code: &DeviceCodeResponse,
        code_verifier: &str,
    ) -> Result<DeviceTokenResponse, AuthError> {
        self.poll_with_sleep(code, code_verifier, tokio::time::sleep)
            .await
    }

    async fn poll_with_sleep<F, Fut>(
        &self,
        code: &DeviceCodeResponse,
        code_verifier: &str,
        mut sleep: F,
    ) -> Result<DeviceTokenResponse, AuthError>
    where
        F: FnMut(Duration) -> Fut,
        Fut: Future<Output = ()>,
    {
        let deadline = Instant::now()
            .checked_add(Duration::from_secs(code.expires_in))
            .ok_or(AuthError::Expired)?;
        let mut interval = Duration::from_secs(code.interval).max(MIN_POLL_INTERVAL);
        let path = "/api/v1/auth/device/token";

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(AuthError::Expired);
            }
            let remaining = deadline.saturating_duration_since(now);
            if interval >= remaining {
                sleep(remaining).await;
                return Err(AuthError::Expired);
            }
            sleep(interval).await;

            let response = self
                .http
                .post(self.url(path))
                .json(&DeviceTokenRequest {
                    client_id: CLIENT_ID,
                    device_code: &code.device_code,
                    code_verifier,
                })
                .send()
                .await
                .map_err(|source| HttpError::Request {
                    operation: format!("POST {path}"),
                    source,
                })?;
            let status = response.status();
            if status == StatusCode::TOO_MANY_REQUESTS {
                interval = retry_after(&response)
                    .unwrap_or(interval)
                    .max(interval)
                    .max(MIN_POLL_INTERVAL);
                continue;
            }
            let body = response.text().await.unwrap_or_default();
            if Instant::now() >= deadline {
                return Err(AuthError::Expired);
            }

            if status.is_success() {
                let token: DeviceTokenResponse =
                    serde_json::from_str(&body).map_err(|source| HttpError::Decode {
                        operation: format!("POST {path}"),
                        status,
                        source,
                    })?;
                validate_token(&token)?;
                return Ok(token);
            }

            if status != StatusCode::BAD_REQUEST {
                return Err(HttpError::Status {
                    operation: format!("POST {path}"),
                    status,
                    body,
                }
                .into());
            }

            let error: DeviceTokenError =
                serde_json::from_str(&body).map_err(|source| HttpError::Decode {
                    operation: format!("POST {path}"),
                    status,
                    source,
                })?;
            match error.error.as_str() {
                "authorization_pending" => {}
                "slow_down" => interval = interval.saturating_add(SLOW_DOWN_INCREMENT),
                "access_denied" => return Err(AuthError::AccessDenied),
                "expired_token" => return Err(AuthError::Expired),
                _ => return Err(AuthError::BackendRejected),
            }
        }
    }

    /// Revocation is intentionally a separate best-effort operation at the
    /// call site: local logout must proceed when the backend is unavailable.
    pub async fn revoke(&self, token: &str) -> Result<(), HttpError> {
        let path = "/api/v1/auth/device/revoke";
        let response = self
            .http
            .post(self.url(path))
            .bearer_auth(token)
            .send()
            .await
            .map_err(|source| HttpError::Request {
                operation: format!("POST {path}"),
                source,
            })?;
        http::expect_success(response, format!("POST {path}")).await
    }
}

fn retry_after(response: &reqwest::Response) -> Option<Duration> {
    let value = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let retry_at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    (retry_at.with_timezone(&chrono::Utc) - chrono::Utc::now())
        .to_std()
        .ok()
}

fn validate_device_code(code: &DeviceCodeResponse) -> Result<(), AuthError> {
    if code.device_code.is_empty() || code.user_code.is_empty() || code.expires_in == 0 {
        return Err(AuthError::Protocol(
            "missing device code, user code, or expiry".into(),
        ));
    }
    validate_http_url(&code.verification_uri)?;
    validate_http_url(&code.verification_uri_complete)?;
    Ok(())
}

fn validate_token(token: &DeviceTokenResponse) -> Result<(), AuthError> {
    // RFC 6749 §5.1: token_type is case-insensitive.
    if !token.token_type.eq_ignore_ascii_case("bearer") {
        return Err(AuthError::Protocol("token_type was not Bearer".into()));
    }
    if token.access_token.is_empty()
        || token.client_installation_id.is_empty()
        || token.device_id.is_empty()
        || token.user_id.is_empty()
        || token.org_id.is_empty()
    {
        return Err(AuthError::Protocol(
            "token response omitted required identity fields".into(),
        ));
    }
    Ok(())
}

pub fn validate_http_url(raw: &str) -> Result<(), AuthError> {
    let parsed = url::Url::parse(raw)
        .map_err(|_| AuthError::Protocol("verification URL was invalid".into()))?;
    let loopback = match parsed.host() {
        Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        None => {
            return Err(AuthError::Protocol(
                "verification URL must include a hostname".into(),
            ));
        }
    };
    if parsed.scheme() != "https" && !(parsed.scheme() == "http" && loopback) {
        return Err(AuthError::Protocol(
            "verification URL must use https (or http on loopback)".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use sha2::{Digest, Sha256};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    #[test]
    fn pkce_has_rfc7636_shape_and_matching_s256_challenge() {
        let pkce = Pkce::generate().unwrap();
        assert_eq!(pkce.verifier().len(), 43);
        assert!(pkce
            .verifier()
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')));
        let expected = URL_SAFE_NO_PAD.encode(Sha256::digest(pkce.verifier().as_bytes()));
        assert_eq!(pkce.challenge(), expected);
        assert_eq!(pkce.challenge().len(), 43);
    }

    #[test]
    fn token_type_is_accepted_case_insensitively() {
        let mut token = DeviceTokenResponse {
            access_token: "token".into(),
            token_type: "bearer".into(),
            client_installation_id: "install-1".into(),
            device_id: "device-1".into(),
            scope: vec![],
            user_id: "user-1".into(),
            org_id: "org-1".into(),
        };
        assert!(validate_token(&token).is_ok());
        token.token_type = "BEARER".into();
        assert!(validate_token(&token).is_ok());
        token.token_type = "MAC".into();
        assert!(validate_token(&token).is_err());
    }

    #[test]
    fn verification_url_rejects_non_http_schemes() {
        assert!(validate_http_url("https://example.test/activate").is_ok());
        assert!(validate_http_url("http://localhost:3001/device").is_ok());
        assert!(validate_http_url("http://127.0.0.1:3001/device").is_ok());
        assert!(validate_http_url("http://[::1]:3001/device").is_ok());
        assert!(validate_http_url("http://example.test/activate").is_err());
        assert!(validate_http_url("javascript:alert(1)").is_err());
        assert!(validate_http_url("file:///tmp/code").is_err());
    }

    #[tokio::test]
    async fn initiation_sends_device_contract() {
        let response = r#"{
            "device_code":"device-code",
            "user_code":"ABCD-EFGH",
            "verification_uri":"https://example.test/activate",
            "verification_uri_complete":"https://example.test/activate?code=ABCD-EFGH",
            "expires_in":600,
            "interval":5
        }"#;
        let (base, bodies, server) = mock_server(vec![(200, response)]).await;
        let client = AuthClient::new(base).unwrap();
        let code = client
            .initiate("pkce-challenge", Some("Laptop"), Some("install-existing"))
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(code.user_code, "ABCD-EFGH");
        let bodies = bodies.lock().unwrap();
        let request: serde_json::Value = serde_json::from_str(&bodies[0]).unwrap();
        assert_eq!(request["client_id"], CLIENT_ID);
        assert_eq!(request["code_challenge"], "pkce-challenge");
        assert_eq!(request["code_challenge_method"], "S256");
        assert_eq!(request["device_label"], "Laptop");
        assert_eq!(request["client_installation_id"], "install-existing");
        assert_eq!(request["platform"], std::env::consts::OS);
        assert_eq!(request["client_version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(
            request["scope"],
            serde_json::json!([
                "tunnel:connect",
                "mcp_requests:create",
                "mcp_requests:read",
                "servers:self:read"
            ])
        );
    }

    #[test]
    fn initiation_omits_missing_installation_identity() {
        let request = DeviceCodeRequest {
            client_id: CLIENT_ID,
            scope: CLIENT_SCOPES,
            code_challenge: "challenge",
            code_challenge_method: "S256",
            device_label: None,
            client_installation_id: None,
            platform: "linux",
            client_version: "test",
        };
        let json = serde_json::to_value(request).unwrap();
        assert!(json.get("client_installation_id").is_none());
    }

    #[tokio::test]
    async fn polling_handles_pending_slow_down_and_success() {
        let success = r#"{
            "access_token":"opaque-client-token",
            "token_type":"Bearer",
            "client_installation_id":"install-1",
            "device_id":"device-1",
            "scope":["tunnel:connect"],
            "user_id":"user-1",
            "org_id":"org-1"
        }"#;
        let (base, bodies, server) = mock_server(vec![
            (400, r#"{"error":"authorization_pending"}"#),
            (400, r#"{"error":"slow_down"}"#),
            (200, success),
        ])
        .await;
        let client = AuthClient::new(base).unwrap();
        let code = sample_code(2, 60);
        let waits = Arc::new(Mutex::new(Vec::new()));
        let observed = waits.clone();
        let token = client
            .poll_with_sleep(&code, "pkce-verifier", move |duration| {
                observed.lock().unwrap().push(duration);
                std::future::ready(())
            })
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(token.client_installation_id, "install-1");
        assert_eq!(
            *waits.lock().unwrap(),
            vec![
                Duration::from_secs(5),
                Duration::from_secs(5),
                Duration::from_secs(10)
            ]
        );
        assert!(bodies
            .lock()
            .unwrap()
            .iter()
            .all(|body| body.contains("pkce-verifier") && body.contains("device-code")));
    }

    #[tokio::test]
    async fn polling_surfaces_denied_and_expired() {
        for (body, expected_denied) in [
            (r#"{"error":"access_denied"}"#, true),
            (r#"{"error":"expired_token"}"#, false),
        ] {
            let (base, _, server) = mock_server(vec![(400, body)]).await;
            let client = AuthClient::new(base).unwrap();
            let error = client
                .poll_with_sleep(&sample_code(0, 60), "verifier", |_| std::future::ready(()))
                .await
                .err()
                .unwrap();
            server.await.unwrap();
            assert_eq!(matches!(error, AuthError::AccessDenied), expected_denied);
            assert_eq!(matches!(error, AuthError::Expired), !expected_denied);
        }
    }

    #[tokio::test]
    async fn polling_honors_retry_after_without_decoding_the_429_body() {
        let success = r#"{
            "access_token":"opaque-client-token",
            "token_type":"Bearer",
            "client_installation_id":"install-1",
            "device_id":"device-1",
            "scope":["tunnel:connect"],
            "user_id":"user-1",
            "org_id":"org-1"
        }"#;
        let (base, _, server) = mock_server(vec![
            (429, "body containing opaque-client-token is ignored"),
            (200, success),
        ])
        .await;
        let client = AuthClient::new(base).unwrap();
        let waits = Arc::new(Mutex::new(Vec::new()));
        let observed = waits.clone();
        let token = client
            .poll_with_sleep(&sample_code(1, 60), "verifier", move |duration| {
                observed.lock().unwrap().push(duration);
                std::future::ready(())
            })
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(token.access_token, "opaque-client-token");
        assert_eq!(
            *waits.lock().unwrap(),
            vec![Duration::from_secs(5), Duration::from_secs(9)]
        );
    }

    #[tokio::test]
    async fn retry_after_cannot_reduce_a_slowed_down_interval() {
        let success = r#"{
            "access_token":"opaque-client-token",
            "token_type":"Bearer",
            "client_installation_id":"install-1",
            "device_id":"device-1",
            "scope":["tunnel:connect"],
            "user_id":"user-1",
            "org_id":"org-1"
        }"#;
        let (base, _, server) = mock_server(vec![
            (400, r#"{"error":"slow_down"}"#),
            (429, "rate limited"),
            (200, success),
        ])
        .await;
        let client = AuthClient::new(base).unwrap();
        let waits = Arc::new(Mutex::new(Vec::new()));
        let observed = waits.clone();
        client
            .poll_with_sleep(&sample_code(1, 60), "verifier", move |duration| {
                observed.lock().unwrap().push(duration);
                std::future::ready(())
            })
            .await
            .unwrap();
        server.await.unwrap();

        assert_eq!(
            *waits.lock().unwrap(),
            vec![
                Duration::from_secs(5),
                Duration::from_secs(10),
                Duration::from_secs(10)
            ]
        );
    }

    #[tokio::test]
    async fn polling_does_not_request_when_minimum_interval_reaches_expiry() {
        let client = AuthClient::new("http://127.0.0.1:1").unwrap();
        let waits = Arc::new(Mutex::new(Vec::new()));
        let observed = waits.clone();
        let result = client
            .poll_with_sleep(&sample_code(0, 5), "verifier", move |duration| {
                observed.lock().unwrap().push(duration);
                std::future::ready(())
            })
            .await;
        let error = match result {
            Ok(_) => panic!("poll unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(matches!(error, AuthError::Expired));
        assert_eq!(waits.lock().unwrap().len(), 1);
    }

    fn sample_code(interval: u64, expires_in: u64) -> DeviceCodeResponse {
        DeviceCodeResponse {
            device_code: "device-code".into(),
            user_code: "ABCD-EFGH".into(),
            verification_uri: "https://example.test/activate".into(),
            verification_uri_complete: "https://example.test/activate?code=ABCD-EFGH".into(),
            expires_in,
            interval,
        }
    }

    type MockBodies = Arc<Mutex<Vec<String>>>;

    async fn mock_server(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, MockBodies, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let captured = bodies.clone();
        let handle = tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let mut chunk = [0_u8; 1024];
                let header_end = loop {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(index) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                        break index + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0);
                    request.extend_from_slice(&chunk[..read]);
                }
                captured.lock().unwrap().push(
                    String::from_utf8_lossy(
                        &request[header_end..header_end.saturating_add(content_length)],
                    )
                    .into_owned(),
                );

                let reason = match status {
                    200 => "OK",
                    429 => "Too Many Requests",
                    _ => "Bad Request",
                };
                let retry_after = if status == 429 {
                    "Retry-After: 9\r\n"
                } else {
                    ""
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\n{retry_after}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        (format!("http://{address}"), bodies, handle)
    }
}

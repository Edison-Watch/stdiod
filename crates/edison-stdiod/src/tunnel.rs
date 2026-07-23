//! WebSocket client for the stdiod ↔ backend tunnel.
//!
//! Provides:
//! - [`connect`]: open one WS to the backend with auth headers.
//! - [`run_frame_loop`]: split-half send/receive over an open WS.
//! - [`OutgoingHandle`]: a swappable, broker-style sender that children
//!   hold for the lifetime of the daemon. The supervisor wires it to the
//!   current WS sink on every (re)connect and clears it on disconnect, so
//!   child subprocesses survive transient network blips per the
//!   architecture doc's "reconcile on every reconnect" model.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail, Context, Result};
use edison_tunnel_protocol::TunnelFrame;
use futures_util::{SinkExt, StreamExt};
use thiserror::Error;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};

use crate::config;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("WebSocket authentication rejected by backend (HTTP {status})")]
    AuthRejected { status: u16 },
    #[error("WebSocket upgrade rejected by backend (HTTP {status})")]
    UpgradeRejected { status: u16 },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl ConnectError {
    pub fn needs_reauth(&self) -> bool {
        matches!(self, Self::AuthRejected { .. })
    }
}

#[derive(Debug, Error)]
pub enum SessionCloseError {
    #[error("backend reports that the client credential was revoked")]
    ClientCredentialRevoked,
    #[error("backend requires a different tunnel protocol version: {reason}")]
    ProtocolVersionMismatch { reason: String },
    #[error("backend closed WebSocket (code {code}): {reason}")]
    Closed { code: u16, reason: String },
}

impl SessionCloseError {
    pub fn needs_reauth(&self) -> bool {
        matches!(self, Self::ClientCredentialRevoked)
    }

    pub fn needs_upgrade(&self) -> bool {
        matches!(self, Self::ProtocolVersionMismatch { .. })
    }
}

/// Stable handle to the daemon's outbound channel.
///
/// Children (and the heartbeat task) clone this once and use it for their
/// entire lifetime. The supervisor swaps the inner sender on every
/// (re)connect; sends issued while disconnected are dropped silently
/// instead of erroring - that's the right behaviour because at that
/// point the backend has already failed any in-flight requests anyway.
#[derive(Clone, Default)]
pub struct OutgoingHandle {
    inner: Arc<Mutex<Option<mpsc::Sender<TunnelFrame>>>>,
}

impl OutgoingHandle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Wire this handle to a new WS-backed sender. Called on connect.
    pub fn set(&self, tx: mpsc::Sender<TunnelFrame>) {
        *self.inner.lock().expect("OutgoingHandle mutex poisoned") = Some(tx);
    }

    /// Disconnect: drop the current sender. Subsequent sends become no-ops
    /// until [`set`] is called again.
    pub fn clear(&self) {
        *self.inner.lock().expect("OutgoingHandle mutex poisoned") = None;
    }

    fn snapshot(&self) -> Option<mpsc::Sender<TunnelFrame>> {
        self.inner
            .lock()
            .expect("OutgoingHandle mutex poisoned")
            .clone()
    }

    /// Send a frame on the current WS sender, or drop silently if
    /// disconnected. Returns whether the frame was queued.
    pub async fn send(&self, frame: TunnelFrame) -> bool {
        match self.snapshot() {
            Some(tx) => tx.send(frame).await.is_ok(),
            None => false,
        }
    }
}

/// Connect to the backend's `/api/v1/stdio-tunnel/ws` endpoint.
pub async fn connect(
    backend_url: &str,
    credential: &str,
    edison_secret_key: Option<&str>,
    device_id: &str,
) -> std::result::Result<WsStream, ConnectError> {
    let request = build_request(backend_url, credential, edison_secret_key, device_id)?;
    let ws_url = request.uri().to_string();
    info!(url = %ws_url, device_id = %device_id, "connecting to backend");

    let (ws, response) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status().as_u16();
            return Err(if matches!(status, 401 | 403) {
                ConnectError::AuthRejected { status }
            } else {
                ConnectError::UpgradeRejected { status }
            });
        }
        Err(error) => {
            return Err(ConnectError::Other(
                anyhow::Error::from(error).context("WebSocket upgrade failed"),
            ));
        }
    };
    debug!(status = %response.status(), "WS upgrade complete");
    Ok(ws)
}

fn build_request(
    backend_url: &str,
    credential: &str,
    edison_secret_key: Option<&str>,
    device_id: &str,
) -> Result<tokio_tungstenite::tungstenite::http::Request<()>> {
    let ws_url = build_ws_url(backend_url)?;
    let mut request = ws_url
        .into_client_request()
        .context("invalid backend URL")?;
    let headers = request.headers_mut();
    let mut authorization = HeaderValue::from_str(&format!("Bearer {credential}"))
        .context("invalid bearer credential for Authorization header")?;
    authorization.set_sensitive(true);
    headers.insert("Authorization", authorization);
    headers.insert(
        "X-Edison-Device-Id",
        HeaderValue::from_str(device_id).context("invalid device_id header")?,
    );
    if let Some(secret) = edison_secret_key {
        let mut secret =
            HeaderValue::from_str(secret).context("invalid Edison secret-key header")?;
        secret.set_sensitive(true);
        headers.insert("X-Edison-Secret-Key", secret);
    }
    Ok(request)
}

fn build_ws_url(backend_url: &str) -> Result<String> {
    let backend_url = config::normalize_backend_url(backend_url)?;
    let ws_base = if let Some(rest) = backend_url.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = backend_url.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        bail!("backend URL must use HTTP or HTTPS");
    };
    Ok(format!("{ws_base}/api/v1/stdio-tunnel/ws"))
}

/// Run a frame-level send loop on the WS sink and a receive loop on the
/// stream. Returns when either side closes the connection.
pub async fn run_frame_loop(
    ws: WsStream,
    mut outgoing: mpsc::Receiver<TunnelFrame>,
    incoming: mpsc::Sender<TunnelFrame>,
) -> Result<()> {
    let (mut sink, mut stream) = ws.split();

    let send_task = tokio::spawn(async move {
        while let Some(frame) = outgoing.recv().await {
            let text = match serde_json::to_string(&frame) {
                Ok(s) => s,
                Err(e) => {
                    warn!(error = %e, "frame serialise failed; skipping");
                    continue;
                }
            };
            if let Err(e) = sink.send(Message::Text(text.into())).await {
                warn!(error = %e, "WS send failed; closing sink");
                let _ = sink.close().await;
                return;
            }
        }
        let _ = sink.close().await;
    });

    // Run the receive loop inside a block so every exit path - including the
    // error returns for recv failures and reasoned closes - falls through to
    // the send-task cleanup below instead of leaking the sink task (and its
    // socket half) into the supervisor's error handling.
    let result = async {
        while let Some(msg) = stream.next().await {
            let msg = msg.context("WS recv failed")?;
            match msg {
                Message::Text(s) => {
                    let value: serde_json::Value = match serde_json::from_str(&s) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(error = %e, "WS text not JSON; dropping");
                            continue;
                        }
                    };
                    match TunnelFrame::from_json(value) {
                        Ok(frame) => {
                            if incoming.send(frame).await.is_err() {
                                debug!("incoming consumer dropped; ending recv loop");
                                break;
                            }
                        }
                        Err(e) => {
                            warn!(error = %e, "unparseable tunnel frame; dropping");
                        }
                    }
                }
                Message::Close(frame) => {
                    if let Some(frame) = frame {
                        let reason = frame.reason.to_string();
                        if !reason.is_empty() {
                            return Err(session_close_error(frame.code, reason).into());
                        }
                        info!(code = %frame.code, "backend closed WS");
                    } else {
                        info!("backend closed WS");
                    }
                    break;
                }
                Message::Ping(p) => debug!(len = p.len(), "got ping"),
                Message::Pong(_) => debug!("got pong"),
                Message::Binary(_) => debug!("ignoring binary frame"),
                Message::Frame(_) => {}
            }
        }
        Ok(())
    }
    .await;

    send_task.abort();
    let _ = send_task.await;
    result
}

fn session_close_error(code: CloseCode, reason: String) -> SessionCloseError {
    let normalized = reason.trim();
    if code == CloseCode::Policy {
        if normalized.eq_ignore_ascii_case("client credential revoked")
            || normalized.eq_ignore_ascii_case("client installation revoked")
        {
            return SessionCloseError::ClientCredentialRevoked;
        }
        if normalized
            .to_ascii_lowercase()
            .starts_with("protocol_version mismatch")
        {
            return SessionCloseError::ProtocolVersionMismatch { reason };
        }
        SessionCloseError::Closed {
            code: code.into(),
            reason,
        }
    } else {
        SessionCloseError::Closed {
            code: code.into(),
            reason,
        }
    }
}

// `anyhow` re-export so the public API can return its Result without callers
// needing the same dependency in scope.
#[allow(dead_code)]
fn _ensure_anyhow_used() -> Result<()> {
    Err(anyhow!("unused"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ws_url_translates_schemes() {
        assert_eq!(
            build_ws_url("http://localhost:8000").unwrap(),
            "ws://localhost:8000/api/v1/stdio-tunnel/ws"
        );
        assert_eq!(
            build_ws_url("HTTPS://DEMO-DASHBOARD.EDISON.WATCH/").unwrap(),
            "wss://demo-dashboard.edison.watch/api/v1/stdio-tunnel/ws"
        );
        assert!(build_ws_url("ws://127.0.0.1:9999").is_err());
        assert!(build_ws_url("plain.example.com").is_err());
    }

    #[test]
    fn request_uses_client_credential_and_marks_secrets_sensitive() {
        let request = build_request(
            "https://example.test",
            "opaque-client-token",
            Some("edison-secret"),
            "device-1",
        )
        .unwrap();
        let authorization = request.headers().get("Authorization").unwrap();
        assert_eq!(authorization, "Bearer opaque-client-token");
        assert!(authorization.is_sensitive());
        assert!(request
            .headers()
            .get("X-Edison-Secret-Key")
            .unwrap()
            .is_sensitive());
    }

    #[test]
    fn upgrade_auth_statuses_are_classified_for_reauthentication() {
        assert!(ConnectError::AuthRejected { status: 401 }.needs_reauth());
        assert!(ConnectError::AuthRejected { status: 403 }.needs_reauth());
        assert!(!ConnectError::UpgradeRejected { status: 500 }.needs_reauth());
    }

    #[test]
    fn only_specific_policy_close_is_classified_as_reauthentication() {
        let revoked =
            session_close_error(CloseCode::Policy, "client credential revoked".to_string());
        let installation_revoked =
            session_close_error(CloseCode::Policy, "client installation revoked".to_string());
        let other_policy =
            session_close_error(CloseCode::Policy, "organization policy changed".to_string());
        let same_reason_wrong_code =
            session_close_error(CloseCode::Normal, "client credential revoked".to_string());
        assert!(revoked.needs_reauth());
        assert!(installation_revoked.needs_reauth());
        assert!(!other_policy.needs_reauth());
        assert!(!same_reason_wrong_code.needs_reauth());
    }

    #[test]
    fn protocol_mismatch_requires_an_upgrade() {
        let mismatch = session_close_error(
            CloseCode::Policy,
            "protocol_version mismatch (server=1, client=0)".to_string(),
        );
        assert!(mismatch.needs_upgrade());
        assert!(!mismatch.needs_reauth());
    }
}

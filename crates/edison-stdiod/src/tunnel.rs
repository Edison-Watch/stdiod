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
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, info, warn};
use tunnel_protocol::TunnelFrame;

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

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
    api_key: &str,
    edison_secret_key: Option<&str>,
    device_id: &str,
) -> Result<WsStream> {
    let ws_url = build_ws_url(backend_url)?;
    info!(url = %ws_url, device_id = %device_id, "connecting to backend");

    let mut request = ws_url
        .into_client_request()
        .context("invalid backend URL")?;
    let headers = request.headers_mut();
    headers.insert(
        "Authorization",
        HeaderValue::from_str(&format!("Bearer {api_key}"))
            .context("invalid api_key for Authorization header")?,
    );
    headers.insert(
        "X-Edison-Device-Id",
        HeaderValue::from_str(device_id).context("invalid device_id header")?,
    );
    if let Some(secret) = edison_secret_key {
        headers.insert(
            "X-Edison-Secret-Key",
            HeaderValue::from_str(secret).context("invalid secret-key header")?,
        );
    }

    let (ws, response) = match tokio_tungstenite::connect_async(request).await {
        Ok(ok) => ok,
        Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => {
            let status = resp.status();
            let body = resp
                .into_body()
                .map(|b| String::from_utf8_lossy(&b).to_string())
                .unwrap_or_default();
            anyhow::bail!(
                "WebSocket upgrade failed: HTTP {} - {}",
                status,
                if body.is_empty() { "(no body)".into() } else { body }
            );
        }
        Err(e) => return Err(anyhow::Error::from(e).context("WebSocket upgrade failed")),
    };
    debug!(status = %response.status(), "WS upgrade complete");
    Ok(ws)
}

fn build_ws_url(backend_url: &str) -> Result<String> {
    let trimmed = backend_url.trim_end_matches('/');
    let ws_base = if let Some(rest) = trimmed.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        format!("ws://{rest}")
    } else if trimmed.starts_with("ws://") || trimmed.starts_with("wss://") {
        trimmed.to_string()
    } else {
        bail!("backend URL must start with http(s):// or ws(s)://, got `{backend_url}`");
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
            if let Err(e) = sink.send(Message::Text(text)).await {
                warn!(error = %e, "WS send failed; closing sink");
                let _ = sink.close().await;
                return;
            }
        }
        let _ = sink.close().await;
    });

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
            Message::Close(_) => {
                info!("backend closed WS");
                break;
            }
            Message::Ping(p) => debug!(len = p.len(), "got ping"),
            Message::Pong(_) => debug!("got pong"),
            Message::Binary(_) => debug!("ignoring binary frame"),
            Message::Frame(_) => {}
        }
    }

    send_task.abort();
    let _ = send_task.await;
    Ok(())
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
            build_ws_url("https://demo-dashboard.edison.watch/").unwrap(),
            "wss://demo-dashboard.edison.watch/api/v1/stdio-tunnel/ws"
        );
        assert_eq!(
            build_ws_url("ws://127.0.0.1:9999").unwrap(),
            "ws://127.0.0.1:9999/api/v1/stdio-tunnel/ws"
        );
        assert!(build_ws_url("plain.example.com").is_err());
    }
}

// `anyhow` re-export so the public API can return its Result without callers
// needing the same dependency in scope.
#[allow(dead_code)]
fn _ensure_anyhow_used() -> Result<()> {
    Err(anyhow!("unused"))
}

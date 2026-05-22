//! Subprocess management for stdio MCP servers.
//!
//! Each [`ChildServer`] owns a child process plus two pump tasks:
//! - **outbound** pump: receives [`McpFrame`]s addressed to this server and
//!   writes the JSON-RPC body (with a trailing newline) to the child's
//!   stdin.
//! - **inbound** pump: reads newline-delimited JSON-RPC frames from the
//!   child's stdout and sends them as [`TunnelFrame::McpFrame`] over the
//!   tunnel.
//!
//! When the child exits or the inbound pump errors, the inbound pump emits a
//! `tunnel_error { code: "server_offline" }` so the backend's transport
//! fails in-flight requests cleanly - this is the load-bearing pattern
//! surfaced by the v0 spike (see `stdiod/ARCHITECTURE.md`).

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use tunnel_protocol::{DesiredServer, McpFrame, TunnelError, TunnelFrame};

use crate::tunnel::OutgoingHandle;

/// One running child stdio MCP server.
pub struct ChildServer {
    /// Logical id matching `desired.server_id`. Useful for logging/diagnostics
    /// even though the supervisor keys children by id externally.
    #[allow(dead_code)]
    pub server_id: String,
    /// The full spec we spawned this child with. ``apply_delta`` compares
    /// against an incoming ``DesiredServer`` to decide whether a
    /// kill+respawn is actually required - see [`DesiredServer::matches`].
    pub spec: DesiredServer,
    pub child: Child,
    pub outbound_tx: mpsc::Sender<serde_json::Value>,
    pub stdin_pump: JoinHandle<()>,
    pub stdout_pump: JoinHandle<()>,
}

impl ChildServer {
    /// Spawn the subprocess and start the two pumps.
    ///
    /// `tunnel_outgoing` is the broker handle the inbound pump uses to send
    /// frames upstream to the backend. It survives WS reconnects - sends
    /// during a disconnect drop silently.
    pub fn spawn(
        desired: &DesiredServer,
        tunnel_outgoing: OutgoingHandle,
    ) -> Result<Self> {
        info!(
            server_id = %desired.server_id,
            command = %desired.command,
            "spawning stdio MCP subprocess",
        );

        let mut cmd = Command::new(&desired.command);
        cmd.args(&desired.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        // Start with current env, then layer the configured env values.
        for (k, v) in &desired.env {
            cmd.env(k, v);
        }
        if let Some(wd) = &desired.working_dir {
            cmd.current_dir(wd);
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", desired.command))?;

        let stdin = child
            .stdin
            .take()
            .context("child stdin not captured")?;
        let stdout = child
            .stdout
            .take()
            .context("child stdout not captured")?;
        let stderr = child
            .stderr
            .take()
            .context("child stderr not captured")?;

        let (outbound_tx, outbound_rx) = mpsc::channel::<serde_json::Value>(64);
        let stdin_pump = tokio::spawn(stdin_pump(desired.server_id.clone(), stdin, outbound_rx));
        let stdout_pump = tokio::spawn(stdout_pump(
            desired.server_id.clone(),
            stdout,
            tunnel_outgoing,
        ));

        // Drain stderr into our log so child diagnostics aren't lost.
        let server_id = desired.server_id.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                debug!(server_id = %server_id, "[child stderr] {}", line);
            }
        });

        Ok(Self {
            server_id: desired.server_id.clone(),
            spec: desired.clone(),
            child,
            outbound_tx,
            stdin_pump,
            stdout_pump,
        })
    }

    /// Kill the child and abort the pumps.
    pub async fn shutdown(mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.stdin_pump.abort();
        self.stdout_pump.abort();
    }
}

async fn stdin_pump(
    server_id: String,
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::Receiver<serde_json::Value>,
) {
    while let Some(body) = rx.recv().await {
        let mut line = match serde_json::to_vec(&body) {
            Ok(v) => v,
            Err(e) => {
                warn!(server_id = %server_id, error = %e, "skipping unserialisable JSON-RPC frame");
                continue;
            }
        };
        line.push(b'\n');
        if let Err(e) = stdin.write_all(&line).await {
            warn!(server_id = %server_id, error = %e, "stdin write failed; ending pump");
            return;
        }
        if let Err(e) = stdin.flush().await {
            warn!(server_id = %server_id, error = %e, "stdin flush failed; ending pump");
            return;
        }
    }
}

async fn stdout_pump(
    server_id: String,
    stdout: tokio::process::ChildStdout,
    tunnel_outgoing: OutgoingHandle,
) {
    let mut reader = BufReader::new(stdout).lines();
    loop {
        match reader.next_line().await {
            Ok(Some(line)) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<serde_json::Value>(trimmed) {
                    Ok(value) => {
                        let frame = TunnelFrame::McpFrame(McpFrame {
                            server_id: server_id.clone(),
                            frame: value,
                        });
                        // ``send`` is a no-op when disconnected; we keep
                        // reading so reconnect doesn't drop us.
                        tunnel_outgoing.send(frame).await;
                    }
                    Err(e) => {
                        warn!(
                            server_id = %server_id,
                            error = %e,
                            "child emitted non-JSON line; skipping",
                        );
                    }
                }
            }
            Ok(None) => break, // EOF - child exited.
            Err(e) => {
                warn!(server_id = %server_id, error = %e, "stdout read failed; ending pump");
                break;
            }
        }
    }

    // Load-bearing per v0 spike + ARCHITECTURE.md: tell the backend that
    // this server's subprocess is gone so any in-flight tool calls fail
    // cleanly instead of hanging.
    tunnel_outgoing
        .send(TunnelFrame::TunnelError(TunnelError {
            server_id: Some(server_id.clone()),
            related_jsonrpc_id: None,
            code: "server_offline".into(),
            message: "stdio subprocess exited".into(),
        }))
        .await;
    info!(server_id = %server_id, "child stdout pump ended");
}

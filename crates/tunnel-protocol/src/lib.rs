//! Wire types for the stdiod tunnel.
//!
//! Hand-written to match `stdiod/schema/tunnel-protocol.json`. The schema is
//! the source of truth; once codegen lands these will be generated. The
//! Python equivalent lives at `src/stdio_tunnel/protocol.py` and must stay in
//! lock-step.
//!
//! The envelope is intentionally **symmetric** and **opaque**: both sides
//! exchange the same [`TunnelFrame`] variants, and [`McpFrame::frame`] is a
//! raw JSON-RPC body that the transport layer never inspects.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Macos,
    Linux,
    Windows,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub device_id: String,
    pub hostname: String,
    pub label: String,
    pub os: Os,
    pub client_version: String,
    pub currently_running: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DesiredServer {
    pub server_id: String,
    pub name: String,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub working_dir: Option<String>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_version: u32,
    pub servers: Vec<DesiredServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DesiredStateUpdate {
    #[serde(default)]
    pub added: Vec<DesiredServer>,
    #[serde(default)]
    pub updated: Vec<DesiredServer>,
    #[serde(default)]
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpFrame {
    pub server_id: String,
    pub frame: serde_json::Value,
}

/// daemon → backend: outcome of a spawn attempt for one server.
///
/// Lets the backend gate the HTTP create/update response on a real spawn
/// result instead of fire-and-forget. A later in-flight crash still
/// surfaces as the existing `tunnel_error { code: "server_offline" }`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerSpawnResult {
    pub server_id: String,
    pub ok: bool,
    #[serde(default)]
    pub error: Option<String>,
}

/// backend → daemon: env values for one stdio server, written to the
/// daemon's local `env_store` and applied on next spawn.
///
/// Sent only when the dashboard creates or updates a server; the
/// steady-state `DesiredStateUpdate` push never re-sends env so the
/// backend doesn't hold secrets at rest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEnvUpdate {
    pub server_id: String,
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TunnelError {
    #[serde(default)]
    pub server_id: Option<String>,
    #[serde(default)]
    pub related_jsonrpc_id: Option<serde_json::Value>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Ping;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Pong;

/// Symmetric, discriminated tunnel frame.
///
/// Tagged with `type` exactly matching the JSON Schema discriminator. The
/// Python side uses the same tag values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TunnelFrame {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    DesiredStateUpdate(DesiredStateUpdate),
    McpFrame(McpFrame),
    TunnelError(TunnelError),
    ServerEnvUpdate(ServerEnvUpdate),
    ServerSpawnResult(ServerSpawnResult),
    Ping(Ping),
    Pong(Pong),
}

impl TunnelFrame {
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).expect("TunnelFrame is always serializable")
    }

    pub fn from_json(value: serde_json::Value) -> Result<Self, serde_json::Error> {
        serde_json::from_value(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_frame_roundtrip() {
        let frame = TunnelFrame::McpFrame(McpFrame {
            server_id: "stub".into(),
            frame: serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        });
        let json = frame.to_json();
        assert_eq!(json["type"], "mcp_frame");
        assert_eq!(json["server_id"], "stub");

        let parsed = TunnelFrame::from_json(json).unwrap();
        assert!(matches!(parsed, TunnelFrame::McpFrame(_)));
    }

    #[test]
    fn client_hello_serialises_with_snake_case_type() {
        let frame = TunnelFrame::ClientHello(ClientHello {
            protocol_version: 1,
            device_id: "d1".into(),
            hostname: "h".into(),
            label: "l".into(),
            os: Os::Macos,
            client_version: "0.0.1".into(),
            currently_running: vec![],
        });
        let json = frame.to_json();
        assert_eq!(json["type"], "client_hello");
        assert_eq!(json["os"], "macos");
    }

    #[test]
    fn server_env_update_roundtrip() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("A".to_string(), "1".to_string());
        env.insert("B".to_string(), "2".to_string());
        let frame = TunnelFrame::ServerEnvUpdate(ServerEnvUpdate {
            server_id: "srv-1".into(),
            env,
        });
        let json = frame.to_json();
        assert_eq!(json["type"], "server_env_update");
        assert_eq!(json["server_id"], "srv-1");
        assert_eq!(json["env"]["A"], "1");

        let parsed = TunnelFrame::from_json(json).unwrap();
        match parsed {
            TunnelFrame::ServerEnvUpdate(u) => {
                assert_eq!(u.server_id, "srv-1");
                assert_eq!(u.env.get("B").map(String::as_str), Some("2"));
            }
            _ => panic!("expected server_env_update"),
        }
    }

    #[test]
    fn server_spawn_result_success_roundtrip() {
        let frame = TunnelFrame::ServerSpawnResult(ServerSpawnResult {
            server_id: "srv-1".into(),
            ok: true,
            error: None,
        });
        let json = frame.to_json();
        assert_eq!(json["type"], "server_spawn_result");
        assert_eq!(json["ok"], true);
        let parsed = TunnelFrame::from_json(json).unwrap();
        match parsed {
            TunnelFrame::ServerSpawnResult(r) => {
                assert!(r.ok);
                assert!(r.error.is_none());
            }
            _ => panic!("expected server_spawn_result"),
        }
    }

    #[test]
    fn server_spawn_result_failure_roundtrip() {
        let raw = serde_json::json!({
            "type": "server_spawn_result",
            "server_id": "srv-1",
            "ok": false,
            "error": "ENOENT: missing binary",
        });
        let parsed = TunnelFrame::from_json(raw).unwrap();
        match parsed {
            TunnelFrame::ServerSpawnResult(r) => {
                assert!(!r.ok);
                assert_eq!(r.error.as_deref(), Some("ENOENT: missing binary"));
            }
            _ => panic!("expected server_spawn_result"),
        }
    }

    #[test]
    fn server_env_update_empty_env_is_valid() {
        let raw = serde_json::json!({
            "type": "server_env_update",
            "server_id": "srv-1",
            "env": {},
        });
        let parsed = TunnelFrame::from_json(raw).unwrap();
        match parsed {
            TunnelFrame::ServerEnvUpdate(u) => assert!(u.env.is_empty()),
            _ => panic!("expected server_env_update"),
        }
    }

    #[test]
    fn tunnel_error_optional_fields() {
        let raw = serde_json::json!({
            "type": "tunnel_error",
            "code": "server_offline",
            "message": "subprocess exited",
        });
        let parsed = TunnelFrame::from_json(raw).unwrap();
        match parsed {
            TunnelFrame::TunnelError(e) => {
                assert_eq!(e.code, "server_offline");
                assert!(e.server_id.is_none());
            }
            _ => panic!("expected tunnel_error"),
        }
    }
}

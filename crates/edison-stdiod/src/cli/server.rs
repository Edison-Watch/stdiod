//! `edison-stdiod server add | list | remove` - manage stdio_tunnel
//! servers from the CLI without opening the dashboard.
//!
//! Each subcommand talks to the same `/api/v1/servers` endpoints the
//! dashboard uses. The CLI does **not** go through the daemon's WS
//! tunnel for these calls: that path is reserved for `mcp_frame` traffic
//! once a server is mounted. CRUD lives on the HTTP API so the same
//! validation (collision check, feature flag, etc.) applies whether the
//! request originates from the dashboard, the CLI, or a script.

use anyhow::Result;
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::config::PersistedConfig;
use crate::http;

#[derive(Debug, Args)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Register a new stdio_tunnel server with the backend. The server
    /// will be mounted via this daemon and proxied through the user's
    /// gateway. The server's MCP prefix must be alphanumeric.
    Add(AddArgs),
    /// List all stdio_tunnel servers currently registered for this
    /// device (filtered to the device_id from config.toml).
    List(ListArgs),
    /// Delete a server by name. Idempotent: re-running for a missing
    /// name reports it as a no-op.
    Remove(RemoveArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// MCP prefix name. Must be alphanumeric (plus hyphens). Tool calls
    /// will appear in the gateway as ``<name>_<tool>``.
    pub name: String,
    /// Executable to spawn on this device.
    #[arg(long)]
    pub command: String,
    /// Arguments passed to the executable.
    #[arg(long = "arg", value_name = "ARG", num_args = 0..)]
    pub args: Vec<String>,
    /// Working directory for the subprocess (defaults to the daemon's
    /// cwd, typically `$HOME`).
    #[arg(long)]
    pub working_dir: Option<String>,
    /// Optional human-readable display name shown in the dashboard.
    /// Defaults to the prefix name.
    #[arg(long)]
    pub display_name: Option<String>,
}

#[derive(Debug, Args)]
pub struct ListArgs {
    /// Print raw JSON instead of the formatted table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Args)]
pub struct RemoveArgs {
    pub name: String,
}

pub async fn run(args: ServerArgs) -> Result<()> {
    match args.command {
        ServerCommand::Add(a) => add(a).await,
        ServerCommand::List(a) => list(a).await,
        ServerCommand::Remove(a) => remove(a).await,
    }
}

// ---------------------------------------------------------------------------
// add
// ---------------------------------------------------------------------------
//
// Mirrors the dashboard's CreateServerRequest body (see
// src/api/v1/schemas/servers.py:167 `CreateServerRequest`). Only the
// fields stdio_tunnel callers need are sent; the backend defaults the
// rest.

#[derive(Debug, Serialize)]
struct CreateServerBody {
    name: String,
    display_name: Option<String>,
    transport_type: &'static str,
    command: String,
    args: Vec<String>,
    device_id: String,
    url: &'static str,
    enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    working_dir: Option<String>,
}

async fn add(args: AddArgs) -> Result<()> {
    let cfg = PersistedConfig::load()?;
    let device_id = cfg
        .device_id
        .clone()
        .unwrap_or_else(crate::config::hostname);
    let client = http::from_persisted()?;
    let body = CreateServerBody {
        name: args.name.clone(),
        display_name: args.display_name,
        transport_type: "stdio_tunnel",
        command: args.command,
        args: args.args,
        device_id: device_id.clone(),
        url: "",
        enabled: true,
        working_dir: args.working_dir,
    };
    let resp: serde_json::Value = client.post_json("/api/v1/servers", &body).await?;
    let name = resp
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or(&args.name);
    println!("Registered {} on device {}", name, device_id);
    println!("Run `edison-stdiod status` to confirm it has spawned.");
    Ok(())
}

// ---------------------------------------------------------------------------
// list
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ListServersResponse {
    #[serde(default)]
    items: Vec<ServerListItem>,
}

#[derive(Debug, Deserialize)]
struct ServerListItem {
    name: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    tool_count: u32,
    #[serde(default)]
    transport_type: Option<String>,
    #[serde(default)]
    device_id: Option<String>,
}

async fn list(args: ListArgs) -> Result<()> {
    let cfg = PersistedConfig::load()?;
    let device_id = cfg
        .device_id
        .clone()
        .unwrap_or_else(crate::config::hostname);
    let client = http::from_persisted()?;
    // The backend returns every server in the org; filter to this
    // device's stdio_tunnel rows on the client side. Per-device server
    // listing endpoint is a v1.1 nice-to-have.
    // The backend caps ``per_page`` at 200. Listing more than that from a
    // single device would already be unusual; pagination is a v1.1 item.
    let resp: ListServersResponse = client
        .get_json("/api/v1/servers?page=1&per_page=200")
        .await?;
    let mut filtered: Vec<&ServerListItem> = resp
        .items
        .iter()
        .filter(|it| {
            it.transport_type.as_deref() == Some("stdio_tunnel")
                && it.device_id.as_deref() == Some(device_id.as_str())
        })
        .collect();
    filtered.sort_by(|a, b| a.name.cmp(&b.name));

    if args.json {
        let json = serde_json::to_string_pretty(&filtered.iter().map(|it| {
            serde_json::json!({
                "name": it.name,
                "display_name": it.display_name,
                "enabled": it.enabled,
                "tool_count": it.tool_count,
                "device_id": it.device_id,
            })
        }).collect::<Vec<_>>())?;
        println!("{json}");
        return Ok(());
    }

    if filtered.is_empty() {
        println!("No stdio_tunnel servers registered for device {device_id}.");
        return Ok(());
    }

    println!("{:<24} {:<8} {:<6} display", "name", "enabled", "tools");
    println!("{:<24} {:<8} {:<6} -------", "----", "-------", "-----");
    for it in filtered {
        println!(
            "{:<24} {:<8} {:<6} {}",
            it.name,
            it.enabled,
            it.tool_count,
            it.display_name.as_deref().unwrap_or(""),
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// remove
// ---------------------------------------------------------------------------

async fn remove(args: RemoveArgs) -> Result<()> {
    let client = http::from_persisted()?;
    let path = format!("/api/v1/servers/{}", args.name);
    if client.delete(&path).await? {
        println!("Removed {}", args.name);
    } else {
        println!("No server named '{}' (already absent).", args.name);
    }
    Ok(())
}

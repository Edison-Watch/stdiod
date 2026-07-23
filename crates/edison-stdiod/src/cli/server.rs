//! `edison-stdiod server add | list | remove` server actions.
//!
//! Browser-auth client credentials use the narrow `/api/v1/client/...`
//! surface. Deprecated API keys use the existing user request surface for add
//! and retain broad list/remove compatibility where those operations are valid.

use std::fmt::Write as _;

use anyhow::{anyhow, bail, Result};
use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};

use crate::config::{CredentialKind, PersistedConfig};
use crate::http::{self, BackendClient};

#[derive(Debug, Args)]
pub struct ServerArgs {
    #[command(subcommand)]
    pub command: ServerCommand,
}

#[derive(Debug, Subcommand)]
pub enum ServerCommand {
    /// Submit a local stdio MCP server for approval.
    Add(AddArgs),
    /// List approved stdio_tunnel servers for this client device.
    List(ListArgs),
    /// Withdraw a pending request by name. With a legacy API key, delete the
    /// approved server directly as before.
    Remove(RemoveArgs),
}

#[derive(Debug, Args)]
pub struct AddArgs {
    /// MCP prefix name. Must be alphanumeric (plus hyphens). Tool calls
    /// will appear in the gateway as `<name>_<tool>`.
    pub name: String,
    /// Executable to spawn on this device.
    #[arg(long)]
    pub command: String,
    /// Arguments passed to the executable.
    #[arg(long = "arg", value_name = "ARG", num_args = 0..)]
    pub args: Vec<String>,
    /// Working directory for the subprocess. MCP requests cannot persist this
    /// field, so this option is currently rejected.
    #[arg(long)]
    pub working_dir: Option<String>,
    /// Optional human-readable display name shown in the dashboard.
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
    let cfg = PersistedConfig::load()?;
    let client = http::from_config(&cfg)?;
    let output = match args.command {
        ServerCommand::Add(args) => add_with(args, &cfg, &client).await?,
        ServerCommand::List(args) => list_with(args, &cfg, &client).await?,
        ServerCommand::Remove(args) => remove_with(args, &cfg, &client).await?,
    };
    println!("{output}");
    Ok(())
}

fn credential_kind(cfg: &PersistedConfig) -> Result<CredentialKind> {
    cfg.usable_credential()
        .map(|credential| credential.kind())
        .ok_or_else(|| anyhow!("no credential on disk. Run `edison-stdiod login --backend ...`."))
}

#[derive(Debug, Serialize)]
struct ClientCreateRequestBody {
    name: String,
    display_name: Option<String>,
    command: String,
    args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    hostname: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ClientCreateRequestResponse {
    request_id: u64,
    #[serde(default)]
    auto_approved: bool,
}

async fn add_with(args: AddArgs, cfg: &PersistedConfig, client: &BackendClient) -> Result<String> {
    let AddArgs {
        name,
        command,
        args,
        working_dir,
        display_name,
    } = args;
    if working_dir.is_some() {
        bail!(
            "--working-dir is not supported because MCP requests cannot persist it; omit the option or configure it through the dashboard"
        );
    }

    match credential_kind(cfg)? {
        CredentialKind::ClientAccessToken => {
            let body = ClientCreateRequestBody {
                name: name.clone(),
                display_name,
                command,
                args,
                hostname: None,
            };
            let response: ClientCreateRequestResponse = client
                .post_json("/api/v1/client/mcp-requests", &body)
                .await?;
            Ok(format_create_result(&name, response))
        }
        CredentialKind::LegacyApiKey => {
            let body = ClientCreateRequestBody {
                name: name.clone(),
                display_name,
                command,
                args,
                hostname: Some(crate::config::hostname()),
            };
            let response: ClientCreateRequestResponse =
                client.post_json("/api/v1/mcp-requests", &body).await?;
            Ok(format_create_result(&name, response))
        }
    }
}

fn format_create_result(name: &str, response: ClientCreateRequestResponse) -> String {
    if response.auto_approved {
        format!(
            "Request '{name}' was auto-approved (request ID {}). Run `edison-stdiod status` to confirm it has spawned.",
            response.request_id
        )
    } else {
        format!(
            "Submitted request '{name}' for approval (request ID {}).",
            response.request_id
        )
    }
}

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

async fn list_with(
    args: ListArgs,
    cfg: &PersistedConfig,
    client: &BackendClient,
) -> Result<String> {
    let kind = credential_kind(cfg)?;
    let mut items = match kind {
        CredentialKind::ClientAccessToken => {
            client
                .get_json::<ListServersResponse>("/api/v1/client/servers")
                .await?
                .items
        }
        CredentialKind::LegacyApiKey => {
            let device_id = cfg
                .device_id
                .clone()
                .unwrap_or_else(crate::config::hostname);
            let mut items = client
                .get_json::<ListServersResponse>("/api/v1/servers?page=1&per_page=200")
                .await?
                .items;
            items.retain(|item| {
                item.transport_type.as_deref() == Some("stdio_tunnel")
                    && item.device_id.as_deref() == Some(device_id.as_str())
            });
            items
        }
    };
    items.sort_by(|left, right| left.name.cmp(&right.name));

    if args.json {
        return Ok(serde_json::to_string_pretty(
            &items
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "name": item.name,
                        "display_name": item.display_name,
                        "enabled": item.enabled,
                        "tool_count": item.tool_count,
                        "device_id": item.device_id,
                    })
                })
                .collect::<Vec<_>>(),
        )?);
    }

    if items.is_empty() {
        return Ok(match kind {
            CredentialKind::ClientAccessToken => {
                "No approved stdio_tunnel servers registered for this client device.".into()
            }
            CredentialKind::LegacyApiKey => format!(
                "No stdio_tunnel servers registered for device {}.",
                cfg.device_id
                    .clone()
                    .unwrap_or_else(crate::config::hostname)
            ),
        });
    }

    let mut output = String::new();
    writeln!(
        output,
        "{:<24} {:<8} {:<6} display",
        "name", "enabled", "tools"
    )?;
    writeln!(
        output,
        "{:<24} {:<8} {:<6} -------",
        "----", "-------", "-----"
    )?;
    for item in items {
        writeln!(
            output,
            "{:<24} {:<8} {:<6} {}",
            item.name,
            item.enabled,
            item.tool_count,
            item.display_name.as_deref().unwrap_or("")
        )?;
    }
    Ok(output.trim_end().to_string())
}

async fn remove_with(
    args: RemoveArgs,
    cfg: &PersistedConfig,
    client: &BackendClient,
) -> Result<String> {
    let encoded_name: String = url::form_urlencoded::byte_serialize(args.name.as_bytes()).collect();
    match credential_kind(cfg)? {
        CredentialKind::ClientAccessToken => {
            let path = format!("/api/v1/client/mcp-requests/{encoded_name}");
            if client.delete(&path).await? {
                Ok(format!("Withdrew pending request '{}'.", args.name))
            } else {
                Ok(format!(
                    "No pending request named '{}'. Approved server removal requires dashboard/admin action.",
                    args.name
                ))
            }
        }
        CredentialKind::LegacyApiKey => {
            let path = format!("/api/v1/servers/{encoded_name}");
            if client.delete(&path).await? {
                Ok(format!("Removed {}", args.name))
            } else {
                Ok(format!("No server named '{}' (already absent).", args.name))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    async fn mock_backend(
        responses: Vec<(u16, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let mut requests = Vec::new();
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                let header_end = loop {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "connection closed before request headers");
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(position) = request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
                        break position + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(str::trim)
                            .and_then(|value| value.parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                while request.len() < header_end + content_length {
                    let mut chunk = [0_u8; 4096];
                    let read = stream.read(&mut chunk).await.unwrap();
                    assert!(read > 0, "connection closed before request body");
                    request.extend_from_slice(&chunk[..read]);
                }
                requests.push(String::from_utf8(request).unwrap());

                let reason = if status == 204 {
                    "No Content"
                } else if status == 404 {
                    "Not Found"
                } else {
                    "OK"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            }
            requests
        });
        (format!("http://{address}"), task)
    }

    fn client_config(backend_url: String) -> PersistedConfig {
        PersistedConfig {
            backend_url: Some(backend_url),
            client_access_token: Some("client-token".into()),
            device_id: Some("configured-device".into()),
            ..Default::default()
        }
    }

    fn legacy_config(backend_url: String) -> PersistedConfig {
        PersistedConfig {
            backend_url: Some(backend_url),
            api_key: Some("legacy-key".into()),
            device_id: Some("legacy-device".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn client_add_uses_request_endpoint_and_reports_request_id() {
        let (base, requests) = mock_backend(vec![(
            201,
            r#"{"status":"success","message":"queued","request_id":42,"auto_approved":false}"#,
        )])
        .await;
        let cfg = client_config(base.clone());
        let client = BackendClient::new(base, "client-token").unwrap();

        let output = add_with(
            AddArgs {
                name: "filesystem".into(),
                command: "npx".into(),
                args: vec!["-y".into(), "server-filesystem".into()],
                working_dir: None,
                display_name: Some("Filesystem".into()),
            },
            &cfg,
            &client,
        )
        .await
        .unwrap();

        assert_eq!(
            output,
            "Submitted request 'filesystem' for approval (request ID 42)."
        );
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("POST /api/v1/client/mcp-requests HTTP/1.1"));
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["name"], "filesystem");
        assert_eq!(body["command"], "npx");
        assert!(body.get("device_id").is_none());
    }

    #[tokio::test]
    async fn client_add_rejects_working_dir_before_http() {
        let cfg = client_config("http://127.0.0.1:1".into());
        let client = BackendClient::new("http://127.0.0.1:1", "client-token").unwrap();

        let error = add_with(
            AddArgs {
                name: "demo".into(),
                command: "npx".into(),
                args: Vec::new(),
                working_dir: Some("/tmp".into()),
                display_name: None,
            },
            &cfg,
            &client,
        )
        .await
        .unwrap_err();

        assert!(error.to_string().contains("cannot persist it"));
    }

    #[tokio::test]
    async fn client_list_uses_scoped_endpoint_without_local_filtering() {
        let (base, requests) = mock_backend(vec![(
            200,
            r#"{"items":[{"name":"remote-id","display_name":"Remote","enabled":true,"tool_count":3,"device_id":"backend-device"}]}"#,
        )])
        .await;
        let cfg = client_config(base.clone());
        let client = BackendClient::new(base, "client-token").unwrap();

        let output = list_with(ListArgs { json: false }, &cfg, &client)
            .await
            .unwrap();

        assert!(output.contains("remote-id"));
        assert!(output.contains("Remote"));
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("GET /api/v1/client/servers HTTP/1.1"));
    }

    #[tokio::test]
    async fn client_remove_404_explains_approved_server_policy() {
        let (base, requests) = mock_backend(vec![(404, r#"{"detail":"not found"}"#)]).await;
        let cfg = client_config(base.clone());
        let client = BackendClient::new(base, "client-token").unwrap();

        let output = remove_with(
            RemoveArgs {
                name: "filesystem".into(),
            },
            &cfg,
            &client,
        )
        .await
        .unwrap();

        assert!(output.contains("Approved server removal requires dashboard/admin action"));
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("DELETE /api/v1/client/mcp-requests/filesystem HTTP/1.1"));
    }

    #[tokio::test]
    async fn legacy_add_uses_request_surface_with_hostname() {
        let (base, requests) = mock_backend(vec![(
            201,
            r#"{"status":"success","message":"created","request_id":17,"auto_approved":true}"#,
        )])
        .await;
        let cfg = legacy_config(base.clone());
        let client = BackendClient::new(base, "legacy-key").unwrap();
        let expected_hostname = crate::config::hostname();

        let output = add_with(
            AddArgs {
                name: "legacy".into(),
                command: "npx".into(),
                args: Vec::new(),
                working_dir: None,
                display_name: None,
            },
            &cfg,
            &client,
        )
        .await
        .unwrap();

        assert!(output.contains("was auto-approved (request ID 17)"));
        let requests = requests.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].starts_with("POST /api/v1/mcp-requests HTTP/1.1"));
        let body: serde_json::Value =
            serde_json::from_str(requests[0].split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(body["hostname"], expected_hostname);
        assert!(body.get("device_ref_id").is_none());
        assert!(body.get("working_dir").is_none());
    }

    #[test]
    fn create_result_distinguishes_pending_and_auto_approved() {
        let pending = format_create_result(
            "demo",
            ClientCreateRequestResponse {
                request_id: 1,
                auto_approved: false,
            },
        );
        let approved = format_create_result(
            "demo",
            ClientCreateRequestResponse {
                request_id: 2,
                auto_approved: true,
            },
        );
        assert!(pending.contains("for approval"));
        assert!(approved.contains("auto-approved"));
    }
}

//! Persisted daemon config (`~/.config/edison-stdiod/config.toml`) plus
//! a few process-level helpers (hostname / OS detection).
//!
//! ## Layered config
//!
//! Both the `run` subcommand and the HTTP-talking subcommands (`server
//! add/list/remove`) accept their inputs from one of two layers:
//!
//! 1. CLI flags / environment variables (highest precedence - wins for dev
//!    iteration where you want to override a setting without touching the
//!    on-disk config).
//! 2. `~/.config/edison-stdiod/config.toml`, written by `edison-stdiod
//!    login`. This is what the OS supervisor unit reads from, since
//!    LaunchAgents / systemd units don't carry secrets in env.
//!
//! [`Resolved`] is the merged view, with a helper per call site that
//! errors with a clear message when a required value is missing from both
//! layers.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use url::Host;

use crate::{paths, secure_file};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    ClientAccessToken,
    LegacyApiKey,
}

/// A selected bearer credential. Its value deliberately has no `Debug`
/// implementation so logging a config decision cannot expose the secret.
#[derive(Clone, Copy)]
pub struct CredentialRef<'a> {
    token: &'a str,
    kind: CredentialKind,
}

impl<'a> CredentialRef<'a> {
    pub fn token(self) -> &'a str {
        self.token
    }

    pub fn kind(self) -> CredentialKind {
        self.kind
    }
}

/// On-disk shape of `config.toml`. All fields optional so a partial config
/// (e.g. backend URL only, before `login`) still parses.
#[derive(Default, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    /// Backend base URL (`http://localhost:3001` for dev,
    /// `https://dashboard.edison.watch` for prod).
    #[serde(default)]
    pub backend_url: Option<String>,
    /// Deprecated legacy API key. Retained because released desktop clients
    /// still invoke `login --api-key`.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Opaque client credential issued by the browser/device auth flow.
    #[serde(default)]
    pub client_access_token: Option<String>,
    /// Backend identity and account binding for the client credential.
    #[serde(default)]
    pub client_installation_id: Option<String>,
    #[serde(default)]
    pub authenticated_user_id: Option<String>,
    #[serde(default)]
    pub authenticated_org_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    /// Optional Edison secret key (`X-Edison-Secret-Key`).
    #[serde(default)]
    pub edison_secret_key: Option<String>,
    /// Server-issued device ID for client auth, or a legacy override.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Human-readable label shown in the admin UI.
    #[serde(default)]
    pub device_label: Option<String>,
}

impl std::fmt::Debug for PersistedConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PersistedConfig")
            .field("backend_url", &self.backend_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "client_access_token",
                &self.client_access_token.as_ref().map(|_| "<redacted>"),
            )
            .field("client_installation_id", &self.client_installation_id)
            .field("authenticated_user_id", &self.authenticated_user_id)
            .field("authenticated_org_id", &self.authenticated_org_id)
            .field("scopes", &self.scopes)
            .field(
                "edison_secret_key",
                &self.edison_secret_key.as_ref().map(|_| "<redacted>"),
            )
            .field("device_id", &self.device_id)
            .field("device_label", &self.device_label)
            .finish()
    }
}

impl PersistedConfig {
    /// Load from `~/.config/edison-stdiod/config.toml`. Missing file is OK
    /// and returns [`PersistedConfig::default`] so first-run / env-only
    /// flows work.
    pub fn load() -> Result<Self> {
        let path = paths::config_file()?;
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(body) => toml::from_str(&body).map_err(|_| {
                anyhow!(
                    "failed to parse {}; config contains invalid TOML",
                    path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Atomically rewrite the config file with mode 0600 on Unix. Existing
    /// fields not present in `self` are dropped - callers that want a
    /// merge load + mutate first.
    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        self.save_to(&path)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let body = toml::to_string_pretty(self).context("serialising config.toml")?;
        secure_file::write_private(path, body.as_bytes())
    }

    /// Prefer a browser-auth client token over a legacy API key when both are
    /// present in an old or manually-edited file.
    pub fn usable_credential(&self) -> Option<CredentialRef<'_>> {
        nonempty(self.client_access_token.as_deref())
            .map(|token| CredentialRef {
                token,
                kind: CredentialKind::ClientAccessToken,
            })
            .or_else(|| {
                nonempty(self.api_key.as_deref()).map(|token| CredentialRef {
                    token,
                    kind: CredentialKind::LegacyApiKey,
                })
            })
    }

    /// Validate the persisted values needed by a supervisor installation.
    /// Legacy credentials intentionally retain their hostname fallback.
    pub fn ensure_installable(&self) -> Result<()> {
        let backend = nonempty(self.backend_url.as_deref())
            .ok_or_else(|| missing("backend URL", "--backend / EDISON_BACKEND_URL"))?;
        normalize_backend_url(backend)?;
        let credential = self
            .usable_credential()
            .ok_or_else(|| anyhow!("no credentials on disk; run `edison-stdiod login` first"))?;
        if credential.kind() == CredentialKind::ClientAccessToken {
            if nonempty(self.client_installation_id.as_deref()).is_none() {
                return Err(anyhow!(
                    "client credential is missing client_installation_id; run `edison-stdiod login` again"
                ));
            }
            if nonempty(self.device_id.as_deref()).is_none() {
                return Err(anyhow!(
                    "client credential is missing its server-issued device_id; run `edison-stdiod login` again"
                ));
            }
        }
        Ok(())
    }

    /// Remove credentials and backend-issued account bindings while retaining
    /// the backend URL and local presentation preferences.
    pub fn clear_authentication(&mut self) {
        self.api_key = None;
        self.client_access_token = None;
        self.client_installation_id = None;
        self.authenticated_user_id = None;
        self.authenticated_org_id = None;
        self.scopes.clear();
        self.edison_secret_key = None;
        self.device_id = None;
    }
}

/// Merged view of CLI/env values overlaid on top of the persisted config.
/// Use the typed `*_required` helpers to extract values, which surface a
/// uniform "missing X - run `edison-stdiod login` or set EDISON_…" error.
pub struct Resolved {
    pub backend_url: Option<String>,
    pub api_key: Option<String>,
    pub client_access_token: Option<String>,
    pub client_installation_id: Option<String>,
    pub edison_secret_key: Option<String>,
    pub device_id: Option<String>,
    pub device_label: Option<String>,
}

impl Resolved {
    /// Build a [`Resolved`] from the on-disk config, letting any
    /// caller-provided overrides win when they're `Some`.
    pub fn merge(persisted: PersistedConfig, overrides: Resolved) -> Result<Self> {
        let has_legacy_override = overrides.api_key.is_some();
        let override_backend = overrides
            .backend_url
            .as_deref()
            .map(normalize_backend_url)
            .transpose()?;
        let saved_backend_result = persisted
            .backend_url
            .as_deref()
            .map(normalize_backend_url)
            .transpose();
        let normalized_saved_backend = if has_legacy_override && override_backend.is_some() {
            saved_backend_result.ok().flatten()
        } else {
            saved_backend_result?
        };
        let preserve_legacy_values = has_legacy_override
            && nonempty(persisted.client_access_token.as_deref()).is_none()
            && nonempty(persisted.api_key.as_deref()).is_some()
            && override_backend
                .as_ref()
                .is_none_or(|requested| normalized_saved_backend.as_ref() == Some(requested));
        let persisted_backend = if has_legacy_override && override_backend.is_some() {
            None
        } else {
            normalized_saved_backend
        };

        if !has_legacy_override {
            if let (Some(requested), Some(saved)) = (&override_backend, &persisted_backend) {
                // A saved backend URL alone (first run, or after logout) is
                // not a binding - only reject the override when the config
                // still holds credentials or account state issued by the
                // saved backend.
                if requested != saved && persisted.has_issuer_bound_values() {
                    return Err(anyhow!(
                        "--backend does not match the backend bound to the saved credential; pass an explicit legacy --api-key or run `edison-stdiod login`"
                    ));
                }
            } else if override_backend.is_some()
                && persisted_backend.is_none()
                && persisted.has_issuer_bound_values()
            {
                return Err(anyhow!(
                    "saved credentials have no backend binding; pass an explicit legacy --api-key or run `edison-stdiod login`"
                ));
            }

            if nonempty(persisted.client_access_token.as_deref()).is_some() {
                if let Some(device_id) = overrides.device_id.as_deref() {
                    if nonempty(persisted.device_id.as_deref()) != Some(device_id) {
                        return Err(anyhow!(
                            "the server-issued device ID bound to a client credential cannot be overridden"
                        ));
                    }
                }
            }
        }

        Ok(Self {
            backend_url: override_backend.or(persisted_backend),
            api_key: if has_legacy_override {
                overrides.api_key
            } else {
                persisted.api_key
            },
            client_access_token: if has_legacy_override {
                None
            } else {
                persisted.client_access_token
            },
            client_installation_id: if has_legacy_override {
                None
            } else {
                persisted.client_installation_id
            },
            edison_secret_key: if has_legacy_override {
                overrides.edison_secret_key.or_else(|| {
                    preserve_legacy_values
                        .then_some(persisted.edison_secret_key)
                        .flatten()
                })
            } else {
                overrides.edison_secret_key.or(persisted.edison_secret_key)
            },
            device_id: if has_legacy_override {
                overrides.device_id.or_else(|| {
                    preserve_legacy_values
                        .then_some(persisted.device_id)
                        .flatten()
                })
            } else {
                overrides.device_id.or(persisted.device_id)
            },
            device_label: overrides.device_label.or(persisted.device_label),
        })
    }

    pub fn backend_url(&self) -> Result<&str> {
        self.backend_url
            .as_deref()
            .ok_or_else(|| missing("backend URL", "--backend / EDISON_BACKEND_URL"))
    }

    pub fn usable_credential(&self) -> Result<CredentialRef<'_>> {
        nonempty(self.client_access_token.as_deref())
            .map(|token| CredentialRef {
                token,
                kind: CredentialKind::ClientAccessToken,
            })
            .or_else(|| {
                nonempty(self.api_key.as_deref()).map(|token| CredentialRef {
                    token,
                    kind: CredentialKind::LegacyApiKey,
                })
            })
            .ok_or_else(|| missing("credentials", "--api-key / EDISON_API_KEY"))
    }

    pub fn device_id(&self) -> Result<String> {
        if let Some(d) = &self.device_id {
            return Ok(d.clone());
        }
        Ok(hostname())
    }

    pub fn device_label(&self) -> String {
        self.device_label.clone().unwrap_or_else(hostname)
    }
}

impl PersistedConfig {
    fn has_issuer_bound_values(&self) -> bool {
        self.usable_credential().is_some()
            || nonempty(self.client_installation_id.as_deref()).is_some()
            || nonempty(self.authenticated_user_id.as_deref()).is_some()
            || nonempty(self.authenticated_org_id.as_deref()).is_some()
            || nonempty(self.edison_secret_key.as_deref()).is_some()
            || nonempty(self.device_id.as_deref()).is_some()
    }
}

/// Validate and canonicalize a backend origin. Production backends must use
/// HTTPS; cleartext HTTP is limited to loopback development endpoints.
pub fn normalize_backend_url(raw: &str) -> Result<String> {
    let parsed = url::Url::parse(raw).map_err(|_| anyhow!("backend URL is invalid"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host().is_none() {
        return Err(anyhow!("backend URL must be an absolute HTTP(S) URL"));
    }
    let authority = raw
        .split_once("://")
        .map(|(_, remainder)| remainder.split(['/', '?', '#']).next().unwrap_or(""))
        .unwrap_or("");
    if !parsed.username().is_empty() || parsed.password().is_some() || authority.contains('@') {
        return Err(anyhow!("backend URL must not contain user information"));
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(anyhow!("backend URL must not contain a query or fragment"));
    }
    if !matches!(parsed.path(), "" | "/") {
        return Err(anyhow!("backend URL must not contain a path"));
    }
    if parsed.scheme() == "http" && !is_loopback_host(parsed.host().expect("host checked above")) {
        return Err(anyhow!(
            "cleartext HTTP backend URLs are allowed only for localhost or loopback IPs"
        ));
    }

    Ok(parsed.as_str().trim_end_matches('/').to_string())
}

pub fn same_backend(left: Option<&str>, right: &str) -> bool {
    let right = match normalize_backend_url(right) {
        Ok(right) => right,
        Err(_) => return false,
    };
    left.and_then(|value| normalize_backend_url(value).ok())
        .is_some_and(|left| left == right)
}

fn is_loopback_host(host: Host<&str>) -> bool {
    match host {
        Host::Domain(domain) => domain.eq_ignore_ascii_case("localhost"),
        Host::Ipv4(address) => address.is_loopback(),
        Host::Ipv6(address) => address.is_loopback(),
    }
}

fn missing(name: &str, flag: &str) -> anyhow::Error {
    anyhow!("missing {name}: run `edison-stdiod login` first, or pass {flag}",)
}

fn nonempty(value: Option<&str>) -> Option<&str> {
    value.filter(|value| !value.is_empty())
}

/// Detected OS, mapped to the wire-protocol `Os` enum.
pub fn current_os() -> edison_tunnel_protocol::Os {
    if cfg!(target_os = "macos") {
        edison_tunnel_protocol::Os::Macos
    } else if cfg!(target_os = "linux") {
        edison_tunnel_protocol::Os::Linux
    } else {
        edison_tunnel_protocol::Os::Windows
    }
}

/// Best-effort hostname.
///
/// Tries env vars first (set in some Docker/CI envs), then falls back to
/// the ``hostname`` shell command - which is the only thing that works
/// reliably on macOS where ``HOSTNAME`` isn't exported to user processes.
pub fn hostname() -> String {
    if let Ok(h) = std::env::var("HOSTNAME") {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        let trimmed = h.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Ok(out) = std::process::Command::new("hostname").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "unknown".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_roundtrip_through_toml() {
        let dir = tempdir_or_skip();
        let path = dir.join("config.toml");
        let cfg = PersistedConfig {
            backend_url: Some("http://localhost:3001".into()),
            api_key: Some("edison_test".into()),
            edison_secret_key: None,
            device_id: Some("laptop".into()),
            device_label: Some("My Laptop".into()),
            ..Default::default()
        };
        let body = toml::to_string_pretty(&cfg).unwrap();
        std::fs::write(&path, body).unwrap();
        let parsed = PersistedConfig::load_from(&path).unwrap();
        assert_eq!(parsed.backend_url.as_deref(), Some("http://localhost:3001"));
        assert_eq!(parsed.api_key.as_deref(), Some("edison_test"));
        assert_eq!(parsed.device_id.as_deref(), Some("laptop"));
    }

    #[test]
    fn load_missing_file_returns_default() {
        let dir = tempdir_or_skip();
        let parsed = PersistedConfig::load_from(&dir.join("nonexistent.toml")).unwrap();
        assert!(parsed.api_key.is_none());
    }

    #[test]
    fn resolved_overrides_win_over_persisted() {
        let persisted = PersistedConfig {
            backend_url: Some("https://EXAMPLE.test/".into()),
            api_key: Some("from-disk".into()),
            ..Default::default()
        };
        let overrides = Resolved {
            backend_url: Some("https://example.test".into()),
            api_key: None,
            client_access_token: None,
            client_installation_id: None,
            edison_secret_key: None,
            device_id: None,
            device_label: None,
        };
        let merged = Resolved::merge(persisted, overrides).unwrap();
        assert_eq!(merged.backend_url.as_deref(), Some("https://example.test"));
        assert_eq!(merged.api_key.as_deref(), Some("from-disk"));
    }

    #[test]
    fn new_client_credential_is_preferred_over_legacy_key() {
        let cfg = PersistedConfig {
            api_key: Some("legacy".into()),
            client_access_token: Some("client".into()),
            client_installation_id: Some("install-1".into()),
            device_id: Some("device-1".into()),
            ..Default::default()
        };
        let selected = cfg.usable_credential().unwrap();
        assert_eq!(selected.kind(), CredentialKind::ClientAccessToken);
        assert_eq!(selected.token(), "client");
    }

    #[test]
    fn explicit_legacy_override_wins_over_persisted_client_token() {
        let persisted = PersistedConfig {
            api_key: Some("old-legacy".into()),
            client_access_token: Some("client".into()),
            client_installation_id: Some("install-1".into()),
            ..Default::default()
        };
        let merged = Resolved::merge(
            persisted,
            Resolved {
                backend_url: Some("https://other.test".into()),
                api_key: Some("flag-key".into()),
                client_access_token: None,
                client_installation_id: None,
                edison_secret_key: Some("explicit-secret".into()),
                device_id: Some("explicit-device".into()),
                device_label: None,
            },
        )
        .unwrap();
        let selected = merged.usable_credential().unwrap();
        assert_eq!(selected.kind(), CredentialKind::LegacyApiKey);
        assert_eq!(selected.token(), "flag-key");
        assert_eq!(merged.backend_url.as_deref(), Some("https://other.test"));
        assert_eq!(merged.edison_secret_key.as_deref(), Some("explicit-secret"));
        assert_eq!(merged.device_id.as_deref(), Some("explicit-device"));
    }

    #[test]
    fn backend_override_cannot_rebind_saved_client_identity() {
        let persisted = PersistedConfig {
            backend_url: Some("https://issuer.test".into()),
            client_access_token: Some("client-token".into()),
            client_installation_id: Some("install-1".into()),
            edison_secret_key: Some("account-secret".into()),
            device_id: Some("device-1".into()),
            ..Default::default()
        };
        let result = Resolved::merge(
            persisted,
            Resolved {
                backend_url: Some("https://other.test".into()),
                api_key: None,
                client_access_token: None,
                client_installation_id: None,
                edison_secret_key: None,
                device_id: None,
                device_label: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn explicit_legacy_override_does_not_inherit_client_bindings() {
        let persisted = PersistedConfig {
            backend_url: Some("https://issuer.test".into()),
            client_access_token: Some("client-token".into()),
            client_installation_id: Some("install-1".into()),
            edison_secret_key: Some("account-secret".into()),
            device_id: Some("device-1".into()),
            ..Default::default()
        };
        let merged = Resolved::merge(
            persisted,
            Resolved {
                backend_url: Some("https://other.test".into()),
                api_key: Some("legacy-key".into()),
                client_access_token: None,
                client_installation_id: None,
                edison_secret_key: None,
                device_id: None,
                device_label: None,
            },
        )
        .unwrap();
        assert!(merged.client_access_token.is_none());
        assert!(merged.client_installation_id.is_none());
        assert!(merged.edison_secret_key.is_none());
        assert!(merged.device_id.is_none());
    }

    #[test]
    fn client_device_binding_cannot_be_overridden() {
        let persisted = PersistedConfig {
            backend_url: Some("https://issuer.test".into()),
            client_access_token: Some("client-token".into()),
            client_installation_id: Some("install-1".into()),
            device_id: Some("device-1".into()),
            ..Default::default()
        };
        let result = Resolved::merge(
            persisted,
            Resolved {
                backend_url: None,
                api_key: None,
                client_access_token: None,
                client_installation_id: None,
                edison_secret_key: None,
                device_id: Some("device-2".into()),
                device_label: None,
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn legacy_toml_without_new_fields_still_loads() {
        let parsed: PersistedConfig = toml::from_str(
            r#"backend_url = "https://example.test"
api_key = "legacy"
device_label = "Laptop"
"#,
        )
        .unwrap();
        assert_eq!(parsed.api_key.as_deref(), Some("legacy"));
        assert!(parsed.client_access_token.is_none());
        assert!(parsed.scopes.is_empty());
    }

    #[test]
    fn clearing_authentication_preserves_preferences() {
        let mut cfg = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            api_key: Some("legacy".into()),
            client_access_token: Some("client".into()),
            client_installation_id: Some("install-1".into()),
            authenticated_user_id: Some("user-1".into()),
            authenticated_org_id: Some("org-1".into()),
            scopes: vec!["tunnel:connect".into()],
            edison_secret_key: Some("secret".into()),
            device_id: Some("device-1".into()),
            device_label: Some("My Laptop".into()),
        };
        cfg.clear_authentication();
        assert_eq!(cfg.backend_url.as_deref(), Some("https://example.test"));
        assert_eq!(cfg.device_label.as_deref(), Some("My Laptop"));
        assert!(cfg.usable_credential().is_none());
        assert!(cfg.device_id.is_none());
        assert!(cfg.edison_secret_key.is_none());
    }

    #[test]
    fn debug_output_redacts_credentials() {
        let cfg = PersistedConfig {
            api_key: Some("legacy-secret-value".into()),
            client_access_token: Some("client-secret-value".into()),
            edison_secret_key: Some("edison-secret-value".into()),
            ..Default::default()
        };
        let debug = format!("{cfg:?}");
        assert!(!debug.contains("legacy-secret-value"));
        assert!(!debug.contains("client-secret-value"));
        assert!(!debug.contains("edison-secret-value"));
    }

    #[test]
    fn toml_parse_errors_do_not_reflect_secret_lines() {
        let dir = tempdir_or_skip();
        let path = dir.join("malformed-config.toml");
        std::fs::write(&path, "client_access_token = \"do-not-reflect-this-token\n").unwrap();
        let message = PersistedConfig::load_from(&path).unwrap_err().to_string();
        assert!(message.contains("invalid TOML"));
        assert!(!message.contains("do-not-reflect-this-token"));
        assert!(!message.contains("client_access_token"));
    }

    #[test]
    fn backend_urls_are_secure_and_canonical() {
        assert_eq!(
            normalize_backend_url("HTTP://LOCALHOST:3001/").unwrap(),
            "http://localhost:3001"
        );
        assert_eq!(
            normalize_backend_url("http://127.42.0.1:3001/").unwrap(),
            "http://127.42.0.1:3001"
        );
        assert_eq!(
            normalize_backend_url("http://[::1]:3001/").unwrap(),
            "http://[::1]:3001"
        );
        assert_eq!(
            normalize_backend_url("HTTPS://EXAMPLE.COM/").unwrap(),
            "https://example.com"
        );
    }

    #[test]
    fn backend_urls_reject_unsafe_or_non_origin_inputs() {
        for invalid in [
            "http://example.com",
            "ws://localhost:3001",
            "wss://example.com",
            "https://user:password@example.com",
            "https://example.com?token=secret",
            "https://example.com#fragment",
            "https://example.com/api",
        ] {
            assert!(
                normalize_backend_url(invalid).is_err(),
                "accepted {invalid}"
            );
        }
    }

    #[test]
    fn repeated_config_saves_replace_and_leave_no_temp_files() {
        let dir = tempdir_or_skip();
        let path = dir.join("repeat-config.toml");
        let mut cfg = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            ..Default::default()
        };
        cfg.save_to(&path).unwrap();
        cfg.device_label = Some("updated".into());
        cfg.save_to(&path).unwrap();
        assert_eq!(
            PersistedConfig::load_from(&path)
                .unwrap()
                .device_label
                .as_deref(),
            Some("updated")
        );
        let prefix = format!(".{}.", path.file_name().unwrap().to_string_lossy());
        assert!(std::fs::read_dir(&dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&prefix)
        }));
    }

    #[test]
    fn client_installation_requires_server_binding_but_legacy_does_not() {
        let incomplete_client = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            client_access_token: Some("client".into()),
            ..Default::default()
        };
        assert!(incomplete_client.ensure_installable().is_err());

        let legacy = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            api_key: Some("legacy".into()),
            ..Default::default()
        };
        assert!(legacy.ensure_installable().is_ok());
    }

    fn tempdir_or_skip() -> std::path::PathBuf {
        use std::sync::atomic::{AtomicUsize, Ordering};

        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "edison-stdiod-test-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

#[cfg(test)]
#[path = "config_extra_tests.rs"]
mod extra_tests;

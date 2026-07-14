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

use crate::paths;

/// On-disk shape of `config.toml`. All fields optional so a partial config
/// (e.g. backend URL only, before `login`) still parses.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PersistedConfig {
    /// Backend base URL (`http://localhost:3001` for dev,
    /// `https://dashboard.edison.watch` for prod).
    #[serde(default)]
    pub backend_url: Option<String>,
    /// API key used for both the WS Bearer header and HTTP API calls.
    #[serde(default)]
    pub api_key: Option<String>,
    /// Optional Edison secret key (`X-Edison-Secret-Key`).
    #[serde(default)]
    pub edison_secret_key: Option<String>,
    /// Stable device id; defaults to the hostname when the user doesn't
    /// pick one explicitly.
    #[serde(default)]
    pub device_id: Option<String>,
    /// Human-readable label shown in the admin UI.
    #[serde(default)]
    pub device_label: Option<String>,
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
            Ok(body) => {
                toml::from_str(&body).with_context(|| format!("failed to parse {}", path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
        }
    }

    /// Atomically rewrite the config file with mode 0600 on Unix. Existing
    /// fields not present in `self` are dropped - callers that want a
    /// merge load + mutate first.
    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        let tmp = path.with_extension("toml.tmp");
        let body = toml::to_string_pretty(self).context("serialising config.toml")?;
        std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&tmp)?.permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&tmp, perms)?;
        }
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming {} -> {}", tmp.display(), path.display()))?;
        Ok(())
    }
}

/// Merged view of CLI/env values overlaid on top of the persisted config.
/// Use the typed `*_required` helpers to extract values, which surface a
/// uniform "missing X - run `edison-stdiod login` or set EDISON_…" error.
pub struct Resolved {
    pub backend_url: Option<String>,
    pub api_key: Option<String>,
    pub edison_secret_key: Option<String>,
    pub device_id: Option<String>,
    pub device_label: Option<String>,
}

impl Resolved {
    /// Build a [`Resolved`] from the on-disk config, letting any
    /// caller-provided overrides win when they're `Some`.
    pub fn merge(persisted: PersistedConfig, overrides: Resolved) -> Self {
        Self {
            backend_url: overrides.backend_url.or(persisted.backend_url),
            api_key: overrides.api_key.or(persisted.api_key),
            edison_secret_key: overrides.edison_secret_key.or(persisted.edison_secret_key),
            device_id: overrides.device_id.or(persisted.device_id),
            device_label: overrides.device_label.or(persisted.device_label),
        }
    }

    pub fn backend_url(&self) -> Result<&str> {
        self.backend_url
            .as_deref()
            .ok_or_else(|| missing("backend URL", "--backend / EDISON_BACKEND_URL"))
    }

    pub fn api_key(&self) -> Result<&str> {
        self.api_key
            .as_deref()
            .ok_or_else(|| missing("API key", "--api-key / EDISON_API_KEY"))
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

fn missing(name: &str, flag: &str) -> anyhow::Error {
    anyhow!("missing {name}: run `edison-stdiod login` first, or pass {flag}",)
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
            backend_url: Some("http://from-disk".into()),
            api_key: Some("from-disk".into()),
            ..Default::default()
        };
        let overrides = Resolved {
            backend_url: Some("http://from-env".into()),
            api_key: None,
            edison_secret_key: None,
            device_id: None,
            device_label: None,
        };
        let merged = Resolved::merge(persisted, overrides);
        assert_eq!(merged.backend_url.as_deref(), Some("http://from-env"));
        assert_eq!(merged.api_key.as_deref(), Some("from-disk"));
    }

    fn tempdir_or_skip() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("edison-stdiod-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}

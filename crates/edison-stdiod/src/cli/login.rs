//! `edison-stdiod login` - persist credentials + backend URL to
//! `~/.config/edison-stdiod/config.toml`.
//!
//! The supervisor unit (`launchctl`/`systemctl`) cannot reliably carry
//! secrets in its environment, so a one-shot login step writes them to a
//! 0600 file the daemon reads at startup. Re-running `login` merges:
//! existing fields are preserved unless explicitly overridden, so a user
//! can rotate their API key without re-supplying the backend URL.

use anyhow::{anyhow, Result};
use clap::Args;
use tracing::info;

use crate::config::PersistedConfig;
use crate::paths;

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Backend base URL - `http://localhost:3001` for `make dev`,
    /// `https://dashboard.edison.watch` for prod.
    #[arg(long)]
    pub backend: Option<String>,
    /// Bearer API key issued by the backend. Stored in plaintext under
    /// mode 0600; rotate by re-running `login --api-key …`.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Optional `X-Edison-Secret-Key` for per-user secret decryption.
    #[arg(long)]
    pub edison_secret_key: Option<String>,
    /// Stable device identifier. Defaults to the machine's hostname when
    /// neither stored nor passed.
    #[arg(long)]
    pub device_id: Option<String>,
    /// Human-readable label shown in the admin Devices page.
    #[arg(long)]
    pub device_label: Option<String>,
}

pub fn run(args: LoginArgs) -> Result<()> {
    let mut cfg = PersistedConfig::load()?;

    if let Some(v) = args.backend {
        cfg.backend_url = Some(normalize_backend_url(&v)?);
    }
    if let Some(v) = args.api_key {
        cfg.api_key = Some(v);
    }
    if let Some(v) = args.edison_secret_key {
        cfg.edison_secret_key = Some(v);
    }
    if let Some(v) = args.device_id {
        cfg.device_id = Some(v);
    }
    if let Some(v) = args.device_label {
        cfg.device_label = Some(v);
    }

    // Require at least the API key + backend URL to exist *after* the
    // merge - otherwise `install` will write a supervisor unit that boots
    // a daemon doomed to fail at first connect.
    if cfg.backend_url.as_deref().unwrap_or("").is_empty() {
        return Err(anyhow!(
            "missing backend URL. Pass --backend on first login.",
        ));
    }
    if cfg.api_key.as_deref().unwrap_or("").is_empty() {
        return Err(anyhow!("missing API key. Pass --api-key on first login.",));
    }

    cfg.save()?;
    let path = paths::config_file()?;
    info!(path = %path.display(), "wrote config.toml");
    println!("Saved {}", path.display());
    println!("Next: `edison-stdiod install` to register the LaunchAgent.");
    Ok(())
}

/// Strip a trailing slash so we can build `<backend>/api/v1/...` paths by
/// simple concatenation, and reject obvious typos early instead of
/// surfacing a confusing TCP error at first connect.
fn normalize_backend_url(raw: &str) -> Result<String> {
    let url = url::Url::parse(raw).map_err(|e| anyhow!("`{}` is not a valid URL ({})", raw, e))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(anyhow!(
            "backend URL must use http or https, got `{}`",
            url.scheme(),
        ));
    }
    Ok(raw.trim_end_matches('/').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_strips_trailing_slash() {
        assert_eq!(
            normalize_backend_url("http://localhost:3001/").unwrap(),
            "http://localhost:3001",
        );
    }

    #[test]
    fn normalize_accepts_https() {
        assert_eq!(
            normalize_backend_url("https://dashboard.edison.watch").unwrap(),
            "https://dashboard.edison.watch",
        );
    }

    #[test]
    fn normalize_rejects_bad_scheme() {
        let err = normalize_backend_url("ftp://example.com").unwrap_err();
        assert!(err.to_string().contains("must use http or https"));
    }

    #[test]
    fn normalize_rejects_garbage() {
        assert!(normalize_backend_url("not a url").is_err());
    }
}

//! `edison-stdiod login` - browser/device authorization or the deprecated
//! API-key persistence path used by existing desktop releases.

use anyhow::{anyhow, Result};
use clap::Args;
use tracing::{info, warn};

use crate::auth::{AuthClient, Pkce};
use crate::config::{self, PersistedConfig};
use crate::paths;

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Backend base URL - `http://localhost:3001` for development or the
    /// Edison Watch dashboard URL for production.
    #[arg(long)]
    pub backend: Option<String>,
    /// Deprecated legacy Bearer API key. When omitted, login uses browser
    /// authorization and stores a scoped client access token.
    #[arg(long)]
    pub api_key: Option<String>,
    /// Do not try to open a browser during interactive login. The
    /// verification URL and user code are still printed.
    #[arg(long)]
    pub no_open: bool,
    /// Optional `X-Edison-Secret-Key` for per-user secret decryption.
    #[arg(long)]
    pub edison_secret_key: Option<String>,
    /// Legacy device identifier override. Interactive login always uses the
    /// server-issued device ID.
    #[arg(long)]
    pub device_id: Option<String>,
    /// Human-readable label shown in the admin Devices page.
    #[arg(long)]
    pub device_label: Option<String>,
}

pub async fn run(args: LoginArgs) -> Result<()> {
    let mut cfg = PersistedConfig::load()?;
    let backend = match args.backend.as_deref() {
        Some(value) => config::normalize_backend_url(value)?,
        None => cfg
            .backend_url
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(config::normalize_backend_url)
            .transpose()?
            .ok_or_else(|| anyhow!("missing backend URL. Pass --backend on first login."))?,
    };
    let same_issuer = config::same_backend(cfg.backend_url.as_deref(), &backend);

    if let Some(api_key) = args.api_key.clone() {
        return legacy_login(cfg, backend, same_issuer, api_key, args).await;
    }
    let previous_credential = capture_client_revocation(&cfg);

    let pkce = Pkce::generate()?;
    let auth = AuthClient::new(backend.clone())?;
    let label = args.device_label.as_deref().or(cfg.device_label.as_deref());
    let existing_installation = reusable_installation(&cfg, same_issuer);
    let code = auth
        .initiate(pkce.challenge(), label, existing_installation)
        .await?;

    println!("Open this URL to authorize stdiod:");
    println!("{}", code.verification_uri_complete);
    println!("User code: {}", code.user_code);
    if !try_open_browser(&code.verification_uri_complete, args.no_open, |url| {
        webbrowser::open(url).is_ok()
    })? && !args.no_open
    {
        warn!("could not open the default browser; continuing with printed instructions");
        eprintln!("Could not open a browser. Open the URL above manually.");
    }
    println!("Waiting for authorization...");

    let token = auth.poll(&code, pkce.verifier()).await?;
    let account_changed = account_changed(&cfg, same_issuer, &token);

    cfg.backend_url = Some(backend);
    cfg.api_key = None;
    cfg.client_access_token = Some(token.access_token);
    cfg.client_installation_id = Some(token.client_installation_id);
    cfg.authenticated_user_id = Some(token.user_id);
    cfg.authenticated_org_id = Some(token.org_id);
    cfg.device_id = Some(token.device_id);
    cfg.scopes = token.scope;
    if let Some(label) = args.device_label {
        cfg.device_label = Some(label);
    }
    if account_changed {
        cfg.edison_secret_key = args.edison_secret_key;
    } else if let Some(secret) = args.edison_secret_key {
        cfg.edison_secret_key = Some(secret);
    }

    save_login(&cfg)?;
    if account_changed {
        revoke_previous(previous_credential).await;
    }
    println!("Authorization complete.");
    Ok(())
}

async fn legacy_login(
    cfg: PersistedConfig,
    backend: String,
    same_issuer: bool,
    api_key: String,
    args: LoginArgs,
) -> Result<()> {
    let previous_credential = capture_client_revocation(&cfg);
    let cfg = configure_legacy_login(cfg, backend, same_issuer, api_key, args)?;
    save_login(&cfg)?;
    revoke_previous(previous_credential).await;
    Ok(())
}

/// A reused installation can still come back bound to a different user or
/// organization, so compare every server-issued account identifier - not just
/// the installation - before deciding to keep per-user state like the Edison
/// secret key or to skip revoking the previous client credential.
fn account_changed(
    cfg: &PersistedConfig,
    same_issuer: bool,
    token: &crate::auth::DeviceTokenResponse,
) -> bool {
    !same_issuer
        || cfg.client_installation_id.as_deref() != Some(token.client_installation_id.as_str())
        || cfg.authenticated_user_id.as_deref() != Some(token.user_id.as_str())
        || cfg.authenticated_org_id.as_deref() != Some(token.org_id.as_str())
}

fn capture_client_revocation(cfg: &PersistedConfig) -> Option<(String, String)> {
    Some((
        cfg.backend_url.as_ref()?.clone(),
        cfg.client_access_token
            .as_ref()
            .filter(|token| !token.is_empty())?
            .clone(),
    ))
}

async fn revoke_previous(revocation: Option<(String, String)>) {
    let Some((backend, token)) = revocation else {
        return;
    };
    match AuthClient::new(backend) {
        Ok(client) => {
            if let Err(error) = client.revoke(&token).await {
                warn!(
                    status = ?error.status(),
                    auth_rejected = error.is_auth_rejection(),
                    "previous client credential revocation failed after login"
                );
            }
        }
        Err(_) => warn!("could not construct previous credential revocation client"),
    }
}

fn reusable_installation(cfg: &PersistedConfig, same_issuer: bool) -> Option<&str> {
    same_issuer
        .then_some(cfg.client_installation_id.as_deref())
        .flatten()
        .filter(|id| !id.is_empty())
}

fn configure_legacy_login(
    mut cfg: PersistedConfig,
    backend: String,
    same_issuer: bool,
    api_key: String,
    args: LoginArgs,
) -> Result<PersistedConfig> {
    if api_key.is_empty() {
        return Err(anyhow!("legacy API key cannot be empty"));
    }
    if !same_issuer
        || cfg
            .client_access_token
            .as_deref()
            .is_some_and(|token| !token.is_empty())
    {
        cfg.clear_authentication();
    }
    cfg.backend_url = Some(backend);
    cfg.api_key = Some(api_key);
    cfg.client_access_token = None;
    cfg.client_installation_id = None;
    cfg.authenticated_user_id = None;
    cfg.authenticated_org_id = None;
    cfg.scopes.clear();
    if let Some(value) = args.edison_secret_key {
        cfg.edison_secret_key = Some(value);
    }
    if let Some(value) = args.device_id {
        cfg.device_id = Some(value);
    }
    if let Some(value) = args.device_label {
        cfg.device_label = Some(value);
    }
    Ok(cfg)
}

fn save_login(cfg: &PersistedConfig) -> Result<()> {
    cfg.save()?;
    let path = paths::config_file()?;
    info!(path = %path.display(), "wrote config.toml");
    println!("Saved {}", path.display());
    println!("Next: `edison-stdiod install` to register the supervisor service.");
    Ok(())
}

/// Validate before invoking the opener. Returning `false` means skipped or
/// failed; both are nonfatal because the printed code is sufficient.
fn try_open_browser<F>(url: &str, no_open: bool, opener: F) -> Result<bool>
where
    F: FnOnce(&str) -> bool,
{
    crate::auth::validate_http_url(url)?;
    if no_open {
        return Ok(false);
    }
    Ok(opener(url))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    #[test]
    fn no_open_does_not_invoke_browser() {
        let called = AtomicBool::new(false);
        let opened = try_open_browser("https://example.test/activate", true, |_| {
            called.store(true, Ordering::Relaxed);
            true
        })
        .unwrap();
        assert!(!opened);
        assert!(!called.load(Ordering::Relaxed));
    }

    #[test]
    fn browser_failure_is_nonfatal() {
        let result = try_open_browser("https://example.test/activate", false, |_| false);
        assert!(matches!(result, Ok(false)));
    }

    #[test]
    fn browser_url_is_validated_even_when_opening_is_disabled() {
        assert!(try_open_browser("javascript:alert(1)", true, |_| true).is_err());
    }

    #[test]
    fn existing_installation_is_reused_only_for_same_backend() {
        let cfg = PersistedConfig {
            client_installation_id: Some("install-1".into()),
            ..Default::default()
        };
        assert_eq!(reusable_installation(&cfg, true), Some("install-1"));
        assert_eq!(reusable_installation(&cfg, false), None);
    }

    #[test]
    fn reauthorization_for_a_different_user_or_org_is_an_account_change() {
        let cfg = PersistedConfig {
            backend_url: Some("https://issuer.test".into()),
            client_installation_id: Some("install-1".into()),
            authenticated_user_id: Some("user-1".into()),
            authenticated_org_id: Some("org-1".into()),
            ..Default::default()
        };
        let token = |user: &str, org: &str| crate::auth::DeviceTokenResponse {
            access_token: "token".into(),
            token_type: "Bearer".into(),
            client_installation_id: "install-1".into(),
            device_id: "device-1".into(),
            scope: vec![],
            user_id: user.into(),
            org_id: org.into(),
        };
        assert!(!account_changed(&cfg, true, &token("user-1", "org-1")));
        assert!(account_changed(&cfg, false, &token("user-1", "org-1")));
        assert!(account_changed(&cfg, true, &token("user-2", "org-1")));
        assert!(account_changed(&cfg, true, &token("user-1", "org-2")));
    }

    #[test]
    fn previous_client_credential_is_captured_before_account_switch() {
        let cfg = PersistedConfig {
            backend_url: Some("https://old.test".into()),
            client_access_token: Some("old-client-token".into()),
            api_key: Some("ignored-legacy-key".into()),
            ..Default::default()
        };
        assert_eq!(
            capture_client_revocation(&cfg),
            Some(("https://old.test".into(), "old-client-token".into()))
        );
    }

    #[test]
    fn changed_backend_legacy_login_clears_account_bindings() {
        let cfg = PersistedConfig {
            backend_url: Some("https://old.test".into()),
            client_access_token: Some("client-token".into()),
            client_installation_id: Some("install-1".into()),
            authenticated_user_id: Some("user-1".into()),
            authenticated_org_id: Some("org-1".into()),
            edison_secret_key: Some("old-secret".into()),
            device_id: Some("old-device".into()),
            ..Default::default()
        };
        let updated = configure_legacy_login(
            cfg,
            "https://new.test".into(),
            false,
            "legacy-key".into(),
            LoginArgs {
                backend: None,
                api_key: None,
                no_open: true,
                edison_secret_key: None,
                device_id: None,
                device_label: None,
            },
        )
        .unwrap();
        assert!(updated.client_access_token.is_none());
        assert!(updated.client_installation_id.is_none());
        assert!(updated.authenticated_user_id.is_none());
        assert!(updated.authenticated_org_id.is_none());
        assert!(updated.edison_secret_key.is_none());
        assert!(updated.device_id.is_none());
    }

    #[test]
    fn changed_backend_legacy_login_keeps_only_explicit_secret_and_device() {
        let cfg = PersistedConfig {
            backend_url: Some("https://old.test".into()),
            edison_secret_key: Some("old-secret".into()),
            device_id: Some("old-device".into()),
            ..Default::default()
        };
        let updated = configure_legacy_login(
            cfg,
            "https://new.test".into(),
            false,
            "legacy-key".into(),
            LoginArgs {
                backend: None,
                api_key: None,
                no_open: true,
                edison_secret_key: Some("new-secret".into()),
                device_id: Some("new-device".into()),
                device_label: None,
            },
        )
        .unwrap();
        assert_eq!(updated.edison_secret_key.as_deref(), Some("new-secret"));
        assert_eq!(updated.device_id.as_deref(), Some("new-device"));
    }
}

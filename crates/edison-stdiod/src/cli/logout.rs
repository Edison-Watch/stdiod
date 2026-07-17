//! `edison-stdiod logout` - local credential removal followed by best-effort
//! remote revocation.

use anyhow::Result;
use clap::Args;
use tracing::warn;

use crate::auth::AuthClient;
use crate::config::{CredentialKind, PersistedConfig};

#[derive(Debug, Args)]
pub struct LogoutArgs {}

pub async fn run(_args: LogoutArgs) -> Result<()> {
    let mut cfg = PersistedConfig::load()?;
    let revoke = capture_revocation(&cfg);

    // Publish local logout atomically before any network wait. The live daemon
    // can then stop authenticated children even if revocation times out.
    cfg.clear_authentication();
    cfg.save()?;

    if let Some((backend, token)) = revoke {
        match AuthClient::new(backend) {
            Ok(client) => {
                if let Err(error) = client.revoke(&token).await {
                    warn!(
                        status = ?error.status(),
                        auth_rejected = error.is_auth_rejection(),
                        "client credential revocation failed; local logout is complete"
                    );
                }
            }
            Err(_) => warn!("could not construct revocation client; local logout is complete"),
        }
    }

    println!("Logged out. Backend URL and local preferences were retained.");
    Ok(())
}

fn capture_revocation(cfg: &PersistedConfig) -> Option<(String, String)> {
    cfg.backend_url.clone().and_then(|backend| {
        cfg.usable_credential().and_then(|credential| {
            (credential.kind() == CredentialKind::ClientAccessToken)
                .then(|| (backend, credential.token().to_string()))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_logout_is_idempotent() {
        let mut cfg = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            client_access_token: Some("token".into()),
            client_installation_id: Some("install-1".into()),
            device_id: Some("device-1".into()),
            device_label: Some("Laptop".into()),
            ..Default::default()
        };
        cfg.clear_authentication();
        cfg.clear_authentication();
        assert!(cfg.usable_credential().is_none());
        assert_eq!(cfg.backend_url.as_deref(), Some("https://example.test"));
        assert_eq!(cfg.device_label.as_deref(), Some("Laptop"));
    }

    #[test]
    fn revocation_data_survives_local_clear() {
        let mut cfg = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            client_access_token: Some("token".into()),
            client_installation_id: Some("install-1".into()),
            ..Default::default()
        };
        let revocation = capture_revocation(&cfg);
        cfg.clear_authentication();
        assert_eq!(
            revocation,
            Some(("https://example.test".into(), "token".into()))
        );
        assert!(cfg.usable_credential().is_none());
    }

    #[test]
    fn legacy_keys_are_not_sent_to_client_revocation() {
        let cfg = PersistedConfig {
            backend_url: Some("https://example.test".into()),
            api_key: Some("legacy".into()),
            ..Default::default()
        };
        assert!(capture_revocation(&cfg).is_none());
    }
}

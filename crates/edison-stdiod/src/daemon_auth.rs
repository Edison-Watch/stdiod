//! Resolved daemon authentication state and account-bound lifecycle decisions.

use anyhow::Result;
use clap::Args;
use thiserror::Error;

use crate::{config, tunnel};

#[derive(Debug, Args, Clone)]
pub struct RunArgs {
    /// Backend base URL (http://localhost:8000, https://dashboard.edison.watch, ...).
    /// Falls back to `backend_url` in `~/.config/edison-stdiod/config.toml`.
    #[arg(long, env = "EDISON_BACKEND_URL")]
    pub backend: Option<String>,
    /// Deprecated legacy API key override. Without it, the browser-auth client
    /// access token in config.toml is preferred.
    #[arg(long, env = "EDISON_API_KEY")]
    pub api_key: Option<String>,
    /// Optional edison secret key (X-Edison-Secret-Key).
    #[arg(long, env = "EDISON_SECRET_KEY")]
    pub edison_secret_key: Option<String>,
    /// Device identifier (must match the row in the backend's `devices` table).
    /// Defaults to the persisted `device_id`, then the machine hostname.
    #[arg(long, env = "EDISON_DEVICE_ID")]
    pub device_id: Option<String>,
    /// Human-readable device label (shown in the admin UI).
    #[arg(long, env = "EDISON_DEVICE_LABEL")]
    pub label: Option<String>,
}

#[derive(Debug, Error)]
pub(crate) enum ResolveRunError {
    #[error("credentials are missing or incomplete; run `edison-stdiod login`")]
    AwaitingLogin,
    #[error(transparent)]
    Invalid(#[from] anyhow::Error),
}

/// Snapshot of resolved connection values. It is rebuilt while reconnecting so
/// login, logout, credential rotation, and account changes are observed.
#[derive(Clone)]
pub(crate) struct ResolvedRun {
    pub(crate) backend: String,
    pub(crate) credential: String,
    pub(crate) credential_kind: config::CredentialKind,
    pub(crate) client_installation_id: Option<String>,
    pub(crate) edison_secret_key: Option<String>,
    pub(crate) device_id: String,
    pub(crate) label: String,
}

impl ResolvedRun {
    pub(crate) fn from_args(args: &RunArgs) -> Result<Self, ResolveRunError> {
        let persisted = config::PersistedConfig::load()?;
        let merged = config::Resolved::merge(
            persisted,
            config::Resolved {
                backend_url: args.backend.clone(),
                api_key: args.api_key.clone(),
                client_access_token: None,
                client_installation_id: None,
                edison_secret_key: args.edison_secret_key.clone(),
                device_id: args.device_id.clone(),
                device_label: args.label.clone(),
            },
        )?;
        let credential = merged
            .usable_credential()
            .map_err(|_| ResolveRunError::AwaitingLogin)?;
        let credential_kind = credential.kind();
        let credential = credential.token().to_string();
        let client_installation_id = if credential_kind == config::CredentialKind::ClientAccessToken
        {
            Some(
                merged
                    .client_installation_id
                    .clone()
                    .filter(|id| !id.is_empty())
                    .ok_or(ResolveRunError::AwaitingLogin)?,
            )
        } else {
            None
        };
        let device_id = if credential_kind == config::CredentialKind::ClientAccessToken {
            merged
                .device_id
                .clone()
                .filter(|id| !id.is_empty())
                .ok_or(ResolveRunError::AwaitingLogin)?
        } else {
            merged.device_id()?
        };
        Ok(Self {
            backend: merged
                .backend_url()
                .map_err(|_| ResolveRunError::AwaitingLogin)?
                .to_string(),
            credential,
            credential_kind,
            client_installation_id,
            edison_secret_key: merged.edison_secret_key.clone(),
            device_id,
            label: merged.device_label(),
        })
    }

    pub(crate) fn same_connection(&self, other: &Self) -> bool {
        self.backend == other.backend
            && self.credential == other.credential
            && self.credential_kind == other.credential_kind
            && self.client_installation_id == other.client_installation_id
            && self.edison_secret_key == other.edison_secret_key
            && self.device_id == other.device_id
            && self.label == other.label
    }

    pub(crate) fn env_namespace(&self) -> Option<String> {
        self.client_installation_id
            .as_ref()
            .map(|installation_id| format!("{}\n{installation_id}", self.backend))
    }
}

pub(crate) fn is_auth_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tunnel::ConnectError>()
        .is_some_and(tunnel::ConnectError::needs_reauth)
        || error
            .downcast_ref::<tunnel::SessionCloseError>()
            .is_some_and(tunnel::SessionCloseError::needs_reauth)
}

pub(crate) fn is_protocol_rejection(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<tunnel::SessionCloseError>()
        .is_some_and(tunnel::SessionCloseError::needs_upgrade)
}

pub(crate) fn requires_child_reset(current: &ResolvedRun, next: &ResolvedRun) -> bool {
    current.backend != next.backend
        || current.credential_kind != next.credential_kind
        || current.client_installation_id != next.client_installation_id
        || current.device_id != next.device_id
        || (current.credential_kind == config::CredentialKind::LegacyApiKey
            && current.credential != next.credential)
}

pub(crate) fn connection_error_message(error: &anyhow::Error, resolved: &ResolvedRun) -> String {
    let mut message = error
        .to_string()
        .replace(&resolved.credential, "<redacted>");
    if let Some(secret) = resolved
        .edison_secret_key
        .as_deref()
        .filter(|secret| !secret.is_empty())
    {
        message = message.replace(secret, "<redacted>");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolved() -> ResolvedRun {
        ResolvedRun {
            backend: "https://example.test".into(),
            credential: "opaque-client-token".into(),
            credential_kind: config::CredentialKind::ClientAccessToken,
            client_installation_id: Some("install-1".into()),
            edison_secret_key: Some("edison-secret".into()),
            device_id: "device-1".into(),
            label: "Laptop".into(),
        }
    }

    #[test]
    fn backend_close_errors_cannot_reflect_credentials() {
        let resolved = resolved();
        let error =
            anyhow::anyhow!("backend close reason echoed opaque-client-token and edison-secret");
        let message = connection_error_message(&error, &resolved);
        assert_eq!(
            message,
            "backend close reason echoed <redacted> and <redacted>"
        );
    }

    #[test]
    fn reconnect_snapshot_detects_credential_and_account_changes() {
        let current = resolved();
        let mut rotated = current.clone();
        rotated.credential = "rotated-client-token".into();
        assert!(!current.same_connection(&rotated));

        let mut switched = current.clone();
        switched.client_installation_id = Some("install-2".into());
        assert!(!current.same_connection(&switched));
        assert_ne!(current.env_namespace(), switched.env_namespace());

        let mut issuer_switched = current.clone();
        issuer_switched.backend = "https://other.test".into();
        assert_ne!(current.env_namespace(), issuer_switched.env_namespace());
    }

    #[test]
    fn reset_decisions_preserve_only_same_installation_client_rotation() {
        let current = resolved();

        let mut rotated_client = current.clone();
        rotated_client.credential = "rotated-client-token".into();
        assert!(!requires_child_reset(&current, &rotated_client));

        let mut backend_changed = current.clone();
        backend_changed.backend = "https://other.test".into();
        assert!(requires_child_reset(&current, &backend_changed));

        let mut account_changed = current.clone();
        account_changed.client_installation_id = Some("install-2".into());
        assert!(requires_child_reset(&current, &account_changed));

        let mut device_changed = current.clone();
        device_changed.device_id = "device-2".into();
        assert!(requires_child_reset(&current, &device_changed));

        let mut kind_changed = current.clone();
        kind_changed.credential_kind = config::CredentialKind::LegacyApiKey;
        kind_changed.client_installation_id = None;
        assert!(requires_child_reset(&current, &kind_changed));
    }

    #[test]
    fn legacy_credential_change_resets_without_an_env_namespace() {
        let mut current = resolved();
        current.credential_kind = config::CredentialKind::LegacyApiKey;
        current.client_installation_id = None;
        let mut changed = current.clone();
        changed.credential = "new-legacy-key".into();
        assert_eq!(current.env_namespace(), None);
        assert_eq!(changed.env_namespace(), None);
        assert!(requires_child_reset(&current, &changed));
    }

    #[test]
    fn daemon_classifies_upgrade_auth_rejections() {
        let unauthorized = anyhow::Error::new(tunnel::ConnectError::AuthRejected { status: 401 });
        let server_error =
            anyhow::Error::new(tunnel::ConnectError::UpgradeRejected { status: 500 });
        assert!(is_auth_rejection(&unauthorized));
        assert!(!is_auth_rejection(&server_error));

        let revoked = anyhow::Error::new(tunnel::SessionCloseError::ClientCredentialRevoked);
        let other_policy = anyhow::Error::new(tunnel::SessionCloseError::Closed {
            code: 1008,
            reason: "some other policy".into(),
        });
        assert!(is_auth_rejection(&revoked));
        assert!(!is_auth_rejection(&other_policy));

        let mismatch = anyhow::Error::new(tunnel::SessionCloseError::ProtocolVersionMismatch {
            reason: "protocol_version mismatch".into(),
        });
        assert!(is_protocol_rejection(&mismatch));
    }
}

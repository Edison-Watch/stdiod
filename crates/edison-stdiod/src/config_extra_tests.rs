use super::*;

#[test]
fn explicit_legacy_rotation_keeps_same_issuer_legacy_settings() {
    let persisted = PersistedConfig {
        backend_url: Some("https://issuer.test".into()),
        api_key: Some("old-key".into()),
        edison_secret_key: Some("legacy-secret".into()),
        device_id: Some("legacy-device".into()),
        ..Default::default()
    };
    let merged = Resolved::merge(
        persisted,
        Resolved {
            backend_url: Some("https://ISSUER.test/".into()),
            api_key: Some("rotated-key".into()),
            client_access_token: None,
            client_installation_id: None,
            edison_secret_key: None,
            device_id: None,
            device_label: None,
        },
    )
    .unwrap();
    assert_eq!(merged.edison_secret_key.as_deref(), Some("legacy-secret"));
    assert_eq!(merged.device_id.as_deref(), Some("legacy-device"));
}

#[test]
fn backend_override_is_allowed_when_no_credential_is_saved() {
    let persisted = PersistedConfig {
        backend_url: Some("https://saved.test".into()),
        ..Default::default()
    };
    let merged = Resolved::merge(
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
    )
    .unwrap();
    assert_eq!(merged.backend_url.as_deref(), Some("https://other.test"));
}

#[test]
fn backend_override_mismatch_is_rejected_while_a_credential_is_saved() {
    let persisted = PersistedConfig {
        backend_url: Some("https://saved.test".into()),
        client_access_token: Some("client-token".into()),
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
fn explicit_legacy_backend_replacement_ignores_invalid_saved_backend() {
    let persisted = PersistedConfig {
        backend_url: Some("not a URL".into()),
        client_access_token: Some("stale-client-token".into()),
        ..Default::default()
    };
    let merged = Resolved::merge(
        persisted,
        Resolved {
            backend_url: Some("https://replacement.test".into()),
            api_key: Some("legacy-key".into()),
            client_access_token: None,
            client_installation_id: None,
            edison_secret_key: None,
            device_id: None,
            device_label: None,
        },
    )
    .unwrap();
    assert_eq!(
        merged.backend_url.as_deref(),
        Some("https://replacement.test")
    );
    assert!(merged.client_access_token.is_none());
    assert!(merged.client_installation_id.is_none());
}

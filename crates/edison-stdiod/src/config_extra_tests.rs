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
}

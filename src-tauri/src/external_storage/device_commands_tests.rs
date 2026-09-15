use super::*;

fn scope(settings: bool, plugins: bool) -> Scope {
    Scope {
        library: true,
        referenced_assets: true,
        device_settings: settings,
        device_plugins: plugins,
    }
}

#[test]
fn exact_scope_requires_settings_and_both_plugin_stores() {
    assert!(validate_sections(&scope(false, false), &[]).is_ok());
    assert!(validate_sections(&scope(true, false), &["device-settings".into()]).is_ok());
    assert!(validate_sections(&scope(true, false), &["local-storage".into()]).is_err());
    assert!(validate_sections(
        &scope(false, true),
        &["local-storage".into(), "localforage".into()]
    )
    .is_ok());
    assert!(validate_sections(&scope(false, true), &["local-storage".into()]).is_err());
}

#[test]
fn section_request_rejects_duplicates_and_unscoped_names() {
    assert!(validate_sections(
        &scope(true, true),
        &[
            "device-settings".into(),
            "local-storage".into(),
            "localforage".into(),
            "indexed-db:0073006100660065005f0070006c007500670069006e005f0078".into(),
        ]
    )
    .is_ok());
    assert!(validate_sections(
        &scope(false, true),
        &[
            "local-storage".into(),
            "local-storage".into(),
            "localforage".into()
        ]
    )
    .is_err());
    assert!(validate_sections(
        &scope(false, true),
        &["local-storage".into(), "localforage".into(), "other".into()]
    )
    .is_err());
}

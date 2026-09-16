// SQL expressions use only schema-owned identifiers, never input values.
pub(super) fn tracked_tables() -> Vec<(&'static str, &'static str, &'static str, &'static str)> {
    vec![
        ("root", "'root'", "''", "''"),
        ("bot_presets", "'preset'", "ROW.preset_id", "''"),
        ("characters", "'character'", "ROW.character_id", "''"),
        (
            "conversations",
            "'conversation'",
            "ROW.character_id",
            "ROW.conversation_id",
        ),
        (
            "messages",
            "'conversation'",
            "ROW.character_id",
            "ROW.conversation_id",
        ),
        ("plugin_storage", "'plugin'", "ROW.owner", "ROW.storage_key"),
        ("asset_aliases", "ROW.kind", "ROW.logical_key", "''"),
        (
            "asset_owner_heads",
            "'owner'",
            "ROW.owner_kind",
            "ROW.owner_locator",
        ),
        ("asset_repository_authority", "'full'", "''", "''"),
    ]
}

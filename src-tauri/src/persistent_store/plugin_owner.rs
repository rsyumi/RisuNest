//! Plugin storage ownership.
//!
//! Every `plugin_storage` row belongs to the plugin whose `//@name` banner
//! matches `owner`. Rows that arrived through an upstream RisuSave without an
//! ownership sidecar carry the sentinel below. The banner is parsed line by
//! line, so a real plugin name can never contain NUL and the sentinel cannot be
//! forged from plugin code.

/// Owner of a row whose plugin is not known.
pub(crate) const UNOWNED_OWNER: &str = "\u{0000}unowned";

/// `claimed_from` value written when an initialization session moves a row away
/// from the sentinel.
pub(crate) const CLAIMED_FROM_UNOWNED: &str = "unowned";

pub(crate) fn is_unowned(owner: &str) -> bool {
    owner == UNOWNED_OWNER
}

/// Rejects names that could collide with the sentinel or overflow a key
/// component. Callers validate before writing so a forged owner cannot enter
/// the table through an import path.
pub(crate) fn validate_owner(owner: &str) -> bool {
    if owner == UNOWNED_OWNER {
        return true;
    }
    !owner.is_empty() && owner.len() <= 512 && !owner.contains('\0')
}

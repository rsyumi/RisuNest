pub(crate) mod backups;
pub(crate) mod cache;
pub(crate) mod client;
pub(crate) mod commands;
pub(crate) mod credentials;
pub(crate) mod management;
pub(crate) mod media;
pub(crate) mod planner;
pub(crate) mod remote;
pub(crate) mod residency;
#[cfg(test)]
mod tests;
pub(crate) mod transfer;

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SyncError {
    pub code: String,
    pub status: u16,
}
impl SyncError {
    pub fn new(code: impl Into<String>, status: u16) -> Self {
        Self {
            code: code.into(),
            status,
        }
    }
}
impl From<risunest_sync_wire::WireError> for SyncError {
    fn from(value: risunest_sync_wire::WireError) -> Self {
        Self::new(value.0, 400)
    }
}
impl From<std::io::Error> for SyncError {
    fn from(_: std::io::Error) -> Self {
        Self::new("local-storage", 503)
    }
}
impl From<rusqlite::Error> for SyncError {
    fn from(_: rusqlite::Error) -> Self {
        Self::new("local-metadata", 503)
    }
}
impl From<crate::persistent_store::StoreError> for SyncError {
    fn from(value: crate::persistent_store::StoreError) -> Self {
        match value {
            crate::persistent_store::StoreError::RevisionConflict { .. } => {
                Self::new("local-revision-changed", 409)
            }
            _ => Self::new("local-validation", 409),
        }
    }
}
pub(crate) type Result<T> = std::result::Result<T, SyncError>;

impl From<risunest_sync_connect::ConnectError> for SyncError {
    fn from(value: risunest_sync_connect::ConnectError) -> Self {
        Self::new(value.0, 400)
    }
}

// The android_* modules are compiled on desktop only so tests can exercise
// them; the Android build itself keeps full dead-code checking, and the
// desktop-test mirror compile must not flag Android-only entry points.
#[cfg(any(target_os = "android", test))]
mod android_client;
#[cfg(any(target_os = "android", test))]
#[cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports))]
pub(crate) mod android_commands;
#[cfg(any(desktop, target_os = "android", test))]
pub(crate) mod android_foreground;
#[cfg(any(target_os = "android", test))]
#[cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports))]
mod android_jni;
#[cfg(any(target_os = "android", test))]
#[cfg_attr(not(target_os = "android"), allow(dead_code, unused_imports))]
pub(crate) mod android_source_commands;
#[cfg(any(desktop, target_os = "android"))]
pub(crate) mod bidirectional_commands;
mod client;
#[cfg(desktop)]
pub(crate) mod commands;
#[cfg(any(desktop, target_os = "android"))]
pub(crate) mod delta_commands;
// The loopback host is a test-only transport double; production clone hosting
// goes through lan::LanCloneHost.
#[cfg(all(desktop, test))]
mod host;
mod http_stream;
mod lan;
pub(crate) mod logical_delta;
mod logical_delta_transfer;
pub(crate) mod maintenance;
#[cfg(any(desktop, target_os = "android", test))]
mod production;
mod protocol;
mod session;

#[cfg(desktop)]
mod tunnel;
#[cfg(any(desktop, target_os = "android"))]
pub(crate) mod tunnel_lifecycle;

#[cfg(test)]
mod maintenance_tests;
#[cfg(all(test, desktop))]
mod tests;

#[cfg(test)]
pub use android_client::AndroidCloneJobPhase;
#[cfg(any(target_os = "android", test))]
pub use android_client::AndroidResumableCloneJob;
#[cfg(any(target_os = "android", test))]
pub use client::DownloadReport;
pub use client::{
    activate_downloaded_clone, CloneActivation, CloneTargetAdapter, CloneValidator,
    LoopbackCloneClient, TransferCancellation,
};
#[cfg(all(desktop, test))]
pub use host::LoopbackCloneHost;
pub use lan::LanCloneClient;
#[cfg(any(desktop, target_os = "android"))]
pub use lan::{LanCloneHost, LanPairing};
#[cfg(test)]
pub use logical_delta_transfer::execute_logical_delta_pull;
pub use logical_delta_transfer::{
    LogicalDeltaActivation, LogicalDeltaApplyOperation, LogicalDeltaObject,
    LogicalDeltaObjectSource, LogicalDeltaStagedTarget, LogicalDeltaTransferSelection,
    ReadyLogicalDeltaPlan,
};
#[cfg(any(desktop, target_os = "android"))]
pub(crate) use production::prepare_lossless_clone_session;
#[cfg(any(desktop, target_os = "android", test))]
pub(crate) use production::LosslessCloneTargetAdapter;
pub use protocol::{CloneManifest, CLONE_LOSSLESS_DATABASE_FORMAT};
#[cfg(test)]
pub use protocol::{CloneObjectKind, CLONE_CHUNK_SIZE};
pub use session::{
    prepare_clone_session, CloneSource, PinnedCloneRevision, PinnedSourceObject,
    PreparedCloneSession,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerSyncError {
    Cancelled,
    ChunkHashMismatch {
        object: String,
        offset: u64,
    },
    WholeObjectHashMismatch {
        object: String,
    },
    StaleManifest {
        expected: String,
        received: String,
    },
    ActivationConflict {
        expected: Option<String>,
        actual: Option<String>,
    },
    LogicalMergeConflict {
        record: String,
    },
    AlreadyActivated,
    Protocol(String),
    Storage(String),
    Transport(String),
    Validation(String),
}

impl std::fmt::Display for PeerSyncError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for PeerSyncError {}

impl From<std::io::Error> for PeerSyncError {
    fn from(error: std::io::Error) -> Self {
        Self::Storage(error.to_string())
    }
}
#[cfg(any(target_os = "android", test))]
mod target_foreground_transition;

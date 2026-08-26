mod android_client;
#[cfg(target_os = "android")]
mod android_jni;
mod client;
#[cfg(desktop)]
pub(crate) mod commands;
#[cfg(desktop)]
mod host;
mod http_stream;
mod lan;
#[cfg(desktop)]
mod production;
mod protocol;
mod session;

// P2 stays private until the P1 peer server provides a production route.
#[cfg(desktop)]
#[allow(dead_code)]
mod tunnel;

#[cfg(all(test, desktop))]
mod tests;

pub use android_client::{AndroidCloneJobPhase, AndroidResumableCloneJob};
pub use client::{
    activate_downloaded_clone, CloneActivation, CloneTargetAdapter, CloneValidator, DownloadReport,
    LoopbackCloneClient, TransferCancellation,
};
#[cfg(desktop)]
pub use host::LoopbackCloneHost;
pub use lan::LanCloneClient;
#[cfg(desktop)]
pub use lan::{LanCloneHost, LanDevice, LanPairing};
#[cfg(desktop)]
pub(crate) use production::{prepare_lossless_clone_session, LosslessCloneTargetAdapter};
pub use protocol::{
    CloneDatabase, CloneManifest, CloneObjectKind, ClonePayload, ObjectDescriptor, VerifiedChunk,
    CLONE_CHUNK_SIZE, CLONE_LOSSLESS_DATABASE_FORMAT,
};
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

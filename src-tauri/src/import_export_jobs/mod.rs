#[allow(dead_code)]
pub(crate) mod png_card;
mod classifier;
mod error;
mod json_card;
mod risum;
mod staging;

pub use classifier::{classify_content, ContentKind};
pub use error::{FormatError, FormatErrorKind};
pub use json_card::{parse_json_card, JsonCardPayload, ParsedJsonCard};
pub use risum::{decode_rpack, parse_risum, ParsedRisum, RisumAsset};
pub use staging::{JobStaging, StagedPayload};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportLimits {
    pub max_metadata_bytes: usize,
    pub max_payload_bytes: u64,
    pub max_aggregate_payload_bytes: u64,
    pub max_payload_count: usize,
    pub max_container_entries: usize,
    pub max_container_directory_bytes: u64,
    pub charx_probe_metadata_bytes: u64,
}

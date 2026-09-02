use super::PeerSyncError;
use crate::trust_boundary::{is_link_like, is_lower_hex_256, sync_directory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_REGISTRY_BYTES: usize = 1024 * 1024;
const MAX_DEVICE_ID_BYTES: usize = 64;
const OUTGOING_SCHEMA: &str = "risunest.peer-device-registry/v1";
const INCOMING_SCHEMA: &str = "risunest.peer-source-registry/v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionLane {
    Clone,
    Delta,
    Bidirectional,
}

impl CompletionLane {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Clone => "clone",
            Self::Delta => "delta",
            Self::Bidirectional => "bidirectional",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, PeerSyncError> {
        match value {
            "clone" => Ok(Self::Clone),
            "delta" => Ok(Self::Delta),
            "bidirectional" => Ok(Self::Bidirectional),
            _ => invalid("invalid peer completion receipt lane"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionSealStatus {
    Sealed,
    AlreadyCompleted,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletionLeaseId(String);

impl CompletionLeaseId {
    pub(crate) fn parse(value: &str) -> Result<Self, PeerSyncError> {
        let parsed = uuid::Uuid::parse_str(value)
            .map_err(|_| PeerSyncError::Protocol("invalid peer completion lease".to_owned()))?;
        if parsed.get_version_num() != 4
            || parsed.get_variant() != uuid::Variant::RFC4122
            || parsed.to_string() != value
        {
            return Err(PeerSyncError::Protocol(
                "invalid peer completion lease".to_owned(),
            ));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionAcceptance {
    Recorded,
    AlreadyRecorded,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionDeliveryPrepareStatus {
    Pending,
    AlreadyDurable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompletionDeliveryFinalizeStatus {
    Finalized,
    AlreadyFinalized,
}

fn outgoing_registry_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn incoming_registry_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DevicePermissions(Vec<String>);

impl DevicePermissions {
    pub(crate) fn read() -> Self {
        Self(vec!["read".to_owned()])
    }

    pub(crate) fn read_and_bidirectional() -> Self {
        Self(vec!["read".to_owned(), "bidirectional".to_owned()])
    }

    pub(crate) fn allows_read(&self) -> bool {
        self.0.iter().any(|permission| permission == "read")
    }

    pub(crate) fn allows_bidirectional(&self) -> bool {
        self.0
            .iter()
            .any(|permission| permission == "bidirectional")
    }

    pub(crate) fn from_values(values: Vec<String>) -> Result<Self, PeerSyncError> {
        let permissions = Self(values);
        permissions.validate()?;
        Ok(permissions)
    }

    pub(crate) fn values(&self) -> &[String] {
        &self.0
    }

    fn validate(&self) -> Result<(), PeerSyncError> {
        if self.0.is_empty()
            || !self.allows_read()
            || self.0.len()
                != self
                    .0
                    .iter()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            || self
                .0
                .iter()
                .any(|permission| !matches!(permission.as_str(), "read" | "bidirectional"))
        {
            return Err(PeerSyncError::Validation(
                "invalid peer device permissions".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OutgoingDevice {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) bearer_digest: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) created_at_ms: u64,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct IncomingSource {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) endpoint: String,
    pub(crate) bearer: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PendingCompletionDelivery {
    pub(crate) source_device_id: String,
    pub(crate) lane: String,
    pub(crate) completion_lease_id: String,
    pub(crate) manifest_id: String,
    pub(crate) useful_bytes: u64,
    pub(crate) receipt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncomingCompletionDeliverySnapshot {
    pub(crate) source: IncomingSource,
    pub(crate) delivery: PendingCompletionDelivery,
}

// These are the only registry records that cross the native command boundary.
// Credentials and endpoints remain native-only so a future controller can use
// them without exposing them to the WebView.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OutgoingDeviceSummary {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) created_at_ms: u64,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IncomingSourceSummary {
    pub(crate) device_id: String,
    pub(crate) name: String,
    pub(crate) permissions: DevicePermissions,
    pub(crate) last_seen_ms: u64,
    pub(crate) total_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutgoingFile {
    schema: String,
    devices: Vec<OutgoingDevice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completed_receipts: Vec<CompletionReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completion_offers: Vec<CompletionOffer>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IncomingFile {
    schema: String,
    sources: Vec<IncomingSource>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    completed_receipts: Vec<CompletionReceipt>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pending_completion_deliveries: Vec<PendingCompletionDelivery>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompletionReceipt {
    device_id: String,
    // The protocol permits at most one active operation per device and lane. Keeping
    // only that lane's latest proof bounds each device to these three receipt heads.
    lane: String,
    receipt_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transferred_bytes: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompletionOffer {
    device_id: String,
    bearer_digest: String,
    // One current lease is retained per device and lane. A completed lease stays
    // as the replay anchor until an explicit opt-in manifest request rotates it.
    lane: String,
    lease_id: String,
    manifest_id: String,
    // This is a source-derived useful-data acknowledgement by an authorized peer.
    // It is not a cryptographic proof that the target activated the dataset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transferred_bytes: Option<u64>,
    #[serde(default)]
    ready: bool,
}

pub(crate) struct OutgoingDeviceRegistry {
    root: PathBuf,
    devices: Vec<OutgoingDevice>,
    completed_receipts: Vec<CompletionReceipt>,
    completion_offers: Vec<CompletionOffer>,
}

impl OutgoingDeviceRegistry {
    pub(crate) fn load(app_root: &Path) -> Result<Self, PeerSyncError> {
        let root = ensure_peer_root(app_root)?;
        let path = root.join("devices.json");
        let (devices, completed_receipts, completion_offers) = match read_registry(&path)? {
            None => (Vec::new(), Vec::new(), Vec::new()),
            Some(bytes) => {
                let file: OutgoingFile = parse_registry(&bytes)?;
                if file.schema != OUTGOING_SCHEMA {
                    return invalid("unsupported outgoing peer device registry schema");
                }
                (
                    file.devices,
                    file.completed_receipts,
                    file.completion_offers,
                )
            }
        };
        validate_outgoing(&devices)?;
        validate_receipts(
            &completed_receipts,
            devices.iter().map(|device| device.device_id.as_str()),
        )?;
        validate_completion_offers(
            &completion_offers,
            devices
                .iter()
                .map(|device| (device.device_id.as_str(), device.bearer_digest.as_str())),
        )?;
        Ok(Self {
            root,
            devices,
            completed_receipts,
            completion_offers,
        })
    }

    pub(crate) fn devices(&self) -> &[OutgoingDevice] {
        &self.devices
    }

    pub(crate) fn upsert(&mut self, device: OutgoingDevice) -> Result<(), PeerSyncError> {
        validate_outgoing(std::slice::from_ref(&device))?;
        if let Some(existing) = self
            .devices
            .iter_mut()
            .find(|item| item.device_id == device.device_id)
        {
            *existing = device;
        } else {
            self.devices.push(device);
        }
        Ok(())
    }

    pub(crate) fn register_claim(
        &mut self,
        mut device: OutgoingDevice,
    ) -> Result<(), PeerSyncError> {
        validate_outgoing(std::slice::from_ref(&device))?;
        if let Some(index) = self
            .devices
            .iter()
            .position(|item| item.device_id == device.device_id)
        {
            let existing = &self.devices[index];
            let credential_changed = existing.bearer_digest != device.bearer_digest;
            if credential_changed
                && (self
                    .completion_offers
                    .iter()
                    .any(|offer| offer.device_id == existing.device_id)
                    || self
                        .completed_receipts
                        .iter()
                        .any(|receipt| receipt.device_id == existing.device_id))
            {
                return invalid("peer completion state blocks credential rotation");
            }
            device.created_at_ms = existing.created_at_ms;
            device.last_seen_ms = existing.last_seen_ms;
            device.total_bytes = existing.total_bytes;
            self.devices[index] = device;
            if credential_changed {
                self.completion_offers
                    .retain(|offer| offer.device_id != self.devices[index].device_id);
            }
        } else {
            self.devices.push(device);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, device_id: &str) -> Result<(), PeerSyncError> {
        validate_id(device_id)?;
        self.devices.retain(|item| item.device_id != device_id);
        self.completed_receipts
            .retain(|receipt| receipt.device_id != device_id);
        self.completion_offers
            .retain(|offer| offer.device_id != device_id);
        Ok(())
    }

    pub(crate) fn record_completed_bytes(
        &mut self,
        device_id: &str,
        bytes: u64,
    ) -> Result<(), PeerSyncError> {
        let device = self
            .devices
            .iter_mut()
            .find(|item| item.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        device.total_bytes = device
            .total_bytes
            .checked_add(bytes)
            .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
        Ok(())
    }

    pub(crate) fn record_completed_operation(
        &mut self,
        device_id: &str,
        lane: &str,
        receipt_id: &str,
        bytes: u64,
        seen_at_ms: u64,
    ) -> Result<(), PeerSyncError> {
        validate_id(device_id)?;
        validate_receipt_lane(lane)?;
        validate_receipt_id(receipt_id)?;
        if self
            .completion_offers
            .iter()
            .any(|offer| offer.device_id == device_id && offer.lane == lane)
        {
            return invalid("deferred peer completion lease is active");
        }
        let mut devices = self.devices.clone();
        let mut receipts = self.completed_receipts.clone();
        let device = devices
            .iter_mut()
            .find(|item| item.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        device.last_seen_ms = device.last_seen_ms.max(seen_at_ms);
        let retained = receipts
            .iter_mut()
            .find(|receipt| receipt.device_id == device_id && receipt.lane == lane);
        if !retained
            .as_ref()
            .is_some_and(|receipt| receipt.receipt_id == receipt_id)
        {
            device.total_bytes = device
                .total_bytes
                .checked_add(bytes)
                .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
            if let Some(retained) = retained {
                retained.receipt_id = receipt_id.to_owned();
                retained.transferred_bytes = Some(bytes);
            } else {
                receipts.push(CompletionReceipt {
                    device_id: device_id.to_owned(),
                    lane: lane.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    transferred_bytes: Some(bytes),
                });
            }
        }
        self.write_state(&devices, &receipts, &self.completion_offers)?;
        self.devices = devices;
        self.completed_receipts = receipts;
        Ok(())
    }

    pub(crate) fn issue_completion_lease(
        &mut self,
        device_id: &str,
        lane: CompletionLane,
        manifest_id: &str,
        transferred_bytes: Option<u64>,
        resume_lease_id: Option<&str>,
    ) -> Result<CompletionLeaseId, PeerSyncError> {
        validate_id(device_id)?;
        if !matches!(
            (lane, transferred_bytes),
            (CompletionLane::Clone, Some(_))
                | (CompletionLane::Delta | CompletionLane::Bidirectional, None)
        ) {
            return invalid("invalid peer completion lease measurement state");
        }
        if !is_lower_hex_256(manifest_id) {
            return invalid("invalid peer completion manifest identity");
        }
        if resume_lease_id.is_some_and(|lease_id| {
            uuid::Uuid::parse_str(lease_id)
                .map(|parsed| {
                    parsed.get_version_num() != 4
                        || parsed.get_variant() != uuid::Variant::RFC4122
                        || parsed.to_string() != lease_id
                })
                .unwrap_or(true)
        }) {
            return invalid("invalid peer completion resume lease");
        }
        let device = self
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        if lane == CompletionLane::Bidirectional && !device.permissions.allows_bidirectional() {
            return invalid("peer completion lane permission denied");
        }
        let lane = lane.as_str();
        let current_index = self
            .completion_offers
            .iter()
            .position(|offer| offer.device_id == device_id && offer.lane == lane);
        if let Some(index) = current_index {
            let current = &self.completion_offers[index];
            let current_receipt_id =
                completion_receipt_id(lane, &current.lease_id, &current.manifest_id);
            let completed = self.completed_receipts.iter().any(|receipt| {
                receipt.device_id == device_id
                    && receipt.lane == lane
                    && receipt.receipt_id == current_receipt_id
                    && current.transferred_bytes.is_some()
                    && receipt.transferred_bytes == current.transferred_bytes
            });
            let exact_payload = current.manifest_id == manifest_id
                && transferred_bytes
                    .map(|bytes| current.transferred_bytes == Some(bytes))
                    .unwrap_or(true);
            let exact_resume = resume_lease_id == Some(current.lease_id.as_str());
            if completed {
                if exact_resume && exact_payload {
                    return Ok(CompletionLeaseId(current.lease_id.clone()));
                }
                if resume_lease_id.is_some() {
                    return invalid("stale peer completion resume lease");
                }
            } else if current.ready {
                if exact_resume && exact_payload {
                    return Ok(CompletionLeaseId(current.lease_id.clone()));
                }
                return invalid("ready peer completion lease is unresolved");
            } else if exact_payload {
                if resume_lease_id.is_none() || exact_resume {
                    return Ok(CompletionLeaseId(current.lease_id.clone()));
                }
                return invalid("stale peer completion resume lease");
            } else if resume_lease_id.is_some() {
                return invalid("stale peer completion resume lease");
            }
        } else if resume_lease_id.is_some() {
            return invalid("stale peer completion resume lease");
        }

        let lease_id = uuid::Uuid::new_v4().to_string();
        let mut offers = self.completion_offers.clone();
        let replacement = CompletionOffer {
            device_id: device_id.to_owned(),
            bearer_digest: device.bearer_digest.clone(),
            lane: lane.to_owned(),
            lease_id: lease_id.clone(),
            manifest_id: manifest_id.to_owned(),
            transferred_bytes,
            ready: false,
        };
        if let Some(index) = current_index {
            offers[index] = replacement;
        } else {
            offers.push(replacement);
        }
        self.write_state(&self.devices, &self.completed_receipts, &offers)?;
        self.completion_offers = offers;
        Ok(CompletionLeaseId(lease_id))
    }

    pub(crate) fn has_completion_lease(
        &self,
        device_id: &str,
        lane: CompletionLane,
    ) -> Result<bool, PeerSyncError> {
        validate_id(device_id)?;
        Ok(self
            .completion_offers
            .iter()
            .any(|offer| offer.device_id == device_id && offer.lane == lane.as_str()))
    }

    pub(crate) fn has_exact_completion_lease(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
    ) -> Result<bool, PeerSyncError> {
        validate_completion_tuple(device_id, lane, lease_id, manifest_id)?;
        Ok(self.completion_offers.iter().any(|offer| {
            offer.device_id == device_id
                && offer.lane == lane.as_str()
                && offer.lease_id == lease_id
                && offer.manifest_id == manifest_id
        }))
    }

    pub(crate) fn completion_lease_ready_bytes(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
    ) -> Result<Option<u64>, PeerSyncError> {
        validate_completion_tuple(device_id, lane, lease_id, manifest_id)?;
        Ok(self
            .completion_offers
            .iter()
            .find(|offer| {
                offer.device_id == device_id
                    && offer.lane == lane.as_str()
                    && offer.lease_id == lease_id
                    && offer.manifest_id == manifest_id
                    && offer.ready
            })
            .and_then(|offer| offer.transferred_bytes))
    }

    pub(crate) fn has_exact_completion_receipt(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
        transferred_bytes: u64,
    ) -> Result<bool, PeerSyncError> {
        validate_completion_tuple(device_id, lane, lease_id, manifest_id)?;
        let receipt_id = completion_receipt_id(lane.as_str(), lease_id, manifest_id);
        Ok(self.completed_receipts.iter().any(|receipt| {
            receipt.device_id == device_id
                && receipt.lane == lane.as_str()
                && receipt.receipt_id == receipt_id
                && receipt.transferred_bytes == Some(transferred_bytes)
        }))
    }

    pub(crate) fn completion_receipt_bytes(
        &self,
        device_id: &str,
        lane: CompletionLane,
        lease_id: &str,
        manifest_id: &str,
    ) -> Result<Option<u64>, PeerSyncError> {
        validate_completion_tuple(device_id, lane, lease_id, manifest_id)?;
        let receipt_id = completion_receipt_id(lane.as_str(), lease_id, manifest_id);
        Ok(self
            .completed_receipts
            .iter()
            .find(|receipt| {
                receipt.device_id == device_id
                    && receipt.lane == lane.as_str()
                    && receipt.receipt_id == receipt_id
            })
            .and_then(|receipt| receipt.transferred_bytes))
    }

    pub(crate) fn completion_lease_allows_remote_apply(
        &self,
        device_id: &str,
        lease_id: &str,
        manifest_id: &str,
    ) -> Result<bool, PeerSyncError> {
        let lane = CompletionLane::Bidirectional;
        validate_completion_tuple(device_id, lane, lease_id, manifest_id)?;
        let Some(offer) = self.completion_offers.iter().find(|offer| {
            offer.device_id == device_id
                && offer.lane == lane.as_str()
                && offer.lease_id == lease_id
                && offer.manifest_id == manifest_id
        }) else {
            return Ok(false);
        };
        let completed = offer.transferred_bytes.is_some_and(|transferred_bytes| {
            let receipt_id = completion_receipt_id(lane.as_str(), lease_id, &offer.manifest_id);
            self.completed_receipts.iter().any(|receipt| {
                receipt.device_id == device_id
                    && receipt.lane == lane.as_str()
                    && receipt.receipt_id == receipt_id
                    && receipt.transferred_bytes == Some(transferred_bytes)
            })
        });
        Ok(!completed)
    }

    pub(crate) fn seal_completion_lease(
        &mut self,
        device_id: &str,
        lane: CompletionLane,
        operation_id: &str,
        manifest_id: &str,
        transferred_bytes: u64,
    ) -> Result<CompletionSealStatus, PeerSyncError> {
        validate_completion_tuple(device_id, lane, operation_id, manifest_id)?;
        let device = self
            .devices
            .iter()
            .find(|device| device.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        if lane == CompletionLane::Bidirectional && !device.permissions.allows_bidirectional() {
            return Ok(CompletionSealStatus::Conflict);
        }
        let lane = lane.as_str();
        let receipt_id = completion_receipt_id(lane, operation_id, manifest_id);
        let Some(index) = self
            .completion_offers
            .iter()
            .position(|offer| offer.device_id == device_id && offer.lane == lane)
        else {
            return Ok(CompletionSealStatus::Conflict);
        };
        if self.completion_offers[index].device_id != device_id
            || self.completion_offers[index].lane != lane
            || self.completion_offers[index].lease_id != operation_id
            || self.completion_offers[index].manifest_id != manifest_id
            || self.completion_offers[index]
                .transferred_bytes
                .is_some_and(|bytes| bytes != transferred_bytes)
        {
            return Ok(CompletionSealStatus::Conflict);
        }
        if self.completed_receipts.iter().any(|receipt| {
            receipt.device_id == device_id
                && receipt.lane == lane
                && receipt.receipt_id == receipt_id
                && receipt.transferred_bytes == Some(transferred_bytes)
        }) {
            return Ok(CompletionSealStatus::AlreadyCompleted);
        }
        if self.completion_offers[index].ready {
            return Ok(CompletionSealStatus::Sealed);
        }
        let mut offers = self.completion_offers.clone();
        offers[index].transferred_bytes = Some(transferred_bytes);
        offers[index].ready = true;
        self.write_state(&self.devices, &self.completed_receipts, &offers)?;
        self.completion_offers = offers;
        Ok(CompletionSealStatus::Sealed)
    }

    pub(crate) fn accept_completion_offer(
        &mut self,
        device_id: &str,
        lane: CompletionLane,
        operation_id: &str,
        manifest_id: &str,
        transferred_bytes: u64,
        seen_at_ms: u64,
    ) -> Result<CompletionAcceptance, PeerSyncError> {
        validate_completion_tuple(device_id, lane, operation_id, manifest_id)?;
        let lane = lane.as_str();
        let receipt_id = completion_receipt_id(lane, operation_id, manifest_id);
        let exact_ready_offer = self.completion_offers.iter().any(|offer| {
            offer.ready
                && completion_offer_matches(
                    offer,
                    device_id,
                    lane,
                    operation_id,
                    manifest_id,
                    transferred_bytes,
                )
        });
        if !exact_ready_offer {
            return Ok(CompletionAcceptance::Rejected);
        }
        if let Some(receipt) = self
            .completed_receipts
            .iter()
            .find(|receipt| receipt.device_id == device_id && receipt.lane == lane)
        {
            if receipt.receipt_id == receipt_id {
                return Ok(if receipt.transferred_bytes == Some(transferred_bytes) {
                    CompletionAcceptance::AlreadyRecorded
                } else {
                    CompletionAcceptance::Rejected
                });
            }
        }

        let mut devices = self.devices.clone();
        let mut receipts = self.completed_receipts.clone();
        let device = devices
            .iter_mut()
            .find(|device| device.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        device.total_bytes = device
            .total_bytes
            .checked_add(transferred_bytes)
            .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
        device.last_seen_ms = device.last_seen_ms.max(seen_at_ms);
        if let Some(receipt) = receipts
            .iter_mut()
            .find(|receipt| receipt.device_id == device_id && receipt.lane == lane)
        {
            receipt.receipt_id = receipt_id;
            receipt.transferred_bytes = Some(transferred_bytes);
        } else {
            receipts.push(CompletionReceipt {
                device_id: device_id.to_owned(),
                lane: lane.to_owned(),
                receipt_id,
                transferred_bytes: Some(transferred_bytes),
            });
        }
        self.write_state(&devices, &receipts, &self.completion_offers)?;
        self.devices = devices;
        self.completed_receipts = receipts;
        Ok(CompletionAcceptance::Recorded)
    }

    pub(crate) fn record_seen(
        &mut self,
        device_id: &str,
        seen_at_ms: u64,
    ) -> Result<(), PeerSyncError> {
        validate_id(device_id)?;
        let mut devices = self.devices.clone();
        let device = devices
            .iter_mut()
            .find(|item| item.device_id == device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered outgoing device is missing".to_owned())
            })?;
        device.last_seen_ms = device.last_seen_ms.max(seen_at_ms);
        self.write_state(&devices, &self.completed_receipts, &self.completion_offers)?;
        self.devices = devices;
        Ok(())
    }

    pub(crate) fn save(&self) -> Result<(), PeerSyncError> {
        self.write_state(
            &self.devices,
            &self.completed_receipts,
            &self.completion_offers,
        )
    }

    fn write_state(
        &self,
        devices: &[OutgoingDevice],
        completed_receipts: &[CompletionReceipt],
        completion_offers: &[CompletionOffer],
    ) -> Result<(), PeerSyncError> {
        write_registry(
            &self.root.join("devices.json"),
            &OutgoingFile {
                schema: OUTGOING_SCHEMA.to_owned(),
                devices: devices.to_vec(),
                completed_receipts: completed_receipts.to_vec(),
                completion_offers: completion_offers.to_vec(),
            },
        )
    }
}

pub(crate) struct IncomingSourceRegistry {
    root: PathBuf,
    sources: Vec<IncomingSource>,
    completed_receipts: Vec<CompletionReceipt>,
    pending_completion_deliveries: Vec<PendingCompletionDelivery>,
}

impl IncomingSourceRegistry {
    pub(crate) fn load(app_root: &Path) -> Result<Self, PeerSyncError> {
        let root = ensure_peer_root(app_root)?;
        let path = root.join("sources.json");
        let (sources, completed_receipts, pending_completion_deliveries) =
            match read_registry(&path)? {
                None => (Vec::new(), Vec::new(), Vec::new()),
                Some(bytes) => {
                    let file: IncomingFile = parse_registry(&bytes)?;
                    if file.schema != INCOMING_SCHEMA {
                        return invalid("unsupported incoming peer source registry schema");
                    }
                    (
                        file.sources,
                        file.completed_receipts,
                        file.pending_completion_deliveries,
                    )
                }
            };
        validate_incoming(&sources)?;
        validate_incoming_receipts(
            &completed_receipts,
            sources.iter().map(|source| source.device_id.as_str()),
        )?;
        validate_pending_completion_deliveries(
            &pending_completion_deliveries,
            &completed_receipts,
            sources.iter().map(|source| source.device_id.as_str()),
        )?;
        Ok(Self {
            root,
            sources,
            completed_receipts,
            pending_completion_deliveries,
        })
    }

    pub(crate) fn sources(&self) -> &[IncomingSource] {
        &self.sources
    }

    pub(crate) fn upsert(&mut self, mut source: IncomingSource) -> Result<(), PeerSyncError> {
        validate_incoming(std::slice::from_ref(&source))?;
        if let Some(existing) = self
            .sources
            .iter_mut()
            .find(|item| item.device_id == source.device_id)
        {
            if existing.bearer != source.bearer
                && self
                    .pending_completion_deliveries
                    .iter()
                    .any(|delivery| delivery.source_device_id == source.device_id)
            {
                return invalid(
                    "incoming source credential cannot rotate while completion delivery is pending",
                );
            }
            source.last_seen_ms = existing.last_seen_ms;
            source.total_bytes = existing.total_bytes;
            *existing = source;
        } else {
            self.sources.push(source);
        }
        Ok(())
    }

    pub(crate) fn remove(&mut self, device_id: &str) -> Result<(), PeerSyncError> {
        validate_id(device_id)?;
        if self
            .pending_completion_deliveries
            .iter()
            .any(|delivery| delivery.source_device_id == device_id)
        {
            return invalid("incoming source has a pending completion delivery");
        }
        self.sources.retain(|item| item.device_id != device_id);
        self.completed_receipts
            .retain(|receipt| receipt.device_id != device_id);
        Ok(())
    }

    pub(crate) fn record_completed_operation(
        &mut self,
        source_id: &str,
        bytes: u64,
        seen_at_ms: u64,
    ) -> Result<(), PeerSyncError> {
        validate_id(source_id)?;
        if self
            .pending_completion_deliveries
            .iter()
            .any(|delivery| delivery.source_device_id == source_id)
        {
            return invalid("incoming source has a pending completion delivery");
        }
        let mut updated = self.sources.clone();
        let source = updated
            .iter_mut()
            .find(|item| item.device_id == source_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered incoming source is missing".to_owned())
            })?;
        source.total_bytes = source
            .total_bytes
            .checked_add(bytes)
            .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
        source.last_seen_ms = seen_at_ms;
        write_registry(
            &self.root.join("sources.json"),
            &IncomingFile {
                schema: INCOMING_SCHEMA.to_owned(),
                sources: updated.clone(),
                completed_receipts: self.completed_receipts.clone(),
                pending_completion_deliveries: self.pending_completion_deliveries.clone(),
            },
        )?;
        self.sources = updated;
        Ok(())
    }

    pub(crate) fn record_completed_operation_once(
        &mut self,
        source_id: &str,
        receipt_id: &str,
        bytes: u64,
        seen_at_ms: u64,
    ) -> Result<(), PeerSyncError> {
        self.record_completed_operation_once_for_lane(
            source_id,
            CompletionLane::Clone,
            receipt_id,
            bytes,
            seen_at_ms,
        )
    }

    pub(crate) fn record_completed_operation_once_for_lane(
        &mut self,
        source_id: &str,
        lane: CompletionLane,
        receipt_id: &str,
        bytes: u64,
        seen_at_ms: u64,
    ) -> Result<(), PeerSyncError> {
        validate_id(source_id)?;
        validate_receipt_id(receipt_id)?;
        let lane = lane.as_str();
        if self
            .pending_completion_deliveries
            .iter()
            .any(|delivery| delivery.source_device_id == source_id && delivery.lane == lane)
        {
            return invalid("incoming source lane has a pending completion delivery");
        }
        let mut sources = self.sources.clone();
        let mut receipts = self.completed_receipts.clone();
        let source = sources
            .iter_mut()
            .find(|item| item.device_id == source_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered incoming source is missing".to_owned())
            })?;
        source.last_seen_ms = source.last_seen_ms.max(seen_at_ms);
        let retained = receipts
            .iter_mut()
            .find(|receipt| receipt.device_id == source_id && receipt.lane == lane);
        if retained.as_ref().is_some_and(|receipt| {
            receipt.receipt_id == receipt_id && receipt.transferred_bytes != Some(bytes)
        }) {
            return invalid("incoming completion receipt byte count conflicts");
        }
        if !retained
            .as_ref()
            .is_some_and(|receipt| receipt.receipt_id == receipt_id)
        {
            source.total_bytes = source
                .total_bytes
                .checked_add(bytes)
                .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
            if let Some(retained) = retained {
                retained.receipt_id = receipt_id.to_owned();
                retained.transferred_bytes = Some(bytes);
            } else {
                receipts.push(CompletionReceipt {
                    device_id: source_id.to_owned(),
                    lane: lane.to_owned(),
                    receipt_id: receipt_id.to_owned(),
                    transferred_bytes: Some(bytes),
                });
            }
        }
        write_registry(
            &self.root.join("sources.json"),
            &IncomingFile {
                schema: INCOMING_SCHEMA.to_owned(),
                sources: sources.clone(),
                completed_receipts: receipts.clone(),
                pending_completion_deliveries: self.pending_completion_deliveries.clone(),
            },
        )?;
        self.sources = sources;
        self.completed_receipts = receipts;
        Ok(())
    }

    pub(crate) fn has_completed_operation(
        &self,
        source_id: &str,
        receipt_id: &str,
    ) -> Result<bool, PeerSyncError> {
        self.has_completed_operation_for_lane(source_id, CompletionLane::Clone, receipt_id)
    }

    pub(crate) fn has_completed_operation_for_lane(
        &self,
        source_id: &str,
        lane: CompletionLane,
        receipt_id: &str,
    ) -> Result<bool, PeerSyncError> {
        validate_id(source_id)?;
        validate_receipt_id(receipt_id)?;
        let lane = lane.as_str();
        Ok(self.completed_receipts.iter().any(|receipt| {
            receipt.device_id == source_id
                && receipt.lane == lane
                && receipt.receipt_id == receipt_id
        }))
    }

    fn completed_operation_bytes_for_lane(
        &self,
        source_id: &str,
        lane: CompletionLane,
        receipt_id: &str,
    ) -> Result<Option<u64>, PeerSyncError> {
        validate_id(source_id)?;
        validate_receipt_id(receipt_id)?;
        let lane = lane.as_str();
        let Some(receipt) = self.completed_receipts.iter().find(|receipt| {
            receipt.device_id == source_id
                && receipt.lane == lane
                && receipt.receipt_id == receipt_id
        }) else {
            return Ok(None);
        };
        receipt.transferred_bytes.map(Some).ok_or_else(|| {
            PeerSyncError::Validation(
                "incoming completion receipt byte count is missing".to_owned(),
            )
        })
    }

    pub(crate) fn prepare_completion_delivery(
        &mut self,
        delivery: PendingCompletionDelivery,
    ) -> Result<CompletionDeliveryPrepareStatus, PeerSyncError> {
        validate_pending_completion_delivery(&delivery)?;
        if !self
            .sources
            .iter()
            .any(|source| source.device_id == delivery.source_device_id)
        {
            return invalid("registered incoming source is missing");
        }
        if let Some(pending) = self.pending_completion_deliveries.iter().find(|pending| {
            pending.source_device_id == delivery.source_device_id && pending.lane == delivery.lane
        }) {
            return if pending == &delivery {
                Ok(CompletionDeliveryPrepareStatus::Pending)
            } else {
                invalid("incoming source lane has a different pending completion delivery")
            };
        }
        if let Some(receipt) = self.completed_receipts.iter().find(|receipt| {
            receipt.device_id == delivery.source_device_id && receipt.lane == delivery.lane
        }) {
            if receipt.receipt_id == delivery.receipt_id {
                return if receipt.transferred_bytes == Some(delivery.useful_bytes) {
                    Ok(CompletionDeliveryPrepareStatus::AlreadyDurable)
                } else {
                    invalid("incoming completion receipt byte count conflicts")
                };
            }
        }
        let mut pending = self.pending_completion_deliveries.clone();
        pending.push(delivery);
        self.write_state(&self.sources, &self.completed_receipts, &pending)?;
        self.pending_completion_deliveries = pending;
        Ok(CompletionDeliveryPrepareStatus::Pending)
    }

    fn completion_delivery_snapshot(
        &self,
        source_id: &str,
        lane: CompletionLane,
    ) -> Result<Option<IncomingCompletionDeliverySnapshot>, PeerSyncError> {
        validate_id(source_id)?;
        let lane = lane.as_str();
        let Some(delivery) = self
            .pending_completion_deliveries
            .iter()
            .find(|delivery| delivery.source_device_id == source_id && delivery.lane == lane)
        else {
            return Ok(None);
        };
        let source = self
            .sources
            .iter()
            .find(|source| source.device_id == source_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered incoming source is missing".to_owned())
            })?;
        Ok(Some(IncomingCompletionDeliverySnapshot {
            source: source.clone(),
            delivery: delivery.clone(),
        }))
    }

    pub(crate) fn finalize_completion_delivery(
        &mut self,
        delivery: &PendingCompletionDelivery,
        seen_at_ms: u64,
    ) -> Result<CompletionDeliveryFinalizeStatus, PeerSyncError> {
        validate_pending_completion_delivery(delivery)?;
        let pending_index = self
            .pending_completion_deliveries
            .iter()
            .position(|pending| {
                pending.source_device_id == delivery.source_device_id
                    && pending.lane == delivery.lane
            });
        let Some(pending_index) = pending_index else {
            return if self.completed_receipts.iter().any(|receipt| {
                receipt.device_id == delivery.source_device_id
                    && receipt.lane == delivery.lane
                    && receipt.receipt_id == delivery.receipt_id
                    && receipt.transferred_bytes == Some(delivery.useful_bytes)
            }) {
                Ok(CompletionDeliveryFinalizeStatus::AlreadyFinalized)
            } else {
                invalid("pending incoming completion delivery is missing")
            };
        };
        if &self.pending_completion_deliveries[pending_index] != delivery {
            return invalid("pending incoming completion delivery does not match acknowledgement");
        }

        let mut sources = self.sources.clone();
        let mut receipts = self.completed_receipts.clone();
        let mut pending = self.pending_completion_deliveries.clone();
        let source = sources
            .iter_mut()
            .find(|source| source.device_id == delivery.source_device_id)
            .ok_or_else(|| {
                PeerSyncError::Validation("registered incoming source is missing".to_owned())
            })?;
        source.total_bytes = source
            .total_bytes
            .checked_add(delivery.useful_bytes)
            .ok_or_else(|| PeerSyncError::Validation("peer total bytes overflow".to_owned()))?;
        source.last_seen_ms = source.last_seen_ms.max(seen_at_ms);
        if let Some(receipt) = receipts.iter_mut().find(|receipt| {
            receipt.device_id == delivery.source_device_id && receipt.lane == delivery.lane
        }) {
            receipt.receipt_id = delivery.receipt_id.clone();
            receipt.transferred_bytes = Some(delivery.useful_bytes);
        } else {
            receipts.push(CompletionReceipt {
                device_id: delivery.source_device_id.clone(),
                lane: delivery.lane.clone(),
                receipt_id: delivery.receipt_id.clone(),
                transferred_bytes: Some(delivery.useful_bytes),
            });
        }
        pending.remove(pending_index);
        self.write_state(&sources, &receipts, &pending)?;
        self.sources = sources;
        self.completed_receipts = receipts;
        self.pending_completion_deliveries = pending;
        Ok(CompletionDeliveryFinalizeStatus::Finalized)
    }

    pub(crate) fn abandon_completion_delivery(
        &mut self,
        delivery: &PendingCompletionDelivery,
    ) -> Result<bool, PeerSyncError> {
        validate_pending_completion_delivery(delivery)?;
        let Some(pending_index) = self
            .pending_completion_deliveries
            .iter()
            .position(|pending| {
                pending.source_device_id == delivery.source_device_id
                    && pending.lane == delivery.lane
            })
        else {
            return Ok(false);
        };
        if &self.pending_completion_deliveries[pending_index] != delivery {
            return invalid("pending incoming completion delivery does not match abandonment");
        }
        let mut pending = self.pending_completion_deliveries.clone();
        pending.remove(pending_index);
        self.write_state(&self.sources, &self.completed_receipts, &pending)?;
        self.pending_completion_deliveries = pending;
        Ok(true)
    }

    fn completion_is_durable(
        &self,
        delivery: &PendingCompletionDelivery,
    ) -> Result<bool, PeerSyncError> {
        validate_pending_completion_delivery(delivery)?;
        let lane_pending = self.pending_completion_deliveries.iter().any(|pending| {
            pending.source_device_id == delivery.source_device_id && pending.lane == delivery.lane
        });
        Ok(!lane_pending
            && self.completed_receipts.iter().any(|receipt| {
                receipt.device_id == delivery.source_device_id
                    && receipt.lane == delivery.lane
                    && receipt.receipt_id == delivery.receipt_id
                    && receipt.transferred_bytes == Some(delivery.useful_bytes)
            }))
    }

    fn write_state(
        &self,
        sources: &[IncomingSource],
        completed_receipts: &[CompletionReceipt],
        pending_completion_deliveries: &[PendingCompletionDelivery],
    ) -> Result<(), PeerSyncError> {
        write_registry(
            &self.root.join("sources.json"),
            &IncomingFile {
                schema: INCOMING_SCHEMA.to_owned(),
                sources: sources.to_vec(),
                completed_receipts: completed_receipts.to_vec(),
                pending_completion_deliveries: pending_completion_deliveries.to_vec(),
            },
        )
    }

    pub(crate) fn save(&self) -> Result<(), PeerSyncError> {
        self.write_state(
            &self.sources,
            &self.completed_receipts,
            &self.pending_completion_deliveries,
        )
    }
}

pub(crate) fn outgoing_device_summaries(
    app_root: &Path,
) -> Result<Vec<OutgoingDeviceSummary>, PeerSyncError> {
    Ok(OutgoingDeviceRegistry::load(app_root)?
        .devices()
        .iter()
        .map(|device| OutgoingDeviceSummary {
            device_id: device.device_id.clone(),
            name: device.name.clone(),
            permissions: device.permissions.clone(),
            created_at_ms: device.created_at_ms,
            last_seen_ms: device.last_seen_ms,
            total_bytes: device.total_bytes,
        })
        .collect())
}

// Registry mutations must keep their read-modify-write sequence within one
// process-wide directional critical section. The guard is deliberately scoped
// to disk work only, so callers never hold it while touching live host state.
fn with_outgoing_registry<T>(
    app_root: &Path,
    mutate: impl FnOnce(&mut OutgoingDeviceRegistry) -> Result<T, PeerSyncError>,
) -> Result<T, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    let result = mutate(&mut registry)?;
    registry.save()?;
    Ok(result)
}

fn with_incoming_registry<T>(
    app_root: &Path,
    mutate: impl FnOnce(&mut IncomingSourceRegistry) -> Result<T, PeerSyncError>,
) -> Result<T, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    let result = mutate(&mut registry)?;
    registry.save()?;
    Ok(result)
}

pub(crate) fn register_outgoing_claim(
    app_root: &Path,
    device: OutgoingDevice,
) -> Result<(), PeerSyncError> {
    with_outgoing_registry(app_root, |registry| registry.register_claim(device))
}

pub(crate) fn register_incoming_source(
    app_root: &Path,
    source: IncomingSource,
) -> Result<(), PeerSyncError> {
    with_incoming_registry(app_root, |registry| registry.upsert(source))
}

pub(crate) fn outgoing_device_is_registered(
    app_root: &Path,
    device_id: &str,
) -> Result<bool, PeerSyncError> {
    validate_id(device_id)?;
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    Ok(OutgoingDeviceRegistry::load(app_root)?
        .devices()
        .iter()
        .any(|device| device.device_id == device_id))
}

pub(crate) fn incoming_source_summaries(
    app_root: &Path,
) -> Result<Vec<IncomingSourceSummary>, PeerSyncError> {
    Ok(IncomingSourceRegistry::load(app_root)?
        .sources()
        .iter()
        .map(|source| IncomingSourceSummary {
            device_id: source.device_id.clone(),
            name: source.name.clone(),
            permissions: source.permissions.clone(),
            last_seen_ms: source.last_seen_ms,
            total_bytes: source.total_bytes,
        })
        .collect())
}

pub(crate) fn incoming_source_by_id(
    app_root: &Path,
    device_id: &str,
) -> Result<Option<IncomingSource>, PeerSyncError> {
    validate_id(device_id)?;
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    Ok(IncomingSourceRegistry::load(app_root)?
        .sources()
        .iter()
        .find(|source| source.device_id == device_id)
        .cloned())
}

pub(crate) fn revoke_outgoing_device(
    app_root: &Path,
    device_id: &str,
    revoke_live: impl FnOnce(&str),
) -> Result<(), PeerSyncError> {
    validate_id(device_id)?;
    with_outgoing_registry(app_root, |registry| registry.remove(device_id))?;
    revoke_live(device_id);
    Ok(())
}

pub(crate) fn remove_incoming_source(
    app_root: &Path,
    device_id: &str,
) -> Result<(), PeerSyncError> {
    validate_id(device_id)?;
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    #[cfg(any(target_os = "android", test))]
    {
        if super::android_client::registered_clone_source_is_active(app_root, device_id)? {
            return Err(PeerSyncError::Validation(
                "incoming source is used by the active Android clone job".to_owned(),
            ));
        }
    }
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    registry.remove(device_id)?;
    registry.save()
}

pub(crate) fn completion_receipt_id(lane: &str, operation_id: &str, manifest_id: &str) -> String {
    let mut hasher = Sha256::new();
    for value in [lane, operation_id, manifest_id] {
        hasher.update((value.len() as u64).to_le_bytes());
        hasher.update(value.as_bytes());
    }
    hex::encode(hasher.finalize())
}

pub(crate) fn record_outgoing_completed_operation(
    app_root: &Path,
    device_id: &str,
    lane: &str,
    receipt_id: &str,
    bytes: u64,
) -> Result<(), PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.record_completed_operation(device_id, lane, receipt_id, bytes, seen_at_ms)
}

pub(crate) fn issue_outgoing_measured_completion_offer(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    manifest_id: &str,
    transferred_bytes: u64,
    resume_lease_id: Option<&str>,
) -> Result<CompletionLeaseId, PeerSyncError> {
    if lane != CompletionLane::Clone {
        return invalid("logical completion lease must be sealed by its source lane");
    }
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.issue_completion_lease(
        device_id,
        lane,
        manifest_id,
        Some(transferred_bytes),
        resume_lease_id,
    )
}

pub(crate) fn issue_outgoing_unmeasured_completion_offer(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    manifest_id: &str,
    resume_lease_id: Option<&str>,
) -> Result<CompletionLeaseId, PeerSyncError> {
    if lane == CompletionLane::Clone {
        return invalid("clone completion lease requires a source byte count");
    }
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.issue_completion_lease(device_id, lane, manifest_id, None, resume_lease_id)
}

pub(crate) fn outgoing_completion_offer_active(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
) -> Result<bool, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.has_completion_lease(device_id, lane)
}

pub(crate) fn outgoing_completion_lease_matches(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> Result<bool, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.has_exact_completion_lease(
        device_id,
        lane,
        lease_id,
        manifest_id,
    )
}

pub(crate) fn outgoing_completion_lease_ready_bytes(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> Result<Option<u64>, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.completion_lease_ready_bytes(
        device_id,
        lane,
        lease_id,
        manifest_id,
    )
}

pub(crate) fn outgoing_completion_receipt_matches(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> Result<bool, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.has_exact_completion_receipt(
        device_id,
        lane,
        lease_id,
        manifest_id,
        transferred_bytes,
    )
}

pub(crate) fn outgoing_completion_receipt_bytes(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    lease_id: &str,
    manifest_id: &str,
) -> Result<Option<u64>, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.completion_receipt_bytes(
        device_id,
        lane,
        lease_id,
        manifest_id,
    )
}

pub(crate) fn outgoing_bidirectional_completion_lease_allows_remote_apply(
    app_root: &Path,
    device_id: &str,
    lease_id: &str,
    manifest_id: &str,
) -> Result<bool, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    OutgoingDeviceRegistry::load(app_root)?.completion_lease_allows_remote_apply(
        device_id,
        lease_id,
        manifest_id,
    )
}

pub(crate) fn seal_outgoing_completion_lease(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    operation_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> Result<CompletionSealStatus, PeerSyncError> {
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.seal_completion_lease(
        device_id,
        lane,
        operation_id,
        manifest_id,
        transferred_bytes,
    )
}

pub(crate) fn accept_outgoing_completion_offer(
    app_root: &Path,
    device_id: &str,
    lane: CompletionLane,
    operation_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> Result<CompletionAcceptance, PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.accept_completion_offer(
        device_id,
        lane,
        operation_id,
        manifest_id,
        transferred_bytes,
        seen_at_ms,
    )
}

pub(crate) fn record_outgoing_seen(app_root: &Path, device_id: &str) -> Result<(), PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    let _guard = outgoing_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("outgoing peer registry lock failed".to_owned()))?;
    let mut registry = OutgoingDeviceRegistry::load(app_root)?;
    registry.record_seen(device_id, seen_at_ms)
}

pub(crate) fn record_incoming_completed_operation(
    app_root: &Path,
    source_id: &str,
    bytes: u64,
) -> Result<(), PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    with_incoming_registry(app_root, |registry| {
        if !registry
            .sources()
            .iter()
            .any(|source| source.device_id == source_id)
        {
            return Ok(());
        }
        registry.record_completed_operation(source_id, bytes, seen_at_ms)
    })
}

pub(crate) fn record_incoming_completed_operation_best_effort(
    app_root: &Path,
    source_id: &str,
    bytes: u64,
) {
    if let Err(error) = record_incoming_completed_operation(app_root, source_id, bytes) {
        crate::nlog!("warn", "peer sync completion accounting failed: {error}");
    }
}

pub(crate) fn record_incoming_completed_operation_once(
    app_root: &Path,
    source_id: &str,
    receipt_id: &str,
    bytes: u64,
) -> Result<(), PeerSyncError> {
    record_incoming_completed_operation_once_for_lane(
        app_root,
        source_id,
        CompletionLane::Clone,
        receipt_id,
        bytes,
    )
}

pub(crate) fn record_incoming_completed_operation_once_for_lane(
    app_root: &Path,
    source_id: &str,
    lane: CompletionLane,
    receipt_id: &str,
    bytes: u64,
) -> Result<(), PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    registry
        .record_completed_operation_once_for_lane(source_id, lane, receipt_id, bytes, seen_at_ms)
}

pub(crate) fn incoming_completed_operation_recorded(
    app_root: &Path,
    source_id: &str,
    receipt_id: &str,
) -> Result<bool, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    IncomingSourceRegistry::load(app_root)?.has_completed_operation(source_id, receipt_id)
}

pub(crate) fn incoming_completed_operation_recorded_for_lane(
    app_root: &Path,
    source_id: &str,
    lane: CompletionLane,
    receipt_id: &str,
) -> Result<bool, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    IncomingSourceRegistry::load(app_root)?
        .has_completed_operation_for_lane(source_id, lane, receipt_id)
}

pub(crate) fn incoming_completed_operation_bytes_for_lane(
    app_root: &Path,
    source_id: &str,
    lane: CompletionLane,
    receipt_id: &str,
) -> Result<Option<u64>, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    IncomingSourceRegistry::load(app_root)?
        .completed_operation_bytes_for_lane(source_id, lane, receipt_id)
}

pub(crate) fn prepare_incoming_completion_delivery(
    app_root: &Path,
    delivery: PendingCompletionDelivery,
) -> Result<CompletionDeliveryPrepareStatus, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    registry.prepare_completion_delivery(delivery)
}

// The returned value owns both the native-only credential and the exact
// delivery record. The registry lock is released before the caller performs
// any HTTP request.
pub(crate) fn snapshot_incoming_completion_delivery(
    app_root: &Path,
    source_id: &str,
    lane: CompletionLane,
) -> Result<Option<IncomingCompletionDeliverySnapshot>, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    IncomingSourceRegistry::load(app_root)?.completion_delivery_snapshot(source_id, lane)
}

pub(crate) fn finalize_incoming_completion_delivery(
    app_root: &Path,
    delivery: &PendingCompletionDelivery,
) -> Result<CompletionDeliveryFinalizeStatus, PeerSyncError> {
    let seen_at_ms = completion_timestamp_ms()?;
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    registry.finalize_completion_delivery(delivery, seen_at_ms)
}

pub(crate) fn abandon_incoming_completion_delivery(
    app_root: &Path,
    delivery: &PendingCompletionDelivery,
) -> Result<bool, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    let mut registry = IncomingSourceRegistry::load(app_root)?;
    registry.abandon_completion_delivery(delivery)
}

pub(crate) fn incoming_completion_is_durable(
    app_root: &Path,
    delivery: &PendingCompletionDelivery,
) -> Result<bool, PeerSyncError> {
    let _guard = incoming_registry_lock()
        .lock()
        .map_err(|_| PeerSyncError::Storage("incoming peer registry lock failed".to_owned()))?;
    IncomingSourceRegistry::load(app_root)?.completion_is_durable(delivery)
}

pub(crate) fn load_or_create_device_id(app_root: &Path) -> Result<String, PeerSyncError> {
    let root = ensure_peer_root(app_root)?;
    let current = root.join("device-id");
    if path_exists(&current)? {
        return read_device_id(&current);
    }
    let legacy = app_root.join("peer-delta/source-device-id");
    let has_legacy = path_exists(&legacy)?;
    let device_id = if has_legacy {
        read_device_id(&legacy)?
    } else {
        uuid::Uuid::new_v4().to_string()
    };
    write_owner_only(&current, device_id.as_bytes())?;
    if has_legacy {
        fs::remove_file(legacy)?;
    }
    Ok(device_id)
}

pub(crate) fn platform_device_name() -> &'static str {
    match std::env::consts::OS {
        "android" => "Android",
        "windows" => "Windows",
        "macos" => "macOS",
        "linux" => "Linux",
        _ => "Desktop",
    }
}

fn peer_root(app_root: &Path) -> PathBuf {
    app_root.join("peer-sync")
}

fn ensure_peer_root(app_root: &Path) -> Result<PathBuf, PeerSyncError> {
    let root = peer_root(app_root);
    match fs::symlink_metadata(&root) {
        Ok(metadata) => ensure_ordinary_directory(&metadata, "peer sync directory")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;

                fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(&root)?;
            }
            #[cfg(not(unix))]
            fs::create_dir_all(&root)?;
            let metadata = fs::symlink_metadata(&root)?;
            ensure_ordinary_directory(&metadata, "peer sync directory")?;
        }
        Err(error) => return Err(error.into()),
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    }
    Ok(root)
}

fn ensure_ordinary_directory(metadata: &fs::Metadata, label: &str) -> Result<(), PeerSyncError> {
    if !metadata.is_dir() || is_link_like(metadata) {
        return invalid(&format!("{label} must be an ordinary directory"));
    }
    Ok(())
}

fn path_exists(path: &Path) -> Result<bool, PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn read_device_id(path: &Path) -> Result<String, PeerSyncError> {
    let bytes = read_regular_bounded_file(path, MAX_DEVICE_ID_BYTES, "peer device id")?;
    let value = std::str::from_utf8(&bytes)
        .map_err(|_| PeerSyncError::Validation("invalid peer device id".to_owned()))?
        .trim_end_matches(['\r', '\n'])
        .to_owned();
    validate_id(&value)?;
    Ok(value)
}

fn read_registry(path: &Path) -> Result<Option<Vec<u8>>, PeerSyncError> {
    match fs::symlink_metadata(path) {
        Ok(_) => read_regular_bounded_file(path, MAX_REGISTRY_BYTES, "peer registry").map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn read_regular_bounded_file(
    path: &Path,
    maximum: usize,
    label: &str,
) -> Result<Vec<u8>, PeerSyncError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || is_link_like(&metadata) {
        return invalid(&format!("{label} must be a regular file"));
    }
    if metadata.len() > maximum as u64 {
        return invalid(&format!("{label} exceeds its size limit"));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)?
        .take(maximum as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > maximum {
        return invalid(&format!("{label} exceeds its size limit"));
    }
    Ok(bytes)
}

fn parse_registry<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, PeerSyncError> {
    serde_json::from_slice(bytes)
        .map_err(|_| PeerSyncError::Validation("malformed peer registry".to_owned()))
}
fn invalid<T>(message: &str) -> Result<T, PeerSyncError> {
    Err(PeerSyncError::Validation(message.to_owned()))
}

fn validate_id(value: &str) -> Result<(), PeerSyncError> {
    if uuid::Uuid::parse_str(value)
        .map(|parsed| parsed.to_string() == value)
        .unwrap_or(false)
    {
        Ok(())
    } else {
        invalid("invalid peer device id")
    }
}
fn validate_name(value: &str) -> Result<(), PeerSyncError> {
    if value.is_empty() || value.len() > 256 {
        invalid("invalid peer device name")
    } else {
        Ok(())
    }
}
fn validate_outgoing(devices: &[OutgoingDevice]) -> Result<(), PeerSyncError> {
    for item in devices {
        validate_id(&item.device_id)?;
        validate_name(&item.name)?;
        if !is_lower_hex_256(&item.bearer_digest) {
            return invalid("invalid peer bearer digest");
        }
        item.permissions.validate()?;
    }
    Ok(())
}
fn validate_incoming(sources: &[IncomingSource]) -> Result<(), PeerSyncError> {
    for item in sources {
        validate_id(&item.device_id)?;
        validate_name(&item.name)?;
        if item.endpoint.len() > 2048
            || super::lan::validate_lan_endpoint(&item.endpoint).is_err()
            || !is_lower_hex_256(&item.bearer)
        {
            return invalid("invalid registered peer source");
        }
        item.permissions.validate()?;
    }
    Ok(())
}

fn validate_receipt_id(value: &str) -> Result<(), PeerSyncError> {
    if is_lower_hex_256(value) {
        Ok(())
    } else {
        invalid("invalid peer completion receipt")
    }
}

fn validate_receipt_lane(value: &str) -> Result<(), PeerSyncError> {
    CompletionLane::parse(value).map(|_| ())
}

fn validate_completion_tuple(
    device_id: &str,
    lane: CompletionLane,
    operation_id: &str,
    manifest_id: &str,
) -> Result<(), PeerSyncError> {
    validate_id(device_id)?;
    validate_receipt_lane(lane.as_str())?;
    let parsed_operation_id = uuid::Uuid::parse_str(operation_id)
        .map_err(|_| PeerSyncError::Validation("invalid completion operation ID".to_owned()))?;
    if parsed_operation_id.get_version_num() != 4
        || parsed_operation_id.get_variant() != uuid::Variant::RFC4122
        || parsed_operation_id.to_string() != operation_id
        || !is_lower_hex_256(manifest_id)
    {
        return invalid("invalid completion offer");
    }
    Ok(())
}

fn completion_offer_matches(
    offer: &CompletionOffer,
    device_id: &str,
    lane: &str,
    operation_id: &str,
    manifest_id: &str,
    transferred_bytes: u64,
) -> bool {
    offer.device_id == device_id
        && offer.lane == lane
        && offer.lease_id == operation_id
        && offer.manifest_id == manifest_id
        && offer.transferred_bytes == Some(transferred_bytes)
}

fn completion_timestamp_ms() -> Result<u64, PeerSyncError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| PeerSyncError::Storage(error.to_string()))?
        .as_millis()
        .try_into()
        .map_err(|_| PeerSyncError::Storage("peer completion timestamp overflow".to_owned()))
}

fn validate_receipts<'a>(
    receipts: &[CompletionReceipt],
    registered_ids: impl Iterator<Item = &'a str>,
) -> Result<(), PeerSyncError> {
    let registered_ids = registered_ids.collect::<std::collections::BTreeSet<_>>();
    if receipts.len() > registered_ids.len().saturating_mul(3) {
        return invalid("too many peer completion receipts");
    }
    let mut unique = std::collections::BTreeSet::new();
    for receipt in receipts {
        validate_id(&receipt.device_id)?;
        validate_receipt_lane(&receipt.lane)?;
        validate_receipt_id(&receipt.receipt_id)?;
        if !registered_ids.contains(receipt.device_id.as_str())
            || !unique.insert((&receipt.device_id, &receipt.lane))
        {
            return invalid("invalid peer completion receipt");
        }
    }
    Ok(())
}

fn validate_incoming_receipts<'a>(
    receipts: &[CompletionReceipt],
    registered_ids: impl Iterator<Item = &'a str>,
) -> Result<(), PeerSyncError> {
    validate_receipts(receipts, registered_ids)
}

fn validate_pending_completion_delivery(
    delivery: &PendingCompletionDelivery,
) -> Result<(), PeerSyncError> {
    let lane = CompletionLane::parse(&delivery.lane)?;
    validate_completion_tuple(
        &delivery.source_device_id,
        lane,
        &delivery.completion_lease_id,
        &delivery.manifest_id,
    )?;
    validate_receipt_id(&delivery.receipt_id)?;
    if completion_receipt_id(
        &delivery.lane,
        &delivery.completion_lease_id,
        &delivery.manifest_id,
    ) != delivery.receipt_id
    {
        return invalid("incoming completion delivery receipt does not match its evidence");
    }
    Ok(())
}

fn validate_pending_completion_deliveries<'a>(
    deliveries: &[PendingCompletionDelivery],
    completed_receipts: &[CompletionReceipt],
    registered_ids: impl Iterator<Item = &'a str>,
) -> Result<(), PeerSyncError> {
    let registered_ids = registered_ids.collect::<std::collections::BTreeSet<_>>();
    if deliveries.len() > registered_ids.len().saturating_mul(3) {
        return invalid("too many pending incoming completion deliveries");
    }
    let mut unique = std::collections::BTreeSet::new();
    for delivery in deliveries {
        validate_pending_completion_delivery(delivery)?;
        if !registered_ids.contains(delivery.source_device_id.as_str())
            || !unique.insert((&delivery.source_device_id, &delivery.lane))
            || completed_receipts.iter().any(|receipt| {
                receipt.device_id == delivery.source_device_id
                    && receipt.lane == delivery.lane
                    && receipt.receipt_id == delivery.receipt_id
            })
        {
            return invalid("invalid pending incoming completion delivery");
        }
    }
    Ok(())
}

fn validate_completion_offers<'a>(
    offers: &[CompletionOffer],
    registered_devices: impl Iterator<Item = (&'a str, &'a str)>,
) -> Result<(), PeerSyncError> {
    let registered_devices = registered_devices.collect::<std::collections::BTreeMap<_, _>>();
    if offers.len() > registered_devices.len().saturating_mul(3) {
        return invalid("too many peer completion offers");
    }
    let mut unique = std::collections::BTreeSet::new();
    for offer in offers {
        let lane = CompletionLane::parse(&offer.lane)?;
        validate_completion_tuple(&offer.device_id, lane, &offer.lease_id, &offer.manifest_id)?;
        let valid_state = match (lane, offer.ready, offer.transferred_bytes) {
            (CompletionLane::Clone, false, Some(_)) => true,
            (CompletionLane::Delta | CompletionLane::Bidirectional, false, None) => true,
            (_, true, Some(_)) => true,
            _ => false,
        };
        if !valid_state
            || registered_devices.get(offer.device_id.as_str())
                != Some(&offer.bearer_digest.as_str())
            || !unique.insert((&offer.device_id, &offer.lane))
        {
            return invalid("invalid peer completion offer");
        }
    }
    Ok(())
}

fn write_registry<T: Serialize>(path: &Path, value: &T) -> Result<(), PeerSyncError> {
    #[cfg(test)]
    {
        let failure_marker = path.with_extension("fail-next-write");
        if fs::remove_file(failure_marker).is_ok() {
            return Err(PeerSyncError::Storage(
                "injected outgoing registry write failure".to_owned(),
            ));
        }
    }
    let bytes =
        serde_json::to_vec(value).map_err(|error| PeerSyncError::Storage(error.to_string()))?;
    if bytes.len() > MAX_REGISTRY_BYTES {
        return invalid("peer registry exceeds 1 MiB");
    }
    let parent = path
        .parent()
        .ok_or_else(|| PeerSyncError::Storage("peer registry has no parent".to_owned()))?;
    let parent_metadata = fs::symlink_metadata(parent)?;
    ensure_ordinary_directory(&parent_metadata, "peer registry parent")?;
    if let Ok(metadata) = fs::symlink_metadata(path) {
        if !metadata.is_file() || is_link_like(&metadata) {
            return invalid("peer registry must be a regular file");
        }
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| PeerSyncError::Storage("peer registry has no UTF-8 name".to_owned()))?;
    let temporary = parent.join(format!(".{name}-{}.tmp", uuid::Uuid::new_v4()));
    let result = write_owner_only(&temporary, &bytes).and_then(|_| {
        fs::rename(&temporary, path)?;
        let _ = sync_directory(parent)?;
        Ok(())
    });
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
pub(crate) fn fail_next_outgoing_registry_write_for_test(
    app_root: &Path,
) -> Result<(), PeerSyncError> {
    let root = ensure_peer_root(app_root)?;
    let marker = root.join("devices.fail-next-write");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    options.open(marker)?.sync_all()?;
    Ok(())
}
fn write_owner_only(path: &Path, bytes: &[u8]) -> Result<(), PeerSyncError> {
    let mut file = create_owner_only_file(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        file.set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()?;
    Ok(())
}

fn create_owner_only_file(path: &Path) -> Result<File, PeerSyncError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    Ok(options.open(path)?)
}

#[cfg(all(test, unix))]
mod unix_permission_tests {
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn owner_only_file_is_restricted_before_post_open_hardening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("registry.tmp");

        let file = super::create_owner_only_file(&path).unwrap();

        assert_eq!(file.metadata().unwrap().permissions().mode() & 0o777, 0o600);
    }
}

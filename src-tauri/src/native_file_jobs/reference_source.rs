use super::{NativeFileJobState, NativeJobError};
use crate::persistent_store::external_conflicts::{self, ConflictSide, ConflictSourceDescriptor};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tauri::{AppHandle, Manager, State};
use uuid::Uuid;

struct ExternalReferenceEntry {
    conflict_id: String,
    descriptor: ConflictSourceDescriptor,
    renderer_active: bool,
    workers: usize,
}

#[derive(Default)]
pub(super) struct ExternalReferenceSources {
    entries: Mutex<BTreeMap<String, ExternalReferenceEntry>>,
}

pub(crate) struct ExternalReferenceSourceGuard {
    sources: Arc<ExternalReferenceSources>,
    token: String,
    descriptor: ConflictSourceDescriptor,
}

impl ExternalReferenceSourceGuard {
    pub(crate) fn descriptor(&self) -> &ConflictSourceDescriptor {
        &self.descriptor
    }
}

impl Drop for ExternalReferenceSourceGuard {
    fn drop(&mut self) {
        let Ok(mut entries) = self.sources.entries.lock() else {
            return;
        };
        let remove = if let Some(entry) = entries.get_mut(&self.token) {
            entry.workers = entry.workers.saturating_sub(1);
            !entry.renderer_active && entry.workers == 0
        } else {
            false
        };
        if remove {
            entries.remove(&self.token);
        }
    }
}

fn conflict_id(descriptor: &ConflictSourceDescriptor) -> &str {
    match descriptor {
        ConflictSourceDescriptor::Local { conflict_id, .. }
        | ConflictSourceDescriptor::Remote { conflict_id, .. } => conflict_id,
    }
}

fn source_unavailable() -> NativeJobError {
    NativeJobError::new(
        "source-unavailable",
        "Conflict source is no longer available",
    )
}

fn validate_external_token(token: &str) -> Result<(), NativeJobError> {
    let id = token
        .strip_prefix("external:")
        .ok_or_else(|| NativeJobError::new("invalid-source", "Invalid conflict source token"))?;
    let uuid = Uuid::parse_str(id).map_err(|_| source_unavailable())?;
    if uuid.to_string() != id || uuid.get_version() != Some(uuid::Version::Random) {
        return Err(source_unavailable());
    }
    Ok(())
}

impl ExternalReferenceSources {
    fn register(
        self: &Arc<Self>,
        descriptor: ConflictSourceDescriptor,
    ) -> Result<String, NativeJobError> {
        let mut entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        loop {
            let token = format!("external:{}", Uuid::new_v4());
            if entries.contains_key(&token) {
                continue;
            }
            entries.insert(
                token.clone(),
                ExternalReferenceEntry {
                    conflict_id: conflict_id(&descriptor).to_owned(),
                    descriptor,
                    renderer_active: true,
                    workers: 0,
                },
            );
            return Ok(token);
        }
    }

    fn claim(
        self: &Arc<Self>,
        token: &str,
    ) -> Result<ExternalReferenceSourceGuard, NativeJobError> {
        validate_external_token(token)?;
        let descriptor = {
            let mut entries = self.entries.lock().map_err(|_| {
                NativeJobError::new("store-error", "Conflict source state is unavailable")
            })?;
            let entry = entries.get_mut(token).ok_or_else(source_unavailable)?;
            if !entry.renderer_active {
                return Err(source_unavailable());
            }
            entry.workers = entry.workers.checked_add(1).ok_or_else(|| {
                NativeJobError::new("store-error", "Conflict source claims overflowed")
            })?;
            entry.descriptor.clone()
        };
        Ok(ExternalReferenceSourceGuard {
            sources: Arc::clone(self),
            token: token.to_owned(),
            descriptor,
        })
    }

    fn release(&self, token: &str) -> Result<(), NativeJobError> {
        validate_external_token(token)?;
        let mut entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        let remove = if let Some(entry) = entries.get_mut(token) {
            entry.renderer_active = false;
            entry.workers == 0
        } else {
            false
        };
        if remove {
            entries.remove(token);
        }
        Ok(())
    }

    fn conflict_in_use(&self, id: &str) -> Result<bool, NativeJobError> {
        if id.is_empty() || id.len() > 1024 || id.contains('\0') {
            return Err(NativeJobError::new(
                "invalid-input",
                "Invalid external conflict ID",
            ));
        }
        let entries = self.entries.lock().map_err(|_| {
            NativeJobError::new("store-error", "Conflict source state is unavailable")
        })?;
        Ok(entries
            .values()
            .any(|entry| entry.conflict_id == id && (entry.renderer_active || entry.workers != 0)))
    }
}

impl NativeFileJobState {
    fn external_conflict_source_open_admission(
        &self,
    ) -> Result<super::admission::Permit, NativeJobError> {
        self.admission
            .file(false)
            .map_err(|code| NativeJobError::new(code, "Another library operation is running"))
    }

    pub(crate) fn external_conflict_mutation_admission(
        &self,
    ) -> Result<super::admission::Permit, NativeJobError> {
        self.admission
            .file(true)
            .map_err(|code| NativeJobError::new(code, "Another library operation is running"))
    }

    fn register_external_reference_source(
        &self,
        descriptor: ConflictSourceDescriptor,
    ) -> Result<String, NativeJobError> {
        self.external_reference_sources.register(descriptor)
    }

    pub(crate) fn claim_external_reference_source(
        &self,
        token: &str,
    ) -> Result<ExternalReferenceSourceGuard, NativeJobError> {
        self.external_reference_sources.claim(token)
    }

    pub(crate) fn external_conflict_in_use(&self, id: &str) -> Result<bool, NativeJobError> {
        self.external_reference_sources.conflict_in_use(id)
    }

    fn release_external_reference_source(&self, token: &str) -> Result<(), NativeJobError> {
        self.external_reference_sources.release(token)
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ConflictReferenceSource {
    ConflictReference { token: String },
}

#[derive(Serialize)]
pub(crate) struct OpenConflictSourceResponse {
    source: ConflictReferenceSource,
}

#[tauri::command]
pub(crate) fn external_storage_open_conflict_source(
    app: AppHandle,
    state: State<'_, NativeFileJobState>,
    id: String,
    side: ConflictSide,
) -> Result<OpenConflictSourceResponse, NativeJobError> {
    let _admission = state.external_conflict_source_open_admission()?;
    crate::persistent_store::commands::with_store_mut(app.state(), |store| {
        let root = store.repository_root().to_path_buf();
        let descriptor = external_conflicts::conflict_source_descriptor(
            store.device_store()?.connection(),
            &root,
            &id,
            side,
        )?;
        state
            .register_external_reference_source(descriptor)
            .map(|token| OpenConflictSourceResponse {
                source: ConflictReferenceSource::ConflictReference { token },
            })
            .map_err(|error| crate::persistent_store::StoreError::Store {
                message: error.to_string(),
            })
    })
    .map_err(super::native_store_error)
}

#[tauri::command]
pub(crate) fn external_storage_release_conflict_source(
    state: State<'_, NativeFileJobState>,
    token: String,
) -> Result<(), NativeJobError> {
    state.release_external_reference_source(&token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        external_storage::capture::DurableCaptureReference,
        persistent_store::sync_selection::CaptureIdentity,
    };

    fn descriptor(id: &str) -> ConflictSourceDescriptor {
        ConflictSourceDescriptor::Local {
            conflict_id: id.into(),
            repository_id: "repository".into(),
            capture: DurableCaptureReference {
                capture_id: "capture".into(),
                identity: CaptureIdentity {
                    store_id: "store".into(),
                    library_epoch: "library".into(),
                    generation: "generation".into(),
                    selection_epoch: "selection".into(),
                    revision: 1,
                },
                catalog_path: "captures/catalog.sqlite".into(),
                catalog_hash: "00".repeat(32),
            },
        }
    }

    #[test]
    fn released_renderer_lease_keeps_worker_claim_pinned_until_drop() {
        let sources = Arc::new(ExternalReferenceSources::default());
        let token = sources.register(descriptor("conflict")).unwrap();
        let guard = sources.claim(&token).unwrap();
        sources.release(&token).unwrap();
        assert!(sources.conflict_in_use("conflict").unwrap());
        let error = match sources.claim(&token) {
            Ok(_) => panic!("released source was claimed"),
            Err(error) => error,
        };
        assert_eq!(error.code, "source-unavailable");
        drop(guard);
        assert!(!sources.conflict_in_use("conflict").unwrap());
        sources.release(&token).unwrap();
    }

    #[test]
    fn conflict_delete_cannot_cross_the_descriptor_read_and_registration_boundary() {
        let state = NativeFileJobState::initialize_with_max_workers(
            tempfile::tempdir().unwrap().path().to_path_buf(),
            1,
        );
        let open = state.external_conflict_source_open_admission().unwrap();
        assert_eq!(
            state
                .external_conflict_mutation_admission()
                .unwrap_err()
                .code,
            "library-operation-busy"
        );
        let token = state
            .register_external_reference_source(descriptor("conflict"))
            .unwrap();
        drop(open);

        let _delete = state.external_conflict_mutation_admission().unwrap();
        assert!(state.external_conflict_in_use("conflict").unwrap());
        state.release_external_reference_source(&token).unwrap();
        assert!(!state.external_conflict_in_use("conflict").unwrap());
    }
}

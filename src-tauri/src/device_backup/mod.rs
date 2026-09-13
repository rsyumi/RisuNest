//! Durable, job-owned maintenance journal for WebView device data.
//!
//! Admission and the native PDS fence belong to the file job. Its owner creates
//! a session only after acquiring both, and keeps them until `is_blocking` is
//! false. IPC can operate on an existing session, never create a job or choose
//! a filesystem path. The maintenance entry must run before normal app imports.

mod archive;
mod commands;
mod spool;
pub(crate) use archive::validate_archive_catalog;
pub(crate) use commands::*;
pub(crate) use spool::{BlobManifest, RowPage, SectionManifest};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

pub(crate) const MAX_CHUNK_BYTES: usize = 256 * 1024;
pub(crate) const MAX_GRAPH_BYTES: usize = 64 * 1024 * 1024;
pub(crate) type Result<T> = std::result::Result<T, DeviceBackupError>;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeviceBackupError {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl std::fmt::Display for DeviceBackupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for DeviceBackupError {}

impl From<rusqlite::Error> for DeviceBackupError {
    fn from(_: rusqlite::Error) -> Self {
        error(
            "device-storage-failed",
            "Device maintenance SQLite operation failed",
        )
    }
}

impl From<std::io::Error> for DeviceBackupError {
    fn from(_: std::io::Error) -> Self {
        error(
            "device-storage-failed",
            "Device maintenance filesystem operation failed",
        )
    }
}

fn error(code: &str, message: &str) -> DeviceBackupError {
    DeviceBackupError {
        code: code.into(),
        message: message.into(),
    }
}

fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(error("device-invalid-state", message))
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Operation {
    Capture,
    Restore,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Spool {
    Source,
    Rollback,
}

impl Spool {
    fn key(self) -> &'static str {
        match self {
            Self::Source => "source",
            Self::Rollback => "rollback",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommitMarker {
    pub(crate) job_id: String,
    pub(crate) session_id: String,
    pub(crate) new_generation: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Session {
    pub(crate) session_id: String,
    pub(crate) job_id: String,
    pub(crate) operation: Operation,
    pub(crate) phase: String,
    pub(crate) includes_library: bool,
    pub(crate) selected_sections: Vec<String>,
    pub(crate) old_generation: Option<String>,
    pub(crate) new_generation: Option<String>,
    pub(crate) action: String,
    pub(crate) failure_code: Option<String>,
    pub(crate) failure_detail: Option<FailureDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FailureDetail {
    pub(crate) section_id: String,
    pub(crate) value_type: String,
    pub(crate) location: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BootstrapDecision {
    pub(crate) mode: &'static str,
    pub(crate) session: Option<Session>,
}

pub(crate) struct DeviceBackupState {
    repository_root: PathBuf,
    inner: Mutex<Inner>,
    maintenance_guard: Mutex<Option<crate::persistent_store::commands::DeviceMaintenanceGuard>>,
    startup_admission: Mutex<Option<crate::native_file_jobs::admission::Permit>>,
    completed_documents: AtomicU64,
    started_documents: AtomicU64,
}

struct Inner {
    connection: Option<Connection>,
    initialization_error: Option<DeviceBackupError>,
    reconciled: bool,
    maintenance_entry: Option<String>,
    entry_requested: bool,
    entry_document: u64,
    required_document: u64,
    // Only an active session loaded during process startup has no native worker.
    cold_session: Option<String>,
}

impl DeviceBackupState {
    /// `root` is the dedicated app-data `device-backup` directory.
    pub(crate) fn initialize(root: PathBuf) -> Self {
        let repository_root = root.parent().unwrap_or(&root).to_path_buf();
        let result = open(&root).and_then(|connection| {
            // initialize is called once before native workers start. An inactive
            // session from the previous process cannot have a live archive reader.
            connection.execute("DELETE FROM sessions WHERE active=0", [])?;
            reclaim_free_pages(&connection)?;
            let cold_session = active_session(&connection)?.map(|session| session.session_id);
            Ok((connection, cold_session))
        });
        let (connection, cold_session, initialization_error) = match result {
            Ok((connection, cold_session)) => (Some(connection), cold_session, None),
            Err(error) => (None, None, Some(error)),
        };
        Self {
            repository_root,
            inner: Mutex::new(Inner {
                connection,
                initialization_error,
                reconciled: false,
                maintenance_entry: None,
                entry_requested: false,
                entry_document: 0,
                required_document: 0,
                cold_session,
            }),
            maintenance_guard: Mutex::new(None),
            startup_admission: Mutex::new(None),
            completed_documents: AtomicU64::new(0),
            started_documents: AtomicU64::new(0),
        }
    }

    pub(crate) fn attach_maintenance_guard(
        &self,
        guard: crate::persistent_store::commands::DeviceMaintenanceGuard,
    ) -> Result<()> {
        let mut slot = self.maintenance_guard.lock().map_err(|_| {
            error(
                "device-storage-failed",
                "Device maintenance guard lock failed",
            )
        })?;
        require(
            slot.is_none(),
            "Device maintenance guard is already attached",
        )?;
        *slot = Some(guard);
        Ok(())
    }

    pub(crate) fn attach_startup_admission(
        &self,
        permit: crate::native_file_jobs::admission::Permit,
    ) -> Result<()> {
        let mut slot = self
            .startup_admission
            .lock()
            .map_err(|_| error("device-storage-failed", "Device admission lock failed"))?;
        require(slot.is_none(), "Device startup admission is already held")?;
        *slot = Some(permit);
        Ok(())
    }

    /// Invoked by Tauri's main-document completion callback, not renderer IPC.
    pub(crate) fn main_document_finished(&self) {
        self.completed_documents.store(
            self.started_documents.load(Ordering::SeqCst),
            Ordering::SeqCst,
        );
    }

    pub(crate) fn main_document_started(&self) {
        self.started_documents.fetch_add(1, Ordering::SeqCst);
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| error("device-storage-failed", "Device maintenance lock failed"))?;
        if let Some(error) = &inner.initialization_error {
            return Err(error.clone());
        }
        Ok(inner)
    }

    fn require_maintenance_guard(&self) -> Result<()> {
        let slot = self.maintenance_guard.lock().map_err(|_| {
            error(
                "device-storage-failed",
                "Device maintenance guard lock failed",
            )
        })?;
        require(
            slot.is_some(),
            "Device maintenance writer fence is not held",
        )
    }

    /// Native-only. The caller owns admission and every writer fence first.
    pub(crate) fn create_session(
        &self,
        job_id: &str,
        operation: Operation,
        includes_library: bool,
        selected_sections: &[String],
        old_generation: Option<String>,
        new_generation: Option<String>,
    ) -> Result<String> {
        self.require_maintenance_guard()?;
        validate_id(job_id)?;
        require(
            !selected_sections.is_empty() && selected_sections.len() <= 1024,
            "Invalid selected section count",
        )?;
        require(
            selected_sections.iter().map(|s| s.len()).sum::<usize>() <= 128 * 1024,
            "Selected section identifiers exceed bootstrap bound",
        )?;
        let mut unique = std::collections::HashSet::new();
        for section in selected_sections {
            validate_section(section)?;
            require(unique.insert(section), "Duplicate selected section")?;
        }
        for generation in [&old_generation, &new_generation].into_iter().flatten() {
            require(
                generation.len() <= 256,
                "Generation identifier is too large",
            )?;
        }
        let mut inner = self.lock()?;
        self.require_maintenance_guard()?;
        let connection = inner.connection.as_mut().unwrap();
        require(
            active_session(connection)?.is_none(),
            "Another device maintenance session is pending",
        )?;
        let id = Uuid::new_v4().to_string();
        let transaction = connection.transaction()?;
        transaction.execute("INSERT INTO sessions(id,job_id,operation,phase,includes_library,old_generation,new_generation) VALUES(?1,?2,?3,?4,?5,?6,?7)", params![id,job_id,if operation == Operation::Capture {"capture"} else {"restore"},if operation == Operation::Capture {"capturing"} else {"loading-source"},includes_library,old_generation,new_generation])?;
        for (position, section) in selected_sections.iter().enumerate() {
            transaction.execute(
                "INSERT INTO selection(session,section,position) VALUES(?1,?2,?3)",
                params![id, section, position as i64],
            )?;
        }
        transaction.commit()?;
        // This process created the request, so first bootstrap continues it.
        inner.reconciled = true;
        inner.maintenance_entry = None;
        inner.entry_requested = false;
        inner.cold_session = None;
        inner.required_document = self
            .started_documents
            .load(Ordering::SeqCst)
            .saturating_add(1);
        Ok(id)
    }

    /// Native-only cleanup of failed admission/preparation. An existing or
    /// unreadable journal keeps every writer fence, even when creation failed.
    pub(crate) fn release_unused_maintenance(&self) -> Result<()> {
        let inner = self.lock()?;
        require(
            active_session(inner.connection.as_ref().unwrap())?.is_none(),
            "A durable device session still requires its fence",
        )?;
        self.maintenance_guard
            .lock()
            .map_err(|_| {
                error(
                    "device-storage-failed",
                    "Device maintenance guard lock failed",
                )
            })?
            .take();
        self.startup_admission
            .lock()
            .map_err(|_| error("device-storage-failed", "Device admission lock failed"))?
            .take();
        Ok(())
    }

    pub(crate) fn is_blocking(&self) -> Result<bool> {
        Ok(active_session(self.lock()?.connection.as_ref().unwrap())?.is_some())
    }

    pub(crate) fn bootstrap(&self) -> Result<BootstrapDecision> {
        self.bootstrap_for_entry(false)
    }

    pub(crate) fn bootstrap_for_entry(&self, fresh_bootstrap: bool) -> Result<BootstrapDecision> {
        let mut inner = self.lock()?;
        let restart = !inner.reconciled;
        let completed = self.completed_documents.load(Ordering::SeqCst);
        if fresh_bootstrap {
            if inner.maintenance_entry.is_some() {
                inner.required_document = inner.entry_document.saturating_add(1);
            }
            inner.maintenance_entry = None;
            inner.entry_requested = true;
        }
        let entry_ready = inner.entry_requested && completed >= inner.required_document;
        let entry_pending = inner.entry_requested && !entry_ready;
        let live_maintenance_document = inner.maintenance_entry.is_some()
            && inner.entry_document >= self.started_documents.load(Ordering::SeqCst);
        let connection = inner.connection.as_mut().unwrap();
        if let Some(session) = active_session(connection)? {
            self.require_maintenance_guard()?;
            // A live native commit may outlast its renderer. Absence of a
            // marker is decisive only after its worker stopped or process loss.
            let live_commit = !restart
                && session.phase == "committing-library"
                && !commit_exists(connection, &self.repository_root, &session)?;
            if (restart || entry_ready) && !live_commit {
                if restart
                    && ((session.operation == Operation::Capture
                        && matches!(session.phase.as_str(), "capturing" | "device-captured"))
                        || session.phase == "loading-source")
                {
                    connection.execute("UPDATE sessions SET phase='rolled-back',failure_code='interrupted-maintenance' WHERE id=?1",[&session.session_id])?;
                } else {
                    reconcile(connection, &self.repository_root, &session)?;
                }
            } else if !fresh_bootstrap
                && !entry_pending
                && live_maintenance_document
                && session.phase == "committing-library"
                && !live_commit
            {
                // The atomic PDS marker may survive a failed journal update.
                // This document already verified its writes, so retain that
                // proof. A new document takes the full reconciliation above.
                connection.execute(
                    "UPDATE sessions SET phase='committed' WHERE id=?1",
                    [&session.session_id],
                )?;
            }
        }
        let mut session = active_session(connection)?;
        inner.reconciled = true;
        if entry_ready {
            inner.maintenance_entry = session.as_ref().map(|s| s.session_id.clone());
            inner.entry_requested = false;
            inner.entry_document = completed;
        }
        if entry_pending || (!restart && session.is_some() && inner.maintenance_entry.is_none()) {
            if let Some(session) = &mut session {
                session.action = "await-navigation".into();
            }
        }
        Ok(BootstrapDecision {
            mode: if session.is_some() {
                "maintenance"
            } else {
                "normal"
            },
            session,
        })
    }

    pub(crate) fn session(&self, id: &str) -> Result<Session> {
        session_for(self.lock()?.connection.as_ref().unwrap(), id)
    }

    pub(crate) fn maintenance_entered(&self, id: &str) -> Result<bool> {
        let inner = self.lock()?;
        active_session_for(inner.connection.as_ref().unwrap(), id)?;
        Ok(inner.maintenance_entry.as_deref() == Some(id)
            && inner.entry_document >= self.started_documents.load(Ordering::SeqCst))
    }

    /// Native-only: source section and binary import has fully completed.
    pub(crate) fn source_ready(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.operation == Operation::Restore && session.phase == "loading-source",
            "Restore source is not loading",
        )?;
        for section in &session.selected_sections {
            spool::verify_section(connection, id, Spool::Source, section)?;
        }
        spool::verify_blobs(connection, id, Spool::Source)?;
        connection.execute("UPDATE sessions SET phase='preparing' WHERE id=?1", [id])?;
        Ok(())
    }

    pub(crate) fn prepared(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(session.phase == "preparing", "Restore is not preparing")?;
        for section in &session.selected_sections {
            spool::verify_section(connection, id, Spool::Source, section)?;
            spool::verify_section(connection, id, Spool::Rollback, section)?;
        }
        spool::verify_blobs(connection, id, Spool::Source)?;
        spool::verify_blobs(connection, id, Spool::Rollback)?;
        connection.execute(
            "UPDATE sessions SET phase='awaiting-native-preparation' WHERE id=?1",
            [id],
        )?;
        Ok(())
    }

    /// Native-only: the incoming library is validated and its objects are staged.
    pub(crate) fn allow_device_apply(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.phase == "awaiting-native-preparation",
            "Restore is not awaiting native preparation",
        )?;
        connection.execute("UPDATE sessions SET phase='prepared' WHERE id=?1", [id])?;
        Ok(())
    }

    /// Native-only: immutable native inventory and device spool export finished.
    pub(crate) fn confirm_capture(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.operation == Operation::Capture && session.phase == "device-captured",
            "Device capture is not awaiting native inventory",
        )?;
        connection.execute(
            "UPDATE sessions SET phase='capture-complete' WHERE id=?1",
            [id],
        )?;
        Ok(())
    }

    pub(crate) fn section_intent(&self, id: &str, section: &str, rollback: bool) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.selected_sections.iter().any(|s| s == section),
            "Section is not selected",
        )?;
        if rollback {
            require(
                session.phase == "rolling-back",
                "Rollback has not been selected",
            )?;
        } else {
            require(
                matches!(
                    session.phase.as_str(),
                    "prepared" | "applying-device" | "committed"
                ),
                "Device apply cannot begin in this phase",
            )?;
        }
        let spool = if rollback {
            Spool::Rollback
        } else {
            Spool::Source
        };
        spool::verify_section(connection, id, spool, section)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE selection SET intent=?3,verified_digest=NULL WHERE session=?1 AND section=?2",
            params![id, section, spool.key()],
        )?;
        if session.phase == "prepared" {
            transaction.execute(
                "UPDATE sessions SET phase='applying-device' WHERE id=?1",
                [id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn section_complete(
        &self,
        id: &str,
        section: &str,
        rollback: bool,
        digest: &str,
    ) -> Result<()> {
        validate_digest(digest)?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        let target = if rollback {
            Spool::Rollback
        } else {
            Spool::Source
        };
        require(
            if rollback {
                session.phase == "rolling-back"
            } else {
                matches!(session.phase.as_str(), "applying-device" | "committed")
            },
            "Section completion has wrong phase",
        )?;
        let expected = spool::verify_section(connection, id, target, section)?;
        require(
            expected.sha256 == digest,
            "Applied section digest does not match sealed spool",
        )?;
        let count = connection.execute(
            "UPDATE selection SET verified_digest=?4 WHERE session=?1 AND section=?2 AND intent=?3",
            params![id, section, target.key(), digest],
        )?;
        require(count == 1, "Section has no durable write intent")
    }

    pub(crate) fn finish_device(&self, id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        if session.operation == Operation::Capture {
            require(session.phase == "capturing", "Capture has wrong phase")?;
            for section in &session.selected_sections {
                spool::verify_section(connection, id, Spool::Source, section)?;
            }
            spool::verify_blobs(connection, id, Spool::Source)?;
            connection.execute(
                "UPDATE sessions SET phase='device-captured' WHERE id=?1",
                [id],
            )?;
        } else {
            require(
                session.phase == "applying-device",
                "Device apply is not running",
            )?;
            verify_completions(connection, id, Spool::Source)?;
            // Device-only success and its marker are one durable transaction.
            connection.execute(
                "UPDATE sessions SET phase=?2,device_committed=?3 WHERE id=?1",
                params![
                    id,
                    if session.includes_library {
                        "committing-library"
                    } else {
                        "committed"
                    },
                    !session.includes_library
                ],
            )?;
        }
        session_for(connection, id)
    }

    /// Native-only, after the same PDS transaction committed the exact marker.
    pub(crate) fn mark_library_committed(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.includes_library && session.phase == "committing-library",
            "Library commit has wrong phase",
        )?;
        require(
            read_commit_marker(&self.repository_root, &session)?.is_some(),
            "Library commit marker is absent",
        )?;
        connection.execute("UPDATE sessions SET phase='committed' WHERE id=?1", [id])?;
        Ok(())
    }

    /// Native-only: call after activation returned a transaction failure, never
    /// merely because cancellation was requested or the renderer disappeared.
    pub(crate) fn library_commit_failed(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.includes_library && session.phase == "committing-library",
            "Library failure has wrong phase",
        )?;
        require(
            !commit_exists(connection, &self.repository_root, &session)?,
            "Library commit already succeeded",
        )?;
        connection.execute("UPDATE sessions SET phase='rolling-back' WHERE id=?1", [id])?;
        Ok(())
    }

    pub(crate) fn commit_marker(&self, id: &str) -> Result<(String, CommitMarker)> {
        let session = self.session(id)?;
        Ok((
            marker_key(&session.job_id),
            CommitMarker {
                job_id: session.job_id,
                session_id: session.session_id,
                new_generation: session.new_generation,
            },
        ))
    }

    pub(crate) fn fail(&self, id: &str, code: &str) -> Result<Session> {
        self.fail_with_detail(id, code, None)
    }

    pub(crate) fn fail_with_detail(
        &self,
        id: &str,
        code: &str,
        detail: Option<FailureDetail>,
    ) -> Result<Session> {
        require(
            !code.is_empty()
                && code.len() <= 80
                && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid failure code",
        )?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        if let Some(detail) = &detail {
            require(
                session.selected_sections.contains(&detail.section_id),
                "Failure section is not selected",
            )?;
            require(
                detail.value_type.len() <= 128
                    && detail.value_type.is_ascii()
                    && detail.location.len() <= 512
                    && detail.location.is_ascii(),
                "Failure diagnostic exceeds structural bounds",
            )?;
        }
        let detail = detail
            .map(|detail| serde_json::to_string(&detail))
            .transpose()
            .map_err(|_| error("device-metadata-invalid", "Failure diagnostic is invalid"))?;
        require(
            session.phase != "committing-library",
            "Library activation has begun; cancellation is too late",
        )?;
        let committed = commit_exists(connection, &self.repository_root, &session)?;
        let phase = if committed || session.phase == "rolling-back" {
            "recovery-required"
        } else if session.operation == Operation::Capture
            || matches!(
                session.phase.as_str(),
                "preparing"
                    | "loading-source"
                    | "capturing"
                    | "device-captured"
                    | "capture-complete"
                    | "awaiting-native-preparation"
            )
        {
            "rolled-back"
        } else {
            "rolling-back"
        };
        connection.execute(
            "UPDATE sessions SET phase=?2,failure_code=?3,failure_detail=?4 WHERE id=?1",
            params![id, phase, code, detail],
        )?;
        session_for(connection, id)
    }

    /// Explicit retry keeps the bootstrap closed and uses the marker as authority.
    pub(crate) fn retry_recovery(&self, id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.phase == "recovery-required",
            "Recovery is not awaiting retry",
        )?;
        reconcile(connection, &self.repository_root, &session)?;
        session_for(connection, id)
    }

    /// User acknowledgement after maintenance verification, before normal reload.
    /// Spools remain available to the native archive worker until explicit cleanup.
    pub(crate) fn recovery_complete(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let cold_recovery = inner.cold_session.as_deref() == Some(id);
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        match session.phase.as_str() {
            "capture-complete" => {
                for section in &session.selected_sections {
                    spool::verify_section(connection, id, Spool::Source, section)?;
                }
            }
            "committed" => {
                require(
                    commit_exists(connection, &self.repository_root, &session)?,
                    "Commit marker is absent",
                )?;
                verify_completions(connection, id, Spool::Source)?;
            }
            "rolling-back" => {
                verify_completions(connection, id, Spool::Rollback)?;
            }
            "rolled-back" => {}
            _ => {
                return Err(error(
                    "device-recovery-pending",
                    "Device recovery has not completed",
                ))
            }
        }
        if cold_recovery {
            // There is no old native worker after process loss. Keep the journal
            // active on any cleanup failure, so retry still has its full spools.
            let transaction = connection.transaction()?;
            transaction.execute("DELETE FROM sessions WHERE id=?1", [id])?;
            reclaim_free_pages(&transaction)?;
            transaction.commit()?;
            inner.cold_session = None;
        } else {
            connection.execute("UPDATE sessions SET active=0,phase=CASE WHEN phase='rolling-back' THEN 'rolled-back' ELSE phase END WHERE id=?1",[id])?;
        }
        self.maintenance_guard
            .lock()
            .map_err(|_| {
                error(
                    "device-storage-failed",
                    "Device maintenance guard lock failed",
                )
            })?
            .take();
        self.startup_admission
            .lock()
            .map_err(|_| error("device-storage-failed", "Device admission lock failed"))?
            .take();
        Ok(())
    }

    /// Native-only cleanup, never called while restore/recovery is pending.
    /// Repeated cleanup also retries page reclamation after a partial failure.
    pub(crate) fn cleanup(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let active: Option<bool> = connection
            .query_row("SELECT active FROM sessions WHERE id=?1", [id], |r| {
                r.get(0)
            })
            .optional()?;
        require(active != Some(true), "Cannot clean pending recovery spools")?;
        connection.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        reclaim_free_pages(connection)
    }
}

fn reclaim_free_pages(connection: &Connection) -> Result<()> {
    // Reclaim bounded page groups without duplicating live rollback data.
    loop {
        let before: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        if before == 0 {
            return Ok(());
        }
        connection.execute_batch("PRAGMA incremental_vacuum(4096)")?;
        let after: i64 = connection.query_row("PRAGMA freelist_count", [], |row| row.get(0))?;
        require(
            after < before,
            "Device spool free pages could not be reclaimed",
        )?;
    }
}

fn open(root: &Path) -> Result<Connection> {
    std::fs::create_dir_all(root)?;
    require(
        !crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(root)?),
        "Device spool root is a link",
    )?;
    let path = root.join("coordinator.sqlite");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => require(
            metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata),
            "Device coordinator path is not a regular file",
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let connection = Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA foreign_keys=ON; PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA auto_vacuum=INCREMENTAL;
        CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,job_id TEXT NOT NULL,operation TEXT NOT NULL,phase TEXT NOT NULL,includes_library INTEGER NOT NULL,old_generation TEXT,new_generation TEXT,device_committed INTEGER NOT NULL DEFAULT 0,active INTEGER NOT NULL DEFAULT 1,failure_code TEXT,failure_detail TEXT);
        CREATE UNIQUE INDEX IF NOT EXISTS one_active_session ON sessions(active) WHERE active=1;
        CREATE TABLE IF NOT EXISTS selection(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,section TEXT NOT NULL,position INTEGER NOT NULL,intent TEXT,verified_digest TEXT,PRIMARY KEY(session,section));
        CREATE TABLE IF NOT EXISTS sections(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,spool TEXT NOT NULL,section TEXT NOT NULL,metadata TEXT NOT NULL,sealed INTEGER NOT NULL DEFAULT 0,records INTEGER NOT NULL DEFAULT 0,sha256 TEXT,PRIMARY KEY(session,spool,section));
        CREATE TABLE IF NOT EXISTS records(session TEXT NOT NULL,spool TEXT NOT NULL,section TEXT NOT NULL,ordinal INTEGER NOT NULL,payload BLOB NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(session,spool,section,ordinal),FOREIGN KEY(session,spool,section) REFERENCES sections(session,spool,section) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS blobs(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,spool TEXT NOT NULL,object_id TEXT NOT NULL,bytes INTEGER NOT NULL DEFAULT 0,sha256 TEXT,sealed INTEGER NOT NULL DEFAULT 0,metadata_only INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(session,spool,object_id));
        CREATE TABLE IF NOT EXISTS chunks(session TEXT NOT NULL,spool TEXT NOT NULL,object_id TEXT NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(session,spool,object_id,offset),FOREIGN KEY(session,spool,object_id) REFERENCES blobs(session,spool,object_id) ON DELETE CASCADE);")?;
    crate::trust_boundary::sync_directory(root)?;
    Ok(connection)
}

fn validate_id(id: &str) -> Result<()> {
    require(
        !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "Invalid opaque identifier",
    )
}
fn validate_digest(digest: &str) -> Result<()> {
    require(
        crate::trust_boundary::is_lower_hex_256(digest),
        "Invalid SHA-256 digest",
    )
}
fn validate_section(section: &str) -> Result<()> {
    let valid = matches!(section, "local-storage" | "localforage" | "device-settings")
        || section.strip_prefix("indexed-db:").is_some_and(|name| {
            name.starts_with("0073006100660065005f0070006c007500670069006e005f")
                && name.len() % 4 == 0
                && name.len() <= 65536
                && name.bytes().all(crate::trust_boundary::is_lower_hex_byte)
        });
    require(valid, "Invalid device section identifier")
}
fn active_session(connection: &Connection) -> Result<Option<Session>> {
    let id: Option<String> = connection
        .query_row("SELECT id FROM sessions WHERE active=1", [], |r| r.get(0))
        .optional()?;
    id.map(|id| session_for(connection, &id)).transpose()
}
fn session_for(connection: &Connection, id: &str) -> Result<Session> {
    validate_id(id)?;
    let mut session=connection.query_row("SELECT id,job_id,operation,phase,includes_library,old_generation,new_generation,failure_code,failure_detail FROM sessions WHERE id=?1",[id],|r|Ok(Session {session_id:r.get(0)?,job_id:r.get(1)?,operation:if r.get::<_,String>(2)?=="capture"{Operation::Capture}else{Operation::Restore},phase:r.get(3)?,includes_library:r.get(4)?,old_generation:r.get(5)?,new_generation:r.get(6)?,selected_sections:Vec::new(),action:String::new(),failure_code:r.get(7)?,failure_detail:None})).optional()?.ok_or_else(||error("device-session-missing","Device maintenance session is absent"))?;
    let detail: Option<String> = connection.query_row(
        "SELECT failure_detail FROM sessions WHERE id=?1",
        [id],
        |r| r.get(0),
    )?;
    session.failure_detail = detail
        .map(|detail| serde_json::from_str(&detail))
        .transpose()
        .map_err(|_| {
            error(
                "device-metadata-invalid",
                "Device failure diagnostic is malformed",
            )
        })?;
    let mut statement =
        connection.prepare("SELECT section FROM selection WHERE session=?1 ORDER BY position")?;
    session.selected_sections = statement
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    session.action = match session.phase.as_str() {
        "capturing" => "capture",
        "preparing" => "prepare",
        "loading-source" => "await-source",
        "rolling-back" => "rollback",
        "committed" => "reapply-source",
        "recovery-required" => "recovery-required",
        "committing-library" => "await-library",
        "device-captured" => "await-capture",
        "awaiting-native-preparation" => "await-native-preparation",
        "capture-complete" | "rolled-back" => "complete",
        _ => "continue",
    }
    .into();
    Ok(session)
}
fn verify_completions(connection: &Connection, id: &str, target: Spool) -> Result<()> {
    let missing:i64=connection.query_row("SELECT COUNT(*) FROM selection s LEFT JOIN sections p ON p.session=s.session AND p.section=s.section AND p.spool=?2 WHERE s.session=?1 AND (s.intent IS NULL OR s.intent<>?2 OR s.verified_digest IS NULL OR p.sha256 IS NULL OR s.verified_digest<>p.sha256)",params![id,target.key()],|r|r.get(0))?;
    require(
        missing == 0,
        "Selected device sections have not all been verified",
    )
}
fn marker_key(job_id: &str) -> String {
    format!("device-backup-commit:{job_id}")
}

/// Reads the sole commit authority without opening or migrating the PDS.
fn read_commit_marker(root: &Path, session: &Session) -> Result<Option<CommitMarker>> {
    let path = root.join("persistent").join("persistent.db");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => require(
            metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata),
            "PDS marker database is not a regular file",
        )?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
    let marker:Option<(i64,Option<String>)>=connection.query_row("SELECT length(CAST(value AS BLOB)),CASE WHEN length(CAST(value AS BLOB))<=4096 THEN value ELSE NULL END FROM app_kv WHERE key=?1",[marker_key(&session.job_id)],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    let Some((length, value)) = marker else {
        return Ok(None);
    };
    require(length <= 4096, "Library commit marker exceeds size bound")?;
    let marker: CommitMarker = serde_json::from_str(
        &value
            .ok_or_else(|| error("device-marker-invalid", "Library commit marker is not text"))?,
    )
    .map_err(|_| {
        error(
            "device-marker-invalid",
            "Library commit marker is malformed",
        )
    })?;
    require(
        marker.job_id == session.job_id
            && marker.session_id == session.session_id
            && marker.new_generation == session.new_generation,
        "Library commit marker identity mismatch",
    )?;
    Ok(Some(marker))
}
fn commit_exists(connection: &Connection, root: &Path, session: &Session) -> Result<bool> {
    if session.operation == Operation::Capture {
        Ok(false)
    } else if session.includes_library {
        Ok(read_commit_marker(root, session)?.is_some())
    } else {
        Ok(connection.query_row(
            "SELECT device_committed FROM sessions WHERE id=?1",
            [&session.session_id],
            |r| r.get(0),
        )?)
    }
}
fn reconcile(connection: &mut Connection, root: &Path, session: &Session) -> Result<()> {
    let committed = commit_exists(connection, root, session)?;
    if committed {
        spool::verify_blobs(connection, &session.session_id, Spool::Source)?;
    } else if !matches!(
        session.phase.as_str(),
        "capturing"
            | "preparing"
            | "loading-source"
            | "device-captured"
            | "capture-complete"
            | "rolled-back"
    ) {
        spool::verify_blobs(connection, &session.session_id, Spool::Rollback)?;
    }
    let transaction = connection.transaction()?;
    let phase = if committed {
        "committed"
    } else {
        match session.phase.as_str() {
            "capturing" => {
                transaction.execute(
                    "DELETE FROM sections WHERE session=?1",
                    [&session.session_id],
                )?;
                transaction.execute("DELETE FROM blobs WHERE session=?1", [&session.session_id])?;
                "capturing"
            }
            "loading-source" => "loading-source",
            "preparing" => {
                transaction.execute(
                    "DELETE FROM sections WHERE session=?1 AND spool='rollback'",
                    [&session.session_id],
                )?;
                transaction.execute(
                    "DELETE FROM blobs WHERE session=?1 AND spool='rollback'",
                    [&session.session_id],
                )?;
                "preparing"
            }
            "capture-complete" => "capture-complete",
            "device-captured" => "device-captured",
            "rolled-back" => "rolled-back",
            _ => "rolling-back",
        }
    };
    transaction.execute(
        "UPDATE sessions SET phase=?2 WHERE id=?1",
        params![session.session_id, phase],
    )?;
    if matches!(phase, "committed" | "rolling-back") {
        transaction.execute(
            "UPDATE selection SET intent=NULL,verified_digest=NULL WHERE session=?1",
            [&session.session_id],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests;

fn active_session_for(connection: &Connection, id: &str) -> Result<Session> {
    let session = session_for(connection, id)?;
    let active: bool =
        connection.query_row("SELECT active FROM sessions WHERE id=?1", [id], |r| {
            r.get(0)
        })?;
    require(active, "Device session is already released")?;
    Ok(session)
}

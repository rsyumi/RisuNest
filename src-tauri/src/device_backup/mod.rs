//! Durable, job-owned maintenance journal for native portable device data.
//!
//! Admission and the native PDS fence belong to the file job. Its owner creates
//! a session only after acquiring both, and keeps them until `is_blocking` is
//! false. Startup resumes the journal before normal app imports.

mod archive;
mod commands;
mod spool;
pub(crate) use archive::{
    apply_prepared_native_sections, capture_native_sections, capture_prepared_native_sections,
    journal_prepared_native_sections, prepare_native_sections,
    resume_journaled_native_restore, validate_archive_catalog, PreparedDeviceSection,
};
pub(crate) use commands::*;
pub(crate) use spool::{BlobManifest, RowPage, SectionManifest};

use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
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
    pub(crate) phase: String,
    pub(crate) includes_library: bool,
    pub(crate) selected_sections: Vec<String>,
    pub(crate) profile: String,
    pub(crate) expected_revision: Option<i64>,
    pub(crate) stage_id: Option<String>,
    pub(crate) action: String,
    pub(crate) failure_code: Option<String>,
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
}

struct Inner {
    connection: Option<Connection>,
    initialization_error: Option<DeviceBackupError>,
    reconciled: bool,
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
                cold_session,
            }),
            maintenance_guard: Mutex::new(None),
            startup_admission: Mutex::new(None),
        }
    }

    pub(crate) fn repository_root(&self) -> &Path {
        &self.repository_root
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
    pub(crate) fn create_native_portable_session(
        &self,
        job_id: &str,
        includes_library: bool,
        selected_sections: &[String],
        expected_revision: i64,
        stage_id: Option<String>,
    ) -> Result<String> {
        require(expected_revision >= 0, "Invalid native restore revision")?;
        require(
            includes_library == stage_id.is_some(),
            "Native portable library selection and stage do not match",
        )?;
        for section in selected_sections {
            risunest_external_storage_format::section::SectionKind::parse(section)
                .map_err(|_| error("device-invalid-state", "Invalid native portable section"))?;
        }
        if let Some(stage_id) = stage_id.as_deref() {
            validate_native_stage_id(stage_id)?;
        }
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
        let mut inner = self.lock()?;
        self.require_maintenance_guard()?;
        let connection = inner.connection.as_mut().unwrap();
        require(
            active_session(connection)?.is_none(),
            "Another device maintenance session is pending",
        )?;
        let id = Uuid::new_v4().to_string();
        let transaction = connection.transaction()?;
        transaction.execute("INSERT INTO sessions(id,job_id,phase,includes_library,profile,expected_revision,stage_id) VALUES(?1,?2,'loading-source',?3,'native-portable',?4,?5)", params![id,job_id,includes_library,expected_revision,stage_id])?;
        for (position, section) in selected_sections.iter().enumerate() {
            transaction.execute(
                "INSERT INTO selection(session,section,position) VALUES(?1,?2,?3)",
                params![id, section, position as i64],
            )?;
        }
        transaction.commit()?;
        inner.reconciled = true;
        inner.cold_session = None;
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

    pub(crate) fn bootstrap_for_entry(&self) -> Result<BootstrapDecision> {
        let mut inner = self.lock()?;
        let restart = !inner.reconciled;
        let connection = inner.connection.as_mut().unwrap();
        if restart {
            if let Some(session) = active_session(connection)? {
                require(
                    session.profile == "native-portable",
                    "Device recovery requires a native portable session",
                )?;
                if matches!(
                    session.phase.as_str(),
                    "prepared" | "applying-device" | "committing-library" | "committed"
                ) {
                    reconcile(connection, &self.repository_root, &session)?;
                }
            }
        }
        if active_session(connection)?.is_some() {
            self.require_maintenance_guard()?;
        }
        let session = active_session(connection)?;
        inner.reconciled = true;
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

    /// Native-only: source section and binary import has fully completed.
    pub(crate) fn source_ready(&self, id: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(session.phase == "loading-source", "Restore source is not loading")?;
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
        connection.execute("UPDATE sessions SET phase='prepared' WHERE id=?1", [id])?;
        Ok(())
    }

    pub(crate) fn section_intent(&self, id: &str, section: &str) -> Result<()> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            session.selected_sections.iter().any(|s| s == section),
            "Section is not selected",
        )?;
        require(
            matches!(
                session.phase.as_str(),
                "prepared" | "applying-device" | "committed"
            ),
            "Device apply cannot begin in this phase",
        )?;
        spool::verify_section(connection, id, Spool::Source, section)?;
        let transaction = connection.transaction()?;
        transaction.execute(
            "UPDATE selection SET intent=?3,verified_digest=NULL WHERE session=?1 AND section=?2",
            params![id, section, Spool::Source.key()],
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
        digest: &str,
    ) -> Result<()> {
        validate_digest(digest)?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            matches!(session.phase.as_str(), "applying-device" | "committed"),
            "Section completion has wrong phase",
        )?;
        let expected = spool::verify_section(connection, id, Spool::Source, section)?;
        require(
            expected.sha256 == digest,
            "Applied section digest does not match sealed spool",
        )?;
        let count = connection.execute(
            "UPDATE selection SET verified_digest=?4 WHERE session=?1 AND section=?2 AND intent=?3",
            params![id, section, Spool::Source.key(), digest],
        )?;
        require(count == 1, "Section has no durable write intent")
    }

    pub(crate) fn finish_device(&self, id: &str) -> Result<Session> {
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
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

    pub(crate) fn commit_marker(&self, id: &str) -> Result<(String, CommitMarker)> {
        let session = self.session(id)?;
        Ok((
            marker_key(&session.job_id),
            CommitMarker {
                job_id: session.job_id,
                session_id: session.session_id,
                new_generation: session.stage_id,
            },
        ))
    }

    pub(crate) fn library_commit_marker_exists(&self, id: &str) -> Result<bool> {
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        let session = active_session_for(connection, id)?;
        require(session.includes_library, "Library marker requires a library restore session")?;
        commit_exists(connection, &self.repository_root, &session)
    }

    pub(crate) fn pending_source_sections(&self, id: &str) -> Result<Vec<String>> {
        let inner = self.lock()?;
        let connection = inner.connection.as_ref().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            native_section_session(&session),
            "Native source progress requires a native restore session",
        )?;
        let mut statement = connection.prepare(
            "SELECT section FROM selection
                WHERE session=?1 AND (intent<>'source' OR intent IS NULL OR verified_digest IS NULL)
                ORDER BY position",
        )?;
        let pending = statement
            .query_map([id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(pending)
    }

    pub(crate) fn fail(&self, id: &str, code: &str) -> Result<Session> {
        require(
            !code.is_empty()
                && code.len() <= 80
                && code.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid failure code",
        )?;
        let mut inner = self.lock()?;
        let connection = inner.connection.as_mut().unwrap();
        let session = active_session_for(connection, id)?;
        require(
            matches!(
                session.phase.as_str(),
                "loading-source" | "preparing"
            ),
            "Accepted native restore must resume instead of failing",
        )?;
        connection.execute(
            "UPDATE sessions SET phase='rolled-back',failure_code=?2 WHERE id=?1",
            params![id, code],
        )?;
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
            "committed" => {
                require(
                    commit_exists(connection, &self.repository_root, &session)?,
                    "Commit marker is absent",
                )?;
                verify_completions(connection, id, Spool::Source)?;
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
            connection.execute("UPDATE sessions SET active=0 WHERE id=?1", [id])?;
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
        CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,job_id TEXT NOT NULL,phase TEXT NOT NULL,includes_library INTEGER NOT NULL,profile TEXT NOT NULL CHECK(profile='native-portable'),expected_revision INTEGER NOT NULL,stage_id TEXT,device_committed INTEGER NOT NULL DEFAULT 0,active INTEGER NOT NULL DEFAULT 1,failure_code TEXT);
        CREATE UNIQUE INDEX IF NOT EXISTS one_active_session ON sessions(active) WHERE active=1;
        CREATE TABLE IF NOT EXISTS selection(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,section TEXT NOT NULL,position INTEGER NOT NULL,intent TEXT,verified_digest TEXT,PRIMARY KEY(session,section));
        CREATE TABLE IF NOT EXISTS sections(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,spool TEXT NOT NULL,section TEXT NOT NULL,metadata TEXT NOT NULL,sealed INTEGER NOT NULL DEFAULT 0,records INTEGER NOT NULL DEFAULT 0,sha256 TEXT,PRIMARY KEY(session,spool,section));
        CREATE TABLE IF NOT EXISTS records(session TEXT NOT NULL,spool TEXT NOT NULL,section TEXT NOT NULL,ordinal INTEGER NOT NULL,payload BLOB NOT NULL,sha256 TEXT NOT NULL,PRIMARY KEY(session,spool,section,ordinal),FOREIGN KEY(session,spool,section) REFERENCES sections(session,spool,section) ON DELETE CASCADE);
        CREATE TABLE IF NOT EXISTS blobs(session TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,spool TEXT NOT NULL,object_id TEXT NOT NULL,bytes INTEGER NOT NULL DEFAULT 0,sha256 TEXT,sealed INTEGER NOT NULL DEFAULT 0,metadata_only INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(session,spool,object_id));
        CREATE TABLE IF NOT EXISTS chunks(session TEXT NOT NULL,spool TEXT NOT NULL,object_id TEXT NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(session,spool,object_id,offset),FOREIGN KEY(session,spool,object_id) REFERENCES blobs(session,spool,object_id) ON DELETE CASCADE);")?;
    crate::trust_boundary::sync_directory(root)?;
    Ok(connection)
}

pub(crate) fn active_native_portable_stage(root: &Path) -> Result<Option<String>> {
    let path = root.join("device-backup").join("coordinator.sqlite");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => require(
            metadata.is_file() && !crate::trust_boundary::is_link_like(&metadata),
            "Device coordinator path is not a regular file",
        )?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.execute_batch("PRAGMA trusted_schema=OFF; PRAGMA query_only=ON;")?;
    let row: Option<(String, bool, Option<String>)> = connection
        .query_row(
            "SELECT phase,includes_library,stage_id FROM sessions
                WHERE active=1 AND profile='native-portable'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((phase, includes_library, stage_id)) = row else {
        return Ok(None);
    };
    if !matches!(
        phase.as_str(),
        "loading-source"
            | "preparing"
            | "prepared"
            | "applying-device"
            | "committing-library"
    ) {
        return Ok(None);
    }
    if includes_library {
        let stage_id = stage_id.ok_or_else(|| {
            error(
                "device-invalid-state",
                "Active native portable restore has no library stage",
            )
        })?;
        validate_native_stage_id(&stage_id)?;
        Ok(Some(stage_id))
    } else {
        require(
            stage_id.is_none(),
            "Device-only native portable restore has a library stage",
        )?;
        Ok(None)
    }
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
fn validate_native_stage_id(stage_id: &str) -> Result<()> {
    validate_id(stage_id)?;
    let uuid = stage_id.strip_prefix("staging-").ok_or_else(|| {
        error(
            "device-invalid-state",
            "Invalid native portable stage identifier",
        )
    })?;
    Uuid::parse_str(uuid)
        .map(|_| ())
        .map_err(|_| {
            error(
                "device-invalid-state",
                "Invalid native portable stage identifier",
            )
        })
}
fn validate_digest(digest: &str) -> Result<()> {
    require(
        crate::trust_boundary::is_lower_hex_256(digest),
        "Invalid SHA-256 digest",
    )
}
fn validate_section(section: &str) -> Result<()> {
    risunest_external_storage_format::section::SectionKind::parse(section)
        .map(|_| ())
        .map_err(|_| error("device-invalid-state", "Invalid native portable section"))
}
fn active_session(connection: &Connection) -> Result<Option<Session>> {
    let id: Option<String> = connection
        .query_row("SELECT id FROM sessions WHERE active=1", [], |r| r.get(0))
        .optional()?;
    id.map(|id| session_for(connection, &id)).transpose()
}
fn session_for(connection: &Connection, id: &str) -> Result<Session> {
    validate_id(id)?;
    let mut session=connection.query_row("SELECT id,job_id,phase,includes_library,profile,expected_revision,stage_id,failure_code FROM sessions WHERE id=?1",[id],|r|Ok(Session {session_id:r.get(0)?,job_id:r.get(1)?,phase:r.get(2)?,includes_library:r.get(3)?,selected_sections:Vec::new(),profile:r.get(4)?,expected_revision:r.get(5)?,stage_id:r.get(6)?,action:String::new(),failure_code:r.get(7)?})).optional()?.ok_or_else(||error("device-session-missing","Device maintenance session is absent"))?;
    let mut statement =
        connection.prepare("SELECT section FROM selection WHERE session=?1 ORDER BY position")?;
    session.selected_sections = statement
        .query_map([id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    session.action = if session.phase == "committed" {
        "native-complete"
    } else {
        match session.phase.as_str() {
            "loading-source" => "await-source",
            "committing-library" => "await-library",
            "rolled-back" => "complete",
            _ => "continue",
        }
    }
    .into();
    Ok(session)
}

fn native_section_session(session: &Session) -> bool {
    session.profile == "native-portable"
        && !session.selected_sections.is_empty()
        && session.selected_sections.iter().all(|section| {
            risunest_external_storage_format::section::SectionKind::parse(section).is_ok()
        })
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
    let path = root
        .join("persistent")
        .join(crate::persistent_store::DATABASE_FILE);
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
            && marker.new_generation == session.stage_id,
        "Library commit marker identity mismatch",
    )?;
    Ok(Some(marker))
}
fn commit_exists(connection: &Connection, root: &Path, session: &Session) -> Result<bool> {
    if session.includes_library {
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
    } else if matches!(
        session.phase.as_str(),
        "prepared" | "applying-device" | "committing-library"
    ) {
        spool::verify_blobs(connection, &session.session_id, Spool::Rollback)?;
    }
    let transaction = connection.transaction()?;
    let phase = if committed {
        "committed"
    } else {
        match session.phase.as_str() {
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
            "rolled-back" => "rolled-back",
            _ => "applying-device",
        }
    };
    transaction.execute(
        "UPDATE sessions SET phase=?2 WHERE id=?1",
        params![session.session_id, phase],
    )?;
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

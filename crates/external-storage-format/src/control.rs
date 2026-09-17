//! Canonical encrypted head, backup point and backup bundle payloads.
use super::{
    section::SectionKind,
    snapshot::{
        bundle_fingerprint, LibrarySnapshotRef, ObjectRole, SectionSnapshotRef, StoredObject,
    },
    FormatError, Result,
};
use risunest_sync_wire::head::Sequence;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const HEAD_SCHEMA: &str = "risunest.external-head/v2";
const POINT_SCHEMA: &str = "risunest.external-backup-point/v2";
const BUNDLE_SCHEMA: &str = "risunest.external-backup-bundle/v1";
const LEASE_SCHEMA: &str = "risunest.external-lease/v1";
pub const MAX_CONTROL_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HeadDocument {
    pub schema: String,
    pub repository_id: String,
    pub library_id: String,
    pub commit_id: String,
    pub parent_commit_id: Option<String>,
    pub state_fingerprint: [u8; 32],
    pub state: StoredObject,
}

impl HeadDocument {
    pub fn new(
        repository_id: String,
        library_id: String,
        commit_id: String,
        parent_commit_id: Option<String>,
        state_fingerprint: [u8; 32],
        state: StoredObject,
    ) -> Result<Self> {
        let value = Self {
            schema: HEAD_SCHEMA.into(),
            repository_id,
            library_id,
            commit_id,
            parent_commit_id,
            state_fingerprint,
            state,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != HEAD_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.library_id.is_empty()
            || self.library_id.len() > 1024
            || self.commit_id.is_empty()
            || self.commit_id.len() > 1024
            || self
                .parent_commit_id
                .as_ref()
                .is_some_and(|value| value.is_empty() || value.len() > 1024)
        {
            return Err(FormatError("invalid-head"));
        }
        self.state.validate()?;
        if self.state.header.role != ObjectRole::SyncState
            || self.state.header.repository_id != self.repository_id
        {
            return Err(FormatError("invalid-head-state"));
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-head")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-head")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-head"));
        }
        Ok(value)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum BackupPointKind {
    Backup,
    History,
    Manual,
    Conflict,
}

/// A point names its bundles by purpose. A conflict preserves both sides in
/// full; every other kind carries exactly one.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupPointDocument {
    pub schema: String,
    pub repository_id: String,
    pub point_id: String,
    pub kind: BackupPointKind,
    pub created_at_ms: u64,
    pub bundle: Option<StoredObject>,
    pub local_bundle: Option<StoredObject>,
    pub remote_bundle: Option<StoredObject>,
}

impl BackupPointDocument {
    pub fn single(
        repository_id: String,
        point_id: String,
        kind: BackupPointKind,
        created_at_ms: u64,
        bundle: StoredObject,
    ) -> Result<Self> {
        let value = Self {
            schema: POINT_SCHEMA.into(),
            repository_id,
            point_id,
            kind,
            created_at_ms,
            bundle: Some(bundle),
            local_bundle: None,
            remote_bundle: None,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn conflict(
        repository_id: String,
        point_id: String,
        created_at_ms: u64,
        local_bundle: StoredObject,
        remote_bundle: StoredObject,
    ) -> Result<Self> {
        let value = Self {
            schema: POINT_SCHEMA.into(),
            repository_id,
            point_id,
            kind: BackupPointKind::Conflict,
            created_at_ms,
            bundle: None,
            local_bundle: Some(local_bundle),
            remote_bundle: Some(remote_bundle),
        };
        value.validate()?;
        Ok(value)
    }
    pub fn bundles(&self) -> Vec<&StoredObject> {
        [&self.bundle, &self.local_bundle, &self.remote_bundle]
            .into_iter()
            .flatten()
            .collect()
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != POINT_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.point_id.is_empty()
            || self.point_id.len() > 1024
        {
            return Err(FormatError("invalid-backup-point"));
        }
        match self.kind {
            BackupPointKind::Conflict => {
                let (Some(local), Some(remote)) = (&self.local_bundle, &self.remote_bundle) else {
                    return Err(FormatError("invalid-backup-point"));
                };
                if self.bundle.is_some() {
                    return Err(FormatError("invalid-backup-point"));
                }
                if local.header.object_id == remote.header.object_id {
                    return Err(FormatError("duplicate-conflict-bundle"));
                }
            }
            _ => {
                if self.bundle.is_none()
                    || self.local_bundle.is_some()
                    || self.remote_bundle.is_some()
                {
                    return Err(FormatError("invalid-backup-point"));
                }
            }
        }
        for bundle in self.bundles() {
            bundle.validate()?;
            if bundle.header.role != ObjectRole::BackupBundle
                || bundle.header.repository_id != self.repository_id
            {
                return Err(FormatError("invalid-backup-point-bundle"));
            }
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-backup-point")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-backup-point")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-backup-point"));
        }
        Ok(value)
    }
}

/// Who captured a bundle. A device bundle holds one device's own values; a
/// synchronized state holds the merged result of several, so a restore never
/// mistakes the publisher for the device the data came from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum BundleSource {
    Device { writer_id: String },
    SyncState { commit_id: String },
}

impl BundleSource {
    fn validate(&self) -> Result<()> {
        let value = match self {
            Self::Device { writer_id } => writer_id,
            Self::SyncState { commit_id } => commit_id,
        };
        if value.is_empty() || value.len() > 1024 {
            return Err(FormatError("invalid-bundle-source"));
        }
        Ok(())
    }
}

/// A backup bundle authenticates its own coverage. The declared list and the
/// references have to agree exactly, so a restore never guesses the scope from
/// the connection's current settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BackupBundleDocument {
    pub schema: String,
    pub repository_id: String,
    pub bundle_id: String,
    pub source: BundleSource,
    pub captured_at_ms: u64,
    pub local_library_revision: Option<Sequence>,
    pub local_device_revision: Option<Sequence>,
    pub remote_generation: Option<Sequence>,
    pub included_sections: Vec<String>,
    pub library: LibrarySnapshotRef,
    pub sections: BTreeMap<String, SectionSnapshotRef>,
    pub bundle_fingerprint: [u8; 32],
}

impl BackupBundleDocument {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        repository_id: String,
        bundle_id: String,
        source: BundleSource,
        captured_at_ms: u64,
        local_library_revision: Option<Sequence>,
        local_device_revision: Option<Sequence>,
        remote_generation: Option<Sequence>,
        library: LibrarySnapshotRef,
        sections: BTreeMap<String, SectionSnapshotRef>,
    ) -> Result<Self> {
        let value = Self {
            schema: BUNDLE_SCHEMA.into(),
            repository_id,
            bundle_id,
            source,
            captured_at_ms,
            local_library_revision,
            local_device_revision,
            remote_generation,
            included_sections: sections.keys().cloned().collect(),
            bundle_fingerprint: bundle_fingerprint(&library.content_fingerprint, &sections),
            library,
            sections,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != BUNDLE_SCHEMA
            || self.repository_id.is_empty()
            || self.repository_id.len() > 128
            || self.bundle_id.is_empty()
            || self.bundle_id.len() > 1024
        {
            return Err(FormatError("invalid-backup-bundle"));
        }
        self.source.validate()?;
        self.library.validate(&self.repository_id)?;
        let mut previous: Option<&str> = None;
        for id in &self.included_sections {
            if previous.is_some_and(|value| value >= id.as_str()) {
                return Err(FormatError("invalid-included-sections"));
            }
            SectionKind::parse(id)?;
            previous = Some(id);
        }
        // An incomplete backup fails before anything is activated.
        if self.included_sections.len() != self.sections.len()
            || !self
                .included_sections
                .iter()
                .all(|id| self.sections.contains_key(id))
        {
            return Err(FormatError("backup-bundle-coverage-mismatch"));
        }
        for (id, section) in &self.sections {
            section.validate(&self.repository_id, id)?;
        }
        if self.bundle_fingerprint
            != bundle_fingerprint(&self.library.content_fingerprint, &self.sections)
        {
            return Err(FormatError("invalid-bundle-fingerprint"));
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-backup-bundle")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-backup-bundle")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-backup-bundle"));
        }
        Ok(value)
    }
}

/// What a lease announces. `Deleting` marks one removal attempt; the other two
/// announce that a device is reading or writing the repository.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum LeaseKind {
    Work,
    Cleanup,
    Deleting,
}

/// The body of one lease object. The identity of the object is its name; this
/// says who placed it and which renewal it is, which only a diagnostic read
/// needs. `seq` counts renewals of one job's lease and never leaves this
/// device's storage as a clock.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LeaseDocument {
    pub schema: String,
    pub writer_id: String,
    pub job_id: String,
    pub kind: LeaseKind,
    pub seq: u64,
    pub created_at_ms: u64,
}

impl LeaseDocument {
    pub fn new(
        writer_id: String,
        job_id: String,
        kind: LeaseKind,
        seq: u64,
        created_at_ms: u64,
    ) -> Result<Self> {
        let value = Self {
            schema: LEASE_SCHEMA.into(),
            writer_id,
            job_id,
            kind,
            seq,
            created_at_ms,
        };
        value.validate()?;
        Ok(value)
    }
    pub fn validate(&self) -> Result<()> {
        if self.schema != LEASE_SCHEMA
            || self.writer_id.is_empty()
            || self.writer_id.len() > 1024
            || self.job_id.is_empty()
            || self.job_id.len() > 1024
        {
            return Err(FormatError("invalid-lease"));
        }
        Ok(())
    }
    pub fn encode(&self, max_bytes: usize) -> Result<Vec<u8>> {
        encode(self, max_bytes, "invalid-lease")
    }
    pub fn decode(bytes: &[u8], max_bytes: usize) -> Result<Self> {
        let value: Self = decode(bytes, max_bytes, "invalid-lease")?;
        value.validate()?;
        if value.encode(max_bytes)? != bytes {
            return Err(FormatError("non-canonical-lease"));
        }
        Ok(value)
    }
}

fn encode(value: &impl Serialize, max_bytes: usize, error: &'static str) -> Result<Vec<u8>> {
    let bytes = serde_json::to_vec(value).map_err(|_| FormatError(error))?;
    if bytes.is_empty() || bytes.len() > max_bytes.min(MAX_CONTROL_BYTES) {
        return Err(FormatError("control-limit-exceeded"));
    }
    Ok(bytes)
}
fn decode<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    max_bytes: usize,
    error: &'static str,
) -> Result<T> {
    if bytes.is_empty() || bytes.len() > max_bytes.min(MAX_CONTROL_BYTES) {
        return Err(FormatError("control-limit-exceeded"));
    }
    serde_json::from_slice(bytes).map_err(|_| FormatError(error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        section::{SECTION_CODEC, SectionKind},
        snapshot::{state_fingerprint, PublicObjectHeader, SyncStateDocument, WireLocator},
    };

    pub(crate) fn stored(id: &str, role: ObjectRole) -> StoredObject {
        let header = PublicObjectHeader::new("repository".into(), id.into(), role, 10).unwrap();
        StoredObject {
            ciphertext_length: crate::snapshot::envelope_length(&header).unwrap(),
            ciphertext_sha256: [2; 32],
            plaintext_length: 10,
            plaintext_sha256: [3; 32],
            locator: WireLocator {
                connection_identity: "account/root".into(),
                collection: None,
                object: format!("opaque-{id}"),
            },
            header,
        }
    }

    pub(crate) fn library() -> LibrarySnapshotRef {
        LibrarySnapshotRef {
            record_catalog: stored("catalog-records", ObjectRole::Catalog),
            asset_catalog: stored("catalog-assets", ObjectRole::Catalog),
            content_fingerprint: [9; 32],
        }
    }

    pub(crate) fn section(kind: SectionKind, generation: u64) -> SectionSnapshotRef {
        SectionSnapshotRef {
            kind,
            codec: SECTION_CODEC.into(),
            generation: Sequence::from(generation),
            gc_floor: Sequence::from(0u64),
            max_write_clock: Sequence::from(generation + 40),
            entries_root: stored(
                &format!("catalog-section-{}", kind.id()),
                ObjectRole::Catalog,
            ),
            content_fingerprint: [u8::try_from(generation % 251).unwrap(); 32],
        }
    }

    fn state(sections: BTreeMap<String, SectionSnapshotRef>) -> SyncStateDocument {
        SyncStateDocument::new(
            "state-synthetic".into(),
            "repository".into(),
            "library".into(),
            "epoch".into(),
            Sequence::from(3u64),
            None,
            "writer-a".into(),
            1,
            library(),
            sections,
        )
        .unwrap()
    }

    #[test]
    fn control_documents_are_canonical_and_conflicts_require_two_distinct_bundles() {
        let head = HeadDocument::new(
            "repository".into(),
            "library".into(),
            "commit".into(),
            None,
            [5; 32],
            stored("state-a", ObjectRole::SyncState),
        )
        .unwrap();
        let encoded = head.encode(4096).unwrap();
        assert_eq!(HeadDocument::decode(&encoded, 4096).unwrap(), head);
        // A head has to point at a state, never at a bundle.
        assert!(HeadDocument::new(
            "repository".into(),
            "library".into(),
            "commit".into(),
            None,
            [5; 32],
            stored("bundle-a", ObjectRole::BackupBundle),
        )
        .is_err());

        assert!(BackupPointDocument::conflict(
            "repository".into(),
            "point".into(),
            1,
            stored("bundle-a", ObjectRole::BackupBundle),
            stored("bundle-a", ObjectRole::BackupBundle),
        )
        .is_err());
        let point = BackupPointDocument::conflict(
            "repository".into(),
            "point".into(),
            1,
            stored("bundle-a", ObjectRole::BackupBundle),
            stored("bundle-b", ObjectRole::BackupBundle),
        )
        .unwrap();
        assert_eq!(
            BackupPointDocument::decode(&point.encode(8192).unwrap(), 8192).unwrap(),
            point
        );
        let single = BackupPointDocument::single(
            "repository".into(),
            "point".into(),
            BackupPointKind::Manual,
            1,
            stored("bundle-a", ObjectRole::BackupBundle),
        )
        .unwrap();
        assert_eq!(single.bundles().len(), 1);
        let mut broken = single.clone();
        broken.local_bundle = Some(stored("bundle-b", ObjectRole::BackupBundle));
        assert!(broken.validate().is_err());
        let mut emptied = single;
        emptied.bundle = None;
        assert!(emptied.validate().is_err());
    }

    /// GC27: a lease body survives the round trip unchanged, so a retry after a
    /// lost answer can resend the same bytes it stored.
    #[test]
    fn a_lease_roundtrips_and_refuses_an_unnamed_writer_or_job() {
        let lease = LeaseDocument::new(
            "writer".into(),
            "job".into(),
            LeaseKind::Deleting,
            4,
            1_700_000_000_000,
        )
        .unwrap();
        let encoded = lease.encode(4096).unwrap();
        assert_eq!(LeaseDocument::decode(&encoded, 4096).unwrap(), lease);
        assert!(std::str::from_utf8(&encoded).unwrap().contains("\"deleting\""));
        assert!(LeaseDocument::new("".into(), "job".into(), LeaseKind::Work, 0, 1).is_err());
        assert!(LeaseDocument::new("writer".into(), "".into(), LeaseKind::Work, 0, 1).is_err());
        let mut reordered: serde_json::Value = serde_json::from_slice(&encoded).unwrap();
        reordered["schema"] = serde_json::json!("risunest.external-lease/v2");
        assert!(LeaseDocument::decode(&serde_json::to_vec(&reordered).unwrap(), 4096).is_err());
    }

    #[test]
    fn a_synchronized_state_refuses_device_fixed_and_unknown_sections() {
        let hypa = BTreeMap::from([(SectionKind::Hypa.id().into(), section(SectionKind::Hypa, 3))]);
        let document = state(hypa.clone());
        assert_eq!(
            SyncStateDocument::decode(&document.encode(16 * 1024).unwrap(), 16 * 1024).unwrap(),
            document
        );
        let mut settings = document.clone();
        settings.sections.insert(
            SectionKind::LocalSettings.id().into(),
            section(SectionKind::LocalSettings, 3),
        );
        settings.state_fingerprint =
            state_fingerprint(&settings.library_fingerprint, &settings.sections);
        assert_eq!(
            settings.validate(),
            Err(FormatError("section-not-synchronizable"))
        );
        let mut unknown = document.clone();
        let reference = unknown.sections.remove(SectionKind::Hypa.id()).unwrap();
        unknown.sections.insert("device".into(), reference);
        unknown.state_fingerprint =
            state_fingerprint(&unknown.library_fingerprint, &unknown.sections);
        assert_eq!(unknown.validate(), Err(FormatError("unknown-section")));
        // A reference cannot sit under a key that names a different section.
        let mut mismatched = document.clone();
        mismatched.sections.insert(
            SectionKind::LocalPlugins.id().into(),
            section(SectionKind::Hypa, 4),
        );
        mismatched.state_fingerprint =
            state_fingerprint(&mismatched.library_fingerprint, &mismatched.sections);
        assert_eq!(
            mismatched.validate(),
            Err(FormatError("section-kind-mismatch"))
        );
    }

    #[test]
    fn a_control_change_alone_moves_the_state_fingerprint_but_not_the_library() {
        let quiet = state(BTreeMap::from([(
            SectionKind::Hypa.id().into(),
            section(SectionKind::Hypa, 3),
        )]));
        let mut moved_floor = section(SectionKind::Hypa, 3);
        moved_floor.gc_floor = Sequence::from(2u64);
        let collected = state(BTreeMap::from([(
            SectionKind::Hypa.id().into(),
            moved_floor,
        )]));
        assert_eq!(quiet.library_fingerprint, collected.library_fingerprint);
        assert_eq!(
            quiet.sections[SectionKind::Hypa.id()].content_fingerprint,
            collected.sections[SectionKind::Hypa.id()].content_fingerprint
        );
        assert_ne!(quiet.state_fingerprint, collected.state_fingerprint);
        // Publishing a section leaves the library identity alone.
        let library_only = state(BTreeMap::new());
        assert_eq!(library_only.library_fingerprint, quiet.library_fingerprint);
        assert_ne!(library_only.state_fingerprint, quiet.state_fingerprint);
    }

    #[test]
    fn a_bundle_declares_its_own_coverage_and_an_empty_section_is_not_an_absent_one() {
        let sections = BTreeMap::from([
            (SectionKind::Hypa.id().into(), section(SectionKind::Hypa, 1)),
            (
                SectionKind::LocalSettings.id().into(),
                section(SectionKind::LocalSettings, 1),
            ),
        ]);
        let bundle = BackupBundleDocument::new(
            "repository".into(),
            "bundle-a".into(),
            BundleSource::Device {
                writer_id: "writer-a".into(),
            },
            1,
            Some(Sequence::from(12u64)),
            Some(Sequence::from(4u64)),
            None,
            library(),
            sections.clone(),
        )
        .unwrap();
        assert_eq!(
            bundle.included_sections,
            vec!["hypa".to_string(), "local-settings".to_string()]
        );
        assert_eq!(
            BackupBundleDocument::decode(&bundle.encode(32 * 1024).unwrap(), 32 * 1024).unwrap(),
            bundle
        );
        // A selected but empty section is still included, and differs from a
        // section the capture left out.
        let empty = BackupBundleDocument::new(
            "repository".into(),
            "bundle-b".into(),
            BundleSource::SyncState {
                commit_id: "commit-a".into(),
            },
            1,
            None,
            None,
            Some(Sequence::from(9u64)),
            library(),
            BTreeMap::new(),
        )
        .unwrap();
        assert!(empty.included_sections.is_empty());
        assert_ne!(empty.bundle_fingerprint, bundle.bundle_fingerprint);

        let mut declared_more = bundle.clone();
        declared_more
            .included_sections
            .insert(1, SectionKind::LocalPlugins.id().into());
        assert_eq!(
            declared_more.validate(),
            Err(FormatError("backup-bundle-coverage-mismatch"))
        );
        let mut unsorted = bundle.clone();
        unsorted.included_sections.reverse();
        assert_eq!(
            unsorted.validate(),
            Err(FormatError("invalid-included-sections"))
        );
        let mut declared_less = bundle;
        declared_less.included_sections.pop();
        assert_eq!(
            declared_less.validate(),
            Err(FormatError("backup-bundle-coverage-mismatch"))
        );
    }

    /// Two devices backing up the same repository produce separate points whose
    /// contents are never merged, and a conflict point names both complete
    /// bundles rather than a combination of them.
    #[test]
    fn a_backup_point_belongs_to_one_device_and_a_conflict_keeps_both_whole() {
        let bundle = |id: &str, writer: &str, kind: SectionKind| {
            BackupBundleDocument::new(
                "repository".into(),
                id.into(),
                BundleSource::Device {
                    writer_id: writer.into(),
                },
                1,
                Some(Sequence::from(12u64)),
                Some(Sequence::from(4u64)),
                None,
                library(),
                BTreeMap::from([(kind.id().into(), section(kind, 1))]),
            )
            .unwrap()
        };
        let first = bundle("bundle-a", "writer-a", SectionKind::Hypa);
        let second = bundle("bundle-b", "writer-b", SectionKind::LocalPlugins);
        assert_ne!(first.bundle_id, second.bundle_id);
        assert_ne!(first.source, second.source);
        assert_ne!(first.included_sections, second.included_sections);
        assert_ne!(first.bundle_fingerprint, second.bundle_fingerprint);

        let point = |id: &str, bundle: &str| {
            BackupPointDocument::single(
                "repository".into(),
                id.into(),
                BackupPointKind::Backup,
                1,
                stored(bundle, ObjectRole::BackupBundle),
            )
            .unwrap()
        };
        let own = point("point-a", "bundle-a");
        let other = point("point-b", "bundle-b");
        assert_eq!(own.bundles().len(), 1);
        assert_eq!(other.bundles().len(), 1);
        assert_ne!(own.bundles()[0].header.object_id, other.bundles()[0].header.object_id);

        let conflict = BackupPointDocument::conflict(
            "repository".into(),
            "point-conflict".into(),
            1,
            stored("bundle-a", ObjectRole::BackupBundle),
            stored("bundle-b", ObjectRole::BackupBundle),
        )
        .unwrap();
        assert!(conflict.bundle.is_none());
        assert_eq!(
            conflict
                .bundles()
                .iter()
                .map(|object| object.header.object_id.as_str())
                .collect::<Vec<_>>(),
            vec!["bundle-a", "bundle-b"]
        );
    }

    /// A repository created while every device section was switched off still
    /// accepts those sections later. Neither the repository identity nor the
    /// recovery information has to be rebuilt for that.
    #[test]
    fn turning_every_section_off_at_creation_is_reversible() {
        use crate::format::{Descriptor, Strategy};
        let created_with_nothing = Descriptor::new("repository".into(), Some(Strategy::Cas)).unwrap();
        let library_only = state(BTreeMap::new());
        assert!(library_only.sections.is_empty());

        let later = state(BTreeMap::from([
            (SectionKind::Hypa.id().into(), section(SectionKind::Hypa, 4)),
            (
                SectionKind::LocalPlugins.id().into(),
                section(SectionKind::LocalPlugins, 4),
            ),
        ]));
        assert!(later.validate().is_ok());
        assert_eq!(later.repository_id, created_with_nothing.repository_id);
        // The descriptor another device fetches is byte-identical to the one
        // the creating device wrote, so no device reports the other corrupt.
        let fetched = Descriptor::decode(&serde_json::to_vec(&created_with_nothing).unwrap()).unwrap();
        assert_eq!(fetched, created_with_nothing);
    }

    /// Absence of a key means never published. An emptied section keeps a valid
    /// reference, so a restore can tell "leave this alone" from "clear this".
    #[test]
    fn an_absent_section_is_distinguishable_from_an_emptied_one() {
        let absent = state(BTreeMap::new());
        let emptied = state(BTreeMap::from([(
            SectionKind::Hypa.id().into(),
            section(SectionKind::Hypa, 5),
        )]));
        assert!(!absent.sections.contains_key(SectionKind::Hypa.id()));
        assert!(emptied.sections.contains_key(SectionKind::Hypa.id()));
        assert_ne!(absent.state_fingerprint, emptied.state_fingerprint);
        assert_eq!(absent.library_fingerprint, emptied.library_fingerprint);
    }

    #[test]
    fn previous_schema_control_documents_are_reported_rather_than_narrowed() {
        let previous_head = serde_json::json!({
            "schema": "risunest.external-head/v1",
            "repositoryId": "repository",
            "libraryId": "library",
            "commitId": "commit",
            "parentCommitId": null,
            "scopeId": vec![4u8; 32],
            "fingerprint": vec![5u8; 32],
            "snapshot": stored("snapshot-a", ObjectRole::SyncState),
        });
        assert!(HeadDocument::decode(
            &serde_json::to_vec(&previous_head).unwrap(),
            MAX_CONTROL_BYTES
        )
        .is_err());
        let previous_point = serde_json::json!({
            "schema": "risunest.external-backup-point/v1",
            "repositoryId": "repository",
            "pointId": "point",
            "kind": "manual",
            "createdAtMs": 1,
            "logicalRevision": 7,
            "scopeId": vec![4u8; 32],
            "snapshots": [stored("snapshot-a", ObjectRole::BackupBundle)],
        });
        assert!(BackupPointDocument::decode(
            &serde_json::to_vec(&previous_point).unwrap(),
            MAX_CONTROL_BYTES
        )
        .is_err());
    }
}

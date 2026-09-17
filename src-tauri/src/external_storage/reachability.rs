//! What the current roots still reach, and which enumerated states and bundles
//! a cleanup may remove. This stage reads documents and decides; it never
//! deletes. A root this device cannot read ends the run with nothing to remove.
// The run that consumes a mark is wired with the removal path; the usage
// summary is the only reader until then.
#![cfg_attr(not(test), allow(dead_code))]
use super::{
    contract::{
        Cancellation, ErrorKind, ObjectReceipt, ObjectRole, ProviderError, ProviderFuture, Result,
    },
    control,
    gc_store::{locator_key, CommittedDeletion, GcStore},
    packaging::RemoteObject,
};
use risunest_external_storage_format::snapshot as wire;
use std::collections::{BTreeMap, BTreeSet};

/// How long a state or bundle stays protected after this device first saw it,
/// and how long an ancestor stays protected after its successor appeared.
pub(crate) const GRACE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn missing() -> ProviderError {
    ProviderError::new(ErrorKind::NotFound)
}

/// Every object a state or bundle document names directly: both library
/// catalogs and one entries root per published section. A publication that
/// only changed the library still names the sections it carried forward.
pub(crate) fn document_references(view: &control::SnapshotView) -> Vec<&wire::StoredObject> {
    [&view.library.record_catalog, &view.library.asset_catalog]
        .into_iter()
        .chain(view.sections.values().map(|section| &section.entries_root))
        .collect()
}

/// One state or bundle document reduced to what a mark follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentNode {
    pub snapshot_id: String,
    /// The state this one replaced, when the document names one. A bundle
    /// wrapped around a capture has no lineage of its own.
    pub parent_snapshot_id: Option<String>,
    pub references: Vec<RemoteObject>,
}

/// Reads the immutable documents a mark follows. Where a failed read leaves the
/// run is decided by the caller, not here: under a root it ends the run, under
/// a candidate it only drops that candidate.
pub(crate) trait DocumentSource: Sync {
    /// A state or bundle this device already holds an authenticated reference
    /// to, such as the one the head names.
    fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode>;
    /// An enumerated state or bundle, whose identity comes from its envelope.
    fn listed<'a>(
        &'a self,
        receipt: &'a ObjectReceipt,
    ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)>;
    /// The catalog nodes and packs one catalog node names.
    fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>>;
}

/// What this run must not remove, gathered before the mark starts.
pub(crate) struct Roots {
    /// The state the current head names. A backup-only repository has none.
    pub head: Option<RemoteObject>,
    /// Backup point documents the retention decision keeps.
    pub kept_points: Vec<RemoteObject>,
    /// The bundles those points name. A conflict point names two.
    pub kept_bundles: Vec<RemoteObject>,
    /// Objects an unfinished job on this device reads or writes, including
    /// fragments it uploaded before any document could name them.
    pub job_objects: Vec<RemoteObject>,
    /// States and bundles an unfinished job on this device targets.
    pub job_snapshot_ids: BTreeSet<String>,
}

pub(crate) struct MarkRequest<'a> {
    pub connection_id: &'a str,
    pub now_ms: u64,
    pub roots: Roots,
    /// Everything the snapshot collection answered, read to the end.
    pub listed: Vec<ObjectReceipt>,
    /// Backup point documents the retention decision dropped. The bundles they
    /// named come back from the snapshot enumeration on their own.
    pub dropped_points: Vec<RemoteObject>,
}

/// The ledger target that carries when this device first saw one state take the
/// position that displaces its parent. A state identifier never holds a slash,
/// so this can never collide with the state's own row.
fn displacement_of(snapshot_id: &str) -> String {
    format!("head/{snapshot_id}")
}

pub(crate) struct Mark {
    /// Every object the roots reach, keyed the way the cleanup tables key one.
    pub reachable: BTreeSet<String>,
    pub reachable_bytes: u64,
    /// The committed list this run left behind, each parent ahead of the
    /// objects below it. It is already stored when the mark answers.
    pub candidates: Vec<CommittedDeletion>,
    /// Enumerated documents this run could not read. It removes neither them
    /// nor anything under them.
    pub skipped: usize,
    /// The stored list could not be read, so this run started a new one and
    /// whatever the old one named is no longer nameable.
    pub list_recomputed: bool,
}

fn candidate_of(object: &RemoteObject, now_ms: u64) -> CommittedDeletion {
    CommittedDeletion {
        locator: object.receipt.locator.clone(),
        role: object.role,
        byte_length: object.receipt.byte_length,
        decided_at_ms: now_ms,
        done: false,
    }
}

type Listed = BTreeMap<String, (RemoteObject, DocumentNode)>;

/// The objects one already visited object names.
async fn expand(
    object: &RemoteObject,
    source: &dyn DocumentSource,
    listed: &Listed,
) -> Result<Vec<RemoteObject>> {
    match object.role {
        ObjectRole::SyncState | ObjectRole::BackupBundle => {
            let key = locator_key(&object.receipt.locator)?;
            match listed.get(&key) {
                Some((_, node)) => Ok(node.references.clone()),
                None => Ok(source.document(object).await?.references),
            }
        }
        ObjectRole::Catalog => source.catalog(object).await,
        ObjectRole::Pack
        | ObjectRole::BackupPoint
        | ObjectRole::Descriptor
        | ObjectRole::Lease => Ok(Vec::new()),
    }
}

/// Walks outward from one starting object. Parents are answered before the
/// objects below them, which is the order a removal has to follow.
async fn walk(
    start: Vec<RemoteObject>,
    source: &dyn DocumentSource,
    listed: &Listed,
    visited: &mut BTreeSet<String>,
    cancel: &Cancellation,
) -> Result<Vec<RemoteObject>> {
    let mut queue = std::collections::VecDeque::from(start);
    let mut reached = Vec::new();
    while let Some(object) = queue.pop_front() {
        cancel.check()?;
        let key = locator_key(&object.receipt.locator)?;
        if !visited.insert(key) {
            continue;
        }
        for next in expand(&object, source, listed).await? {
            queue.push_back(next);
        }
        reached.push(object);
    }
    Ok(reached)
}

/// Decides what the repository still reaches and what this run may remove.
///
/// A root that cannot be read, an ancestor the enumeration no longer holds or a
/// catalog under a root that cannot be opened all end the run with an error and
/// leave the ledger untouched. An enumerated document that cannot be read only
/// takes itself and everything under it out of this run.
pub(crate) async fn mark(
    request: MarkRequest<'_>,
    source: &dyn DocumentSource,
    store: &GcStore,
    cancel: &Cancellation,
) -> Result<Mark> {
    let mut listed: Listed = BTreeMap::new();
    let mut by_snapshot: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped = 0usize;
    for receipt in &request.listed {
        cancel.check()?;
        let key = locator_key(&receipt.locator)?;
        match source.listed(receipt).await {
            Ok((object, node)) => {
                by_snapshot.insert(node.snapshot_id.clone(), key.clone());
                listed.insert(key, (object, node));
            }
            Err(_) => skipped += 1,
        }
    }

    // The head is a root, so failing to read it ends the run here.
    let head_node = match &request.roots.head {
        Some(head) => {
            let key = locator_key(&head.receipt.locator)?;
            Some(match listed.get(&key) {
                Some((_, node)) => node.clone(),
                None => {
                    let node = source.document(head).await?;
                    listed.insert(key, (head.clone(), node.clone()));
                    node
                }
            })
        }
        None => None,
    };

    let mut observed: BTreeSet<String> = by_snapshot.keys().cloned().collect();
    if let Some(node) = &head_node {
        observed.insert(node.snapshot_id.clone());
    }
    let first_seen = store.record_observations(request.connection_id, &observed, request.now_ms)?;
    let inside_grace = |id: &str| -> bool {
        first_seen
            .get(id)
            .is_some_and(|seen| request.now_ms.saturating_sub(*seen) < GRACE_MS)
    };

    let mut roots: Vec<RemoteObject> = Vec::new();
    roots.extend(request.roots.head.iter().cloned());
    roots.extend(request.roots.kept_points.iter().cloned());
    roots.extend(request.roots.kept_bundles.iter().cloned());
    roots.extend(request.roots.job_objects.iter().cloned());
    for id in &request.roots.job_snapshot_ids {
        if let Some(key) = by_snapshot.get(id) {
            roots.push(listed[key].0.clone());
        }
    }
    // Anything this device saw for the first time recently, including whatever
    // it has never seen before, stays protected for the grace window.
    for (id, key) in &by_snapshot {
        if inside_grace(id) {
            roots.push(listed[key].0.clone());
        }
    }
    // An ancestor is protected from the moment this device saw the state that
    // displaced it, which is not the moment that state was uploaded: a job can
    // spend longer than the grace window between its upload and its head write.
    // The walk stops at the first child whose window has closed, so a long
    // chain of old states is never followed to its beginning.
    let mut displaced = BTreeSet::new();
    if let Some(node) = head_node {
        let mut child = node;
        let mut ancestors = BTreeSet::new();
        while let Some(parent) = child.parent_snapshot_id.clone() {
            let target = displacement_of(&child.snapshot_id);
            match first_seen.get(&target) {
                Some(seen) if request.now_ms.saturating_sub(*seen) >= GRACE_MS => break,
                Some(_) => {}
                None => {
                    displaced.insert(target);
                }
            }
            if !ancestors.insert(parent.clone()) {
                break;
            }
            let key = by_snapshot.get(&parent).ok_or_else(missing)?;
            let (object, ancestor) = listed.get(key).ok_or_else(corrupt)?;
            roots.push(object.clone());
            child = ancestor.clone();
        }
    }
    if !displaced.is_empty() {
        store.record_observations(request.connection_id, &displaced, request.now_ms)?;
    }

    let mut reachable = BTreeSet::new();
    let reached = walk(roots, source, &listed, &mut reachable, cancel).await?;
    let mut reachable_bytes = 0u64;
    for object in &reached {
        reachable_bytes = reachable_bytes
            .checked_add(object.receipt.byte_length)
            .ok_or_else(corrupt)?;
    }

    // A dropped point goes before the documents it named, so an interrupted run
    // leaves fragments nothing points at rather than a point pointing at
    // nothing.
    let mut documents: Vec<CommittedDeletion> = request
        .dropped_points
        .iter()
        .map(|object| candidate_of(object, request.now_ms))
        .collect();
    let mut below = Vec::new();
    let mut visited = reachable.clone();
    for (id, key) in &by_snapshot {
        cancel.check()?;
        if reachable.contains(key) || inside_grace(id) {
            continue;
        }
        let (object, node) = &listed[key];
        let mut subtree = visited.clone();
        subtree.insert(key.clone());
        match walk(
            node.references.clone(),
            source,
            &listed,
            &mut subtree,
            cancel,
        )
        .await
        {
            Ok(reached) => {
                documents.push(candidate_of(object, request.now_ms));
                below.extend(
                    reached
                        .iter()
                        .map(|object| candidate_of(object, request.now_ms)),
                );
                visited = subtree;
            }
            // Removing a document whose fragments cannot be listed would leave
            // pieces nothing can name again.
            Err(_) => skipped += 1,
        }
    }
    documents.extend(below);

    // Once a parent document is gone nothing enumerates the fragments under it
    // again, so what an interrupted run still owes is written down rather than
    // rediscovered. A list this device can no longer read costs it those
    // fragments and the run says so.
    let (carried, list_recomputed) = match store.committed_deletions(request.connection_id) {
        Ok(rows) => (rows, false),
        Err(_) => (Vec::new(), true),
    };
    let mut committed = Vec::new();
    let mut named = BTreeSet::new();
    for entry in carried
        .into_iter()
        .filter(|entry| !entry.done)
        .chain(documents)
    {
        let key = locator_key(&entry.locator)?;
        // A target the current roots reach again leaves the list instead of
        // being removed.
        if reachable.contains(&key) || !named.insert(key) {
            continue;
        }
        committed.push(entry);
    }
    store.replace_deletions(request.connection_id, &committed)?;
    let mut keep = observed.clone();
    keep.extend(
        observed
            .iter()
            .map(|id| displacement_of(id))
            .collect::<Vec<_>>(),
    );
    store.prune_observations(request.connection_id, &keep)?;
    store.set_last_reachable_bytes(request.connection_id, reachable_bytes)?;
    Ok(Mark {
        reachable,
        reachable_bytes,
        candidates: committed,
        skipped,
        list_recomputed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::contract::{ObjectReceipt, RemoteLocator};
    use std::sync::Mutex;

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_000 * DAY;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn object(id: &str, role: ObjectRole, byte_length: u64) -> RemoteObject {
        RemoteObject {
            repository_id: "synthetic-repository".into(),
            object_id: id.into(),
            role,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: "synthetic-account/root".into(),
                    collection: match role {
                        ObjectRole::SyncState | ObjectRole::BackupBundle => Some("snapshots".into()),
                        ObjectRole::Catalog => Some("catalogs".into()),
                        _ => None,
                    },
                    object: id.into(),
                },
                byte_length,
                version: None,
                checksum: None,
                complete: true,
            },
            ciphertext_sha256: "11".repeat(32),
            plaintext_length: byte_length,
            plaintext_sha256: "22".repeat(32),
        }
    }
    fn key(object: &RemoteObject) -> String {
        locator_key(&object.receipt.locator).unwrap()
    }

    enum Entry {
        Document(DocumentNode),
        Catalog(Vec<RemoteObject>),
    }

    #[derive(Default)]
    struct Repository {
        entries: BTreeMap<String, Entry>,
        broken: BTreeSet<String>,
        reads: Mutex<Vec<String>>,
    }
    impl Repository {
        fn with_document(
            &mut self,
            object: &RemoteObject,
            parent: Option<&str>,
            references: Vec<RemoteObject>,
        ) {
            self.entries.insert(
                key(object),
                Entry::Document(DocumentNode {
                    snapshot_id: object
                        .object_id
                        .strip_prefix("snapshot-")
                        .unwrap_or(&object.object_id)
                        .to_owned(),
                    parent_snapshot_id: parent.map(str::to_owned),
                    references,
                }),
            );
        }
        fn with_catalog(&mut self, object: &RemoteObject, children: Vec<RemoteObject>) {
            self.entries.insert(key(object), Entry::Catalog(children));
        }
        fn read(&self, locator: &RemoteLocator) -> Result<&Entry> {
            let key = locator_key(locator)?;
            self.reads.lock().unwrap().push(key.clone());
            if self.broken.contains(&key) {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            self.entries.get(&key).ok_or_else(missing)
        }
        fn requested(&self, object: &RemoteObject) -> bool {
            self.reads.lock().unwrap().contains(&key(object))
        }
    }
    impl DocumentSource for Repository {
        fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
            Box::pin(async move {
                match self.read(&object.receipt.locator)? {
                    Entry::Document(node) => Ok(node.clone()),
                    Entry::Catalog(_) => Err(corrupt()),
                }
            })
        }
        fn listed<'a>(
            &'a self,
            receipt: &'a ObjectReceipt,
        ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
            Box::pin(async move {
                let node = match self.read(&receipt.locator)? {
                    Entry::Document(node) => node.clone(),
                    Entry::Catalog(_) => return Err(corrupt()),
                };
                let mut object = object(
                    &format!("snapshot-{}", node.snapshot_id),
                    ObjectRole::SyncState,
                    receipt.byte_length,
                );
                object.receipt = receipt.clone();
                Ok((object, node))
            })
        }
        fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
            Box::pin(async move {
                match self.read(&object.receipt.locator)? {
                    Entry::Catalog(children) => Ok(children.clone()),
                    Entry::Document(_) => Err(corrupt()),
                }
            })
        }
    }

    fn store() -> (tempfile::TempDir, GcStore) {
        let root = tempfile::tempdir().unwrap();
        let store = GcStore::open(root.path()).unwrap();
        (root, store)
    }
    fn receipts(objects: &[&RemoteObject]) -> Vec<ObjectReceipt> {
        objects
            .iter()
            .map(|object| object.receipt.clone())
            .collect()
    }
    fn roots(head: Option<&RemoteObject>) -> Roots {
        Roots {
            head: head.cloned(),
            kept_points: Vec::new(),
            kept_bundles: Vec::new(),
            job_objects: Vec::new(),
            job_snapshot_ids: BTreeSet::new(),
        }
    }
    fn names(mark: &Mark) -> Vec<String> {
        mark.candidates
            .iter()
            .map(|candidate| candidate.locator.object.clone())
            .collect()
    }

    /// A library, a section and one pack under each, shared by the tests that
    /// need a published state to look like a real one.
    struct Library {
        records: RemoteObject,
        assets: RemoteObject,
        section: RemoteObject,
        section_pack: RemoteObject,
        record_pack: RemoteObject,
    }
    impl Library {
        fn new(tag: &str) -> Self {
            Self {
                records: object(&format!("records-{tag}"), ObjectRole::Catalog, 10),
                assets: object(&format!("assets-{tag}"), ObjectRole::Catalog, 20),
                section: object(&format!("hypa-{tag}"), ObjectRole::Catalog, 30),
                section_pack: object(&format!("hypa-pack-{tag}"), ObjectRole::Pack, 400),
                record_pack: object(&format!("record-pack-{tag}"), ObjectRole::Pack, 500),
            }
        }
        fn install(&self, repository: &mut Repository) {
            repository.with_catalog(&self.records, vec![self.record_pack.clone()]);
            repository.with_catalog(&self.assets, Vec::new());
            repository.with_catalog(&self.section, vec![self.section_pack.clone()]);
        }
        fn references(&self) -> Vec<RemoteObject> {
            vec![
                self.records.clone(),
                self.assets.clone(),
                self.section.clone(),
            ]
        }
    }

    /// Invariant GC1. A publication that only changed the library still names
    /// the section it inherited, so that section's catalog and packs stay
    /// reachable even once the state that introduced them is removable.
    #[test]
    fn an_inherited_section_survives_a_library_only_publication() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let old_library = Library::new("old");
        let new_library = Library::new("new");
        old_library.install(&mut repository);
        repository.with_catalog(&new_library.records, vec![new_library.record_pack.clone()]);
        repository.with_catalog(&new_library.assets, Vec::new());
        let parent = object("snapshot-parent", ObjectRole::SyncState, 1);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        repository.with_document(&parent, None, old_library.references());
        repository.with_document(
            &head,
            Some("parent"),
            vec![
                new_library.records.clone(),
                new_library.assets.clone(),
                // The section reference the new state carried forward.
                old_library.section.clone(),
            ],
        );
        // Both states were seen long ago, so only the head is a root.
        store
            .record_observations(
                "connection",
                &["head".to_owned(), "parent".to_owned(), "head/head".to_owned()]
                    .into_iter()
                    .collect(),
                NOW - 60 * DAY,
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head, &parent]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(mark.skipped, 0);
        for kept in [
            &old_library.section,
            &old_library.section_pack,
            &new_library.records,
            &new_library.record_pack,
        ] {
            assert!(mark.reachable.contains(&key(kept)), "{}", kept.object_id);
        }
        assert_eq!(
            names(&mark),
            vec![
                "snapshot-parent",
                "records-old",
                "assets-old",
                "record-pack-old"
            ]
        );
    }

    /// Invariant GC3. One unreadable root ends the run: no candidate, no
    /// recorded total, and an observation ledger that still holds the target
    /// that left the enumeration.
    #[test]
    fn an_unreadable_root_leaves_the_run_with_nothing_to_remove() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("head");
        library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let stale = object("snapshot-stale", ObjectRole::SyncState, 3);
        repository.with_document(&head, None, library.references());
        repository.with_document(&stale, None, Vec::new());
        repository.broken.insert(key(&library.records));
        let carried = leftover(&object("orphan-pack", ObjectRole::Pack, 11), false);
        store
            .replace_deletions("connection", &[carried.clone()])
            .unwrap();
        store
            .record_observations(
                "connection",
                &["stale".to_owned(), "gone".to_owned()].into_iter().collect(),
                NOW - 60 * DAY,
            )
            .unwrap();
        let error = runtime().block_on(mark(
            MarkRequest {
                connection_id: "connection",
                now_ms: NOW,
                roots: roots(Some(&head)),
                listed: receipts(&[&head, &stale]),
                dropped_points: Vec::new(),
            },
            &repository,
            &store,
            &Cancellation::default(),
        ));
        assert!(error.is_err());
        assert_eq!(store.last_reachable_bytes("connection").unwrap(), None);
        // The run added nothing to the list and took nothing off it.
        assert_eq!(
            store.committed_deletions("connection").unwrap(),
            vec![carried]
        );
        let ledger = store
            .record_observations(
                "connection",
                &["gone".to_owned()].into_iter().collect(),
                NOW,
            )
            .unwrap();
        assert_eq!(ledger.get("gone"), Some(&(NOW - 60 * DAY)));
    }

    /// Invariant GC15. A head made long ago and displaced a moment ago keeps
    /// its own objects, and the walk stops at the first ancestor whose grace
    /// window closed instead of following the lineage to its beginning.
    #[test]
    fn a_head_displaced_a_moment_ago_is_not_a_candidate() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let displaced_library = Library::new("displaced");
        let older_library = Library::new("older");
        displaced_library.install(&mut repository);
        older_library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let displaced = object("snapshot-displaced", ObjectRole::SyncState, 3);
        let older = object("snapshot-older", ObjectRole::SyncState, 4);
        repository.with_document(&head, Some("displaced"), Vec::new());
        repository.with_document(&displaced, Some("older"), displaced_library.references());
        repository.with_document(&older, None, older_library.references());
        store
            .record_observations(
                "connection",
                &[
                    "displaced".to_owned(),
                    "older".to_owned(),
                    "head/displaced".to_owned(),
                ]
                .into_iter()
                .collect(),
                NOW - 40 * DAY,
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head, &displaced, &older]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(mark.reachable.contains(&key(&displaced)));
        assert!(mark.reachable.contains(&key(&displaced_library.section_pack)));
        // The walk stopped at the displaced state, so its own parent was never
        // taken as a root and is removable.
        assert!(names(&mark).contains(&"snapshot-older".to_owned()));
        assert!(!names(&mark).contains(&"snapshot-displaced".to_owned()));
    }

    /// Invariant GC22. A target with no ledger row is recorded and left alone,
    /// however old the remote document says it is.
    #[test]
    fn a_target_seen_for_the_first_time_is_only_recorded() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("head");
        library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let slow = object("snapshot-slow", ObjectRole::SyncState, 3);
        repository.with_document(&head, None, library.references());
        repository.with_document(&slow, None, Vec::new());
        let request = |now: u64| MarkRequest {
            connection_id: "connection",
            now_ms: now,
            roots: roots(Some(&head)),
            listed: receipts(&[&head, &slow]),
            dropped_points: Vec::new(),
        };
        let first = runtime()
            .block_on(mark(
                request(NOW),
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(first.candidates.is_empty());
        let later = runtime()
            .block_on(mark(
                request(NOW + 8 * DAY),
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(names(&later), vec!["snapshot-slow"]);
    }

    /// Invariant GC15, second half. An ancestor past the grace window is never
    /// requested as a root, so the lineage read stops there.
    #[test]
    fn the_lineage_read_stops_at_the_first_closed_grace_window() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let ancient = object("snapshot-ancient", ObjectRole::SyncState, 3);
        repository.with_document(&head, Some("ancient"), Vec::new());
        store
            .record_observations(
                "connection",
                &["head".to_owned(), "head/head".to_owned()]
                    .into_iter()
                    .collect(),
                NOW - 40 * DAY,
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(mark.candidates.is_empty());
        assert!(!repository.requested(&ancient));
    }

    #[test]
    fn an_unreadable_candidate_takes_only_itself_out_of_the_run() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("head");
        let stale_library = Library::new("stale");
        library.install(&mut repository);
        stale_library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let stale = object("snapshot-stale", ObjectRole::SyncState, 3);
        let broken = object("snapshot-broken", ObjectRole::SyncState, 4);
        repository.with_document(&head, None, library.references());
        repository.with_document(&stale, None, stale_library.references());
        repository.with_document(&broken, None, Vec::new());
        repository.broken.insert(key(&broken));
        store
            .record_observations(
                "connection",
                &["stale".to_owned()].into_iter().collect(),
                NOW - 40 * DAY,
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head, &stale, &broken]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(mark.skipped, 1);
        assert!(names(&mark).contains(&"snapshot-stale".to_owned()));
        assert!(!names(&mark).contains(&"snapshot-broken".to_owned()));
    }

    #[test]
    fn an_unfinished_job_keeps_its_own_fragments() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("head");
        library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let orphan = object("job-pack", ObjectRole::Pack, 700);
        repository.with_document(&head, None, library.references());
        let mut roots = roots(Some(&head));
        roots.job_objects = vec![orphan.clone()];
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots,
                    listed: receipts(&[&head]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(mark.reachable.contains(&key(&orphan)));
        assert_eq!(
            store.last_reachable_bytes("connection").unwrap(),
            Some(mark.reachable_bytes)
        );
        assert_eq!(
            mark.reachable_bytes,
            head.receipt.byte_length + 10 + 20 + 30 + 400 + 500 + 700
        );
    }

    /// Invariant GC22. A job that spends longer than the grace window between
    /// uploading its state and writing the head does not make the state it
    /// replaced removable the moment that publication lands. The remote
    /// document's own times never enter the decision.
    #[test]
    fn a_state_published_long_after_it_was_uploaded_still_protects_its_parent() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("parent");
        library.install(&mut repository);
        let parent = object("snapshot-s0", ObjectRole::SyncState, 2);
        let published = object("snapshot-s1", ObjectRole::SyncState, 3);
        repository.with_document(&parent, None, library.references());
        repository.with_document(&published, Some("s0"), Vec::new());
        let run = |now: u64, head: &RemoteObject, job: &[&str]| {
            runtime()
                .block_on(mark(
                    MarkRequest {
                        connection_id: "connection",
                        now_ms: now,
                        roots: Roots {
                            head: Some(head.clone()),
                            job_snapshot_ids: job
                                .iter()
                                .map(|id| (*id).to_owned())
                                .collect(),
                            ..roots(None)
                        },
                        listed: receipts(&[&parent, &published]),
                        dropped_points: Vec::new(),
                    },
                    &repository,
                    &store,
                    &Cancellation::default(),
                ))
                .unwrap()
        };
        // The state is uploaded and enumerated while its job waits on budget.
        assert!(run(NOW, &parent, &["s1"]).candidates.is_empty());
        // The head write lands past the window the upload opened.
        let landed = run(NOW + 8 * DAY, &published, &[]);
        assert!(landed.candidates.is_empty());
        assert!(landed.reachable.contains(&key(&library.section_pack)));
        // The parent's own window runs from the publication this device saw,
        // so it becomes removable a window after that, not before.
        let later = run(NOW + 16 * DAY, &published, &[]);
        assert!(names(&later).contains(&"snapshot-s0".to_owned()));
    }

    /// A point the retention decision dropped leads the list, ahead of the
    /// documents it named.
    #[test]
    fn a_dropped_backup_point_is_removed_before_what_it_named() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        let bundle = object("snapshot-bundle", ObjectRole::BackupBundle, 3);
        let point = object("point-old", ObjectRole::BackupPoint, 4);
        repository.with_document(&head, None, Vec::new());
        repository.with_document(&bundle, None, Vec::new());
        store
            .record_observations(
                "connection",
                &["bundle".to_owned()].into_iter().collect(),
                NOW - 40 * DAY,
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head, &bundle]),
                    dropped_points: vec![point.clone()],
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert_eq!(names(&mark), vec!["point-old", "snapshot-bundle"]);
    }

    fn leftover(object: &RemoteObject, done: bool) -> CommittedDeletion {
        CommittedDeletion {
            locator: object.receipt.locator.clone(),
            role: object.role,
            byte_length: object.receipt.byte_length,
            decided_at_ms: NOW - DAY,
            done,
        }
    }

    /// What an earlier run committed to but never finished joins this run, and
    /// what it did finish leaves the list once this mark is over. The list this
    /// run answers with is the one it stored.
    #[test]
    fn an_unfinished_target_rejoins_the_run_and_a_finished_one_leaves() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        repository.with_document(&head, None, Vec::new());
        let unfinished = object("orphan-pack", ObjectRole::Pack, 11);
        let finished = object("removed-pack", ObjectRole::Pack, 22);
        store
            .replace_deletions(
                "connection",
                &[leftover(&unfinished, false), leftover(&finished, true)],
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(!mark.list_recomputed);
        assert_eq!(names(&mark), vec!["orphan-pack"]);
        // The decision time of a carried target is the one it was given.
        assert_eq!(mark.candidates[0].decided_at_ms, NOW - DAY);
        assert_eq!(store.committed_deletions("connection").unwrap(), mark.candidates);
    }

    /// Invariant GC25. A target on the list that the current roots reach again
    /// is dropped from the list instead of being removed.
    #[test]
    fn a_target_that_is_reachable_again_leaves_the_list() {
        let (_root, store) = store();
        let mut repository = Repository::default();
        let library = Library::new("head");
        library.install(&mut repository);
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        repository.with_document(&head, None, library.references());
        store
            .replace_deletions(
                "connection",
                &[
                    leftover(&library.record_pack, false),
                    leftover(&object("orphan-pack", ObjectRole::Pack, 11), false),
                ],
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(mark.reachable.contains(&key(&library.record_pack)));
        assert_eq!(names(&mark), vec!["orphan-pack"]);
        assert_eq!(store.committed_deletions("connection").unwrap(), mark.candidates);
    }

    #[test]
    fn a_list_this_device_cannot_read_is_replaced_and_reported() {
        let (root, store) = store();
        let mut repository = Repository::default();
        let head = object("snapshot-head", ObjectRole::SyncState, 2);
        repository.with_document(&head, None, Vec::new());
        rusqlite::Connection::open(root.path().join("external-gc.sqlite"))
            .unwrap()
            .execute(
                "INSERT INTO deletions(connection_id,locator,role,byte_length,decided_at_ms,done)
                 VALUES('connection','not-a-locator','pack',1,1,0)",
                [],
            )
            .unwrap();
        let mark = runtime()
            .block_on(mark(
                MarkRequest {
                    connection_id: "connection",
                    now_ms: NOW,
                    roots: roots(Some(&head)),
                    listed: receipts(&[&head]),
                    dropped_points: Vec::new(),
                },
                &repository,
                &store,
                &Cancellation::default(),
            ))
            .unwrap();
        assert!(mark.list_recomputed);
        assert!(mark.candidates.is_empty());
        assert!(store.committed_deletions("connection").unwrap().is_empty());
    }
}

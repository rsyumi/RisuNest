//! Fresh reachability over authenticated roots and a bounded ownership universe.
//! Snapshot discovery is not permission to collect another writer's objects.
use super::{
    contract::{
        Cancellation, ErrorKind, ObjectReceipt, ObjectRole, ProviderError, ProviderFuture,
        RepositoryHandle, Result,
    },
    gc_store::{locator_key, GcStore},
    leases::UNREACHABLE_GRACE_MS,
    packaging::RemoteObject,
};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    path::Path,
};

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

/// Every published section is included, even if this device no longer selects it.
pub(crate) fn document_references(
    view: &super::control::SnapshotView,
) -> Vec<&risunest_external_storage_format::snapshot::StoredObject> {
    std::iter::once(&view.library.record_catalog)
        .chain(std::iter::once(&view.library.asset_catalog))
        .chain(view.sections.values().map(|section| &section.entries_root))
        .collect()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DocumentNode {
    pub snapshot_id: String,
    pub parent_snapshot_id: Option<String>,
    pub references: Vec<RemoteObject>,
}

pub(crate) trait DocumentSource: Sync {
    fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode>;
    fn listed<'a>(
        &'a self, receipt: &'a ObjectReceipt,
    ) -> ProviderFuture<'a, (RemoteObject, DocumentNode)>;
    fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>>;
    /// Verify current identity and bytes. Only a confirmed missing object is None.
    fn probe<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>>;
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roots {
    pub head: Option<RemoteObject>,
    pub kept_points: Vec<RemoteObject>,
    pub kept_bundles: Vec<RemoteObject>,
    pub job_objects: Vec<ObjectReceipt>,
    /// Authenticated source references also protect reused catalog children.
    pub job_references: Vec<RemoteObject>,
    pub job_snapshot_ids: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RetiredPoint {
    pub point: RemoteObject,
    pub bundles: Vec<RemoteObject>,
}

pub(crate) struct MarkRequest<'a> {
    pub connection_id: &'a str,
    pub repository: &'a RepositoryHandle,
    pub format_repository_id: &'a str,
    pub now_ms: u64,
    pub roots: Roots,
    pub listed: Vec<ObjectReceipt>,
    /// Objects recorded by this device's existing packaging/transfer inventory.
    pub known_objects: Vec<RemoteObject>,
    pub retired_points: Vec<RetiredPoint>,
}

pub(crate) struct Mark {
    pub reachable: BTreeSet<String>,
    pub reachable_bytes: u64,
    /// Parents precede children; all entries passed this run's seven-day check.
    pub candidates: Vec<RemoteObject>,
    pub deferred: usize,
}

pub(crate) fn object_identity(object: &RemoteObject, repository: &RepositoryHandle) -> Result<String> {
    let encoded = serde_json::to_vec(&object.stored(repository)?).map_err(|_| corrupt())?;
    Ok(risunest_sync_wire::hash(&encoded))
}
fn validate_object(
    object: &RemoteObject, repository: &RepositoryHandle, format_repository_id: &str,
) -> Result<()> {
    object.stored(repository)?;
    if object.repository_id != format_repository_id
        || matches!(object.role, ObjectRole::Descriptor | ObjectRole::Lease)
    {
        return Err(corrupt());
    }
    Ok(())
}

#[derive(Clone)]
struct Node {
    object: RemoteObject,
    children: BTreeSet<String>,
}
#[derive(Default)]
struct Graph {
    nodes: BTreeMap<String, Node>,
    missing: BTreeSet<String>,
    identities: BTreeMap<String, String>,
}
impl Graph {
    fn check_acyclic(&self) -> Result<()> {
        let mut incoming: BTreeMap<_, usize> = self.nodes.keys().map(|key| (key.clone(), 0)).collect();
        for node in self.nodes.values() {
            for child in &node.children {
                if let Some(count) = incoming.get_mut(child) {
                    *count = count.checked_add(1).ok_or_else(corrupt)?;
                } else if !self.missing.contains(child) {
                    return Err(corrupt());
                }
            }
        }
        let mut ready: VecDeque<_> = incoming.iter().filter(|(_, count)| **count == 0)
            .map(|(key, _)| key.clone()).collect();
        let mut visited = 0;
        while let Some(key) = ready.pop_front() {
            visited += 1;
            for child in &self.nodes[&key].children {
                if let Some(count) = incoming.get_mut(child) {
                    *count = count.checked_sub(1).ok_or_else(corrupt)?;
                    if *count == 0 { ready.push_back(child.clone()); }
                }
            }
        }
        if visited != self.nodes.len() { return Err(corrupt()); }
        Ok(())
    }
}

async fn walk(
    source: &dyn DocumentSource,
    objects: Vec<RemoteObject>,
    repository: &RepositoryHandle,
    format_repository_id: &str,
    tolerate_missing: bool,
    cancel: &Cancellation,
) -> Result<Graph> {
    let mut graph = Graph::default();
    let mut pending: VecDeque<_> = objects.into();
    while let Some(object) = pending.pop_front() {
        cancel.check()?;
        validate_object(&object, repository, format_repository_id)?;
        let key = locator_key(&object.receipt.locator)?;
        let identity = object_identity(&object, repository)?;
        if let Some(previous) = graph.identities.insert(key.clone(), identity.clone()) {
            if previous != identity { return Err(corrupt()); }
            continue;
        }
        let Some(current) = source.probe(&object).await? else {
            if !tolerate_missing { return Err(ProviderError::new(ErrorKind::NotFound)); }
            graph.missing.insert(key);
            continue;
        };
        if !current.complete || current.locator != object.receipt.locator
            || current.byte_length != object.receipt.byte_length
        {
            return Err(corrupt());
        }
        let children = match object.role {
            ObjectRole::SyncState | ObjectRole::BackupBundle => {
                source.document(&object).await.map(|node| node.references)
            }
            ObjectRole::Catalog => source.catalog(&object).await,
            ObjectRole::Pack | ObjectRole::BackupPoint => Ok(Vec::new()),
            ObjectRole::Descriptor | ObjectRole::Lease => return Err(corrupt()),
        };
        // A disappearing metadata object changes the survey even when an earlier
        // direct probe found it. Do not hide a partially walked closure.
        let children = children?;
        let keys = children.iter().map(|child| locator_key(&child.receipt.locator))
            .collect::<Result<BTreeSet<_>>>()?;
        graph.nodes.insert(key, Node { object, children: keys });
        pending.extend(children);
    }
    graph.check_acyclic()?;
    Ok(graph)
}

fn merge(target: &mut Graph, other: Graph) -> Result<()> {
    for (key, identity) in other.identities {
        if target.identities.insert(key, identity.clone()).is_some_and(|previous| previous != identity) {
            return Err(corrupt());
        }
    }
    for (key, node) in other.nodes {
        if let Some(previous) = target.nodes.get(&key) {
            if previous.children != node.children { return Err(corrupt()); }
        } else {
            target.nodes.insert(key, node);
        }
    }
    target.missing.extend(other.missing);
    if target.missing.iter().any(|key| target.nodes.contains_key(key)) { return Err(corrupt()); }
    target.check_acyclic()
}

/// The caller invalidates its observation before any remote read and confirms
/// it only after checking the roots and repository protection again.
pub(crate) async fn mark(
    root: &Path,
    source: &dyn DocumentSource,
    request: MarkRequest<'_>,
    cancel: &Cancellation,
) -> Result<Mark> {
    cancel.check()?;
    let mut by_snapshot = BTreeMap::new();
    let mut listed_locators = BTreeSet::new();
    for receipt in &request.listed {
        receipt.locator.validate_for(request.repository)?;
        if !receipt.complete || receipt.byte_length == 0
            || !listed_locators.insert(locator_key(&receipt.locator)?)
        {
            return Err(corrupt());
        }
        let (object, document) = source.listed(receipt).await?;
        validate_object(&object, request.repository, request.format_repository_id)?;
        if !matches!(object.role, ObjectRole::SyncState | ObjectRole::BackupBundle)
            || object.receipt.locator != receipt.locator || object.receipt.byte_length != receipt.byte_length
            || document.snapshot_id.is_empty()
            || by_snapshot.insert(document.snapshot_id, object).is_some()
        {
            return Err(corrupt());
        }
    }

    let mut authorized = BTreeSet::new();
    let mut known_by_locator = BTreeMap::new();
    for object in &request.known_objects {
        validate_object(object, request.repository, request.format_repository_id)?;
        let key = locator_key(&object.receipt.locator)?;
        authorized.insert(key.clone());
        if let Some(previous) = known_by_locator.insert(key, object.clone()) {
            if object_identity(&previous, request.repository)? != object_identity(object, request.repository)? {
                return Err(corrupt());
            }
        }
    }

    let mut protected = Vec::new();
    protected.extend(request.roots.head.clone());
    protected.extend(request.roots.kept_points.iter().cloned());
    protected.extend(request.roots.kept_bundles.iter().cloned());
    protected.extend(request.roots.job_references.iter().cloned());
    for id in &request.roots.job_snapshot_ids {
        if let Some(object) = by_snapshot.get(id) { protected.push(object.clone()); }
    }
    for receipt in &request.roots.job_objects {
        receipt.locator.validate_for(request.repository)?;
        if !receipt.complete || receipt.byte_length == 0 { return Err(corrupt()); }
        if let Some(object) = known_by_locator.get(&locator_key(&receipt.locator)?) {
            if object.receipt.byte_length != receipt.byte_length { return Err(corrupt()); }
            protected.push(object.clone());
        }
    }
    let live = walk(
        source, protected, request.repository, request.format_repository_id, false, cancel,
    ).await?;
    let mut reachable = live.nodes.keys().cloned().collect::<BTreeSet<_>>();
    let mut reachable_bytes = live.nodes.values().try_fold(0u64, |sum, node| {
        sum.checked_add(node.object.receipt.byte_length).ok_or_else(corrupt)
    })?;
    for object in &request.roots.job_objects {
        if reachable.insert(locator_key(&object.locator)?) {
            reachable_bytes = reachable_bytes.checked_add(object.byte_length).ok_or_else(corrupt)?;
        }
    }

    // Only retention-authorized points expand the ownership universe. Merely
    // discovering an unheaded snapshot never claims its packs for this device.
    let expired_roots = request.retired_points.iter().flat_map(|point| {
        std::iter::once(point.point.clone()).chain(point.bundles.iter().cloned())
    }).collect();
    let mut expired = walk(
        source, expired_roots, request.repository, request.format_repository_id, true, cancel,
    ).await?;
    for point in &request.retired_points {
        if point.point.role != ObjectRole::BackupPoint
            || point.bundles.iter().any(|bundle| bundle.role != ObjectRole::BackupBundle)
        {
            return Err(corrupt());
        }
        if let Some(node) = expired.nodes.get_mut(&locator_key(&point.point.receipt.locator)?) {
            node.children.extend(point.bundles.iter().map(|bundle| locator_key(&bundle.receipt.locator))
                .collect::<Result<BTreeSet<_>>>()?);
        }
    }
    authorized.extend(expired.nodes.keys().cloned());
    let mut candidates = walk(
        source, request.known_objects, request.repository, request.format_repository_id, true, cancel,
    ).await?;
    for point in &request.retired_points {
        if let Some(node) = candidates.nodes.get_mut(&locator_key(&point.point.receipt.locator)?) {
            node.children.extend(point.bundles.iter().map(|bundle| locator_key(&bundle.receipt.locator))
                .collect::<Result<BTreeSet<_>>>()?);
        }
    }
    merge(&mut candidates, expired)?;
    for (key, identity) in &live.identities {
        if candidates.identities.get(key).is_some_and(|other| other != identity)
            || candidates.missing.contains(key)
        {
            return Err(corrupt());
        }
    }
    let unreachable = authorized.into_iter().filter(|key| {
        !reachable.contains(key) && !candidates.missing.contains(key) && candidates.nodes.contains_key(key)
    }).collect::<BTreeSet<_>>();
    let identities = unreachable.iter().map(|key| (
        key.clone(), candidates.identities[key].clone(),
    )).collect();
    let store = GcStore::open(root)?;
    let observed = store.record_observations(request.connection_id, &identities, request.now_ms)?;

    // Walk dependencies through inherited catalogs without claiming ownership of
    // them. A young owned ancestor also delays an old owned indirect descendant.
    let inactive: BTreeSet<_> = candidates.nodes.keys().filter(|key| !reachable.contains(*key))
        .cloned().collect();
    let mut incoming: BTreeMap<_, usize> = inactive.iter().map(|key| (key.clone(), 0)).collect();
    for key in &inactive {
        for child in &candidates.nodes[key].children {
            if let Some(count) = incoming.get_mut(child) {
                *count = count.checked_add(1).ok_or_else(corrupt)?;
            }
        }
    }
    let mut ready: BTreeSet<_> = incoming.iter().filter(|(_, count)| **count == 0)
        .map(|(key, _)| key.clone()).collect();
    let mut blocked = BTreeSet::new();
    let mut eligible = Vec::new();
    let mut visited = 0;
    while let Some(key) = ready.pop_first() {
        cancel.check()?;
        visited += 1;
        let node = &candidates.nodes[&key];
        let old_enough = observed.get(&key).and_then(|first| first.checked_add(UNREACHABLE_GRACE_MS))
            .is_some_and(|expiry| request.now_ms >= expiry);
        let managed = unreachable.contains(&key);
        let collect = managed && old_enough && !blocked.contains(&key);
        let delays_children = blocked.contains(&key) || (managed && !old_enough);
        if collect { eligible.push(node.object.clone()); }
        for child in &node.children {
            if let Some(count) = incoming.get_mut(child) {
                if delays_children { blocked.insert(child.clone()); }
                *count = count.checked_sub(1).ok_or_else(corrupt)?;
                if *count == 0 { ready.insert(child.clone()); }
            }
        }
    }
    if visited != inactive.len() { return Err(corrupt()); }
    Ok(Mark {
        reachable, reachable_bytes,
        deferred: unreachable.len().saturating_sub(eligible.len()), candidates: eligible,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::external_storage::{contract::RemoteLocator, fake};
    use risunest_external_storage_format::snapshot as wire;
    use std::sync::Mutex;

    pub(crate) fn object(id: &str, role: ObjectRole) -> RemoteObject {
        let header = wire::PublicObjectHeader::new(
            "format-repository".into(), id.into(), super::super::packaging::wire_role(role).unwrap(), 1,
        ).unwrap();
        RemoteObject {
            repository_id: "format-repository".into(), object_id: id.into(), role,
            receipt: ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: fake::repository().connection_identity,
                    collection: None, object: id.into(),
                },
                byte_length: wire::envelope_length(&header).unwrap(),
                version: None, checksum: None, complete: true,
            },
            ciphertext_sha256: risunest_sync_wire::hash(id.as_bytes()), plaintext_length: 1,
            plaintext_sha256: risunest_sync_wire::hash(id.as_bytes()),
        }
    }
    pub(crate) struct Source {
        pub(crate) objects: BTreeMap<String, RemoteObject>,
        pub(crate) children: BTreeMap<String, Vec<RemoteObject>>,
        pub(crate) fail: Mutex<Option<ErrorKind>>,
        pub(crate) absent: Mutex<BTreeSet<String>>,
    }
    impl Source {
        fn node(&self, object: &RemoteObject) -> Result<DocumentNode> {
            if !self.objects.contains_key(&object.object_id) {
                return Err(ProviderError::new(ErrorKind::NotFound));
            }
            Ok(DocumentNode {
                snapshot_id: object.object_id.clone(), parent_snapshot_id: None,
                references: self.children.get(&object.object_id).cloned().unwrap_or_default(),
            })
        }
    }
    impl DocumentSource for Source {
        fn document<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, DocumentNode> {
            Box::pin(async move { self.node(object) })
        }
        fn listed<'a>(&'a self, receipt: &'a ObjectReceipt) -> ProviderFuture<'a, (RemoteObject, DocumentNode)> {
            Box::pin(async move {
                let object = self.objects.get(&receipt.locator.object)
                    .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
                Ok((object.clone(), self.node(object)?))
            })
        }
        fn catalog<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Vec<RemoteObject>> {
            Box::pin(async move { Ok(self.node(object)?.references) })
        }
        fn probe<'a>(&'a self, object: &'a RemoteObject) -> ProviderFuture<'a, Option<ObjectReceipt>> {
            Box::pin(async move {
                if let Some(kind) = self.fail.lock().unwrap().take() { return Err(ProviderError::new(kind)); }
                if self.absent.lock().unwrap().contains(&object.object_id) { return Ok(None); }
                Ok(self.objects.get(&object.object_id).map(|object| object.receipt.clone()))
            })
        }
    }
    pub(crate) fn source(objects: &[RemoteObject], edges: &[(&RemoteObject, Vec<RemoteObject>)]) -> Source {
        Source {
            objects: objects.iter().map(|object| (object.object_id.clone(), object.clone())).collect(),
            children: edges.iter().map(|(object, children)| (object.object_id.clone(), children.clone())).collect(),
            fail: Mutex::new(None), absent: Mutex::new(BTreeSet::new()),
        }
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    async fn observe(
        root: &Path, source: &Source, now: u64, roots: Roots,
        known: Vec<RemoteObject>, retired: Vec<RetiredPoint>,
    ) -> Result<Mark> {
        let store = GcStore::open(root)?;
        store.begin_observation("connection")?;
        let listed = source.objects.values()
            .filter(|object| matches!(object.role, ObjectRole::SyncState | ObjectRole::BackupBundle))
            .map(|object| object.receipt.clone()).collect();
        let marked = mark(root, source, MarkRequest {
            connection_id: "connection", repository: &fake::repository(), format_repository_id: "format-repository",
            now_ms: now, roots, listed, known_objects: known, retired_points: retired,
        }, &Cancellation::default()).await?;
        store.finish_observation("connection")?;
        Ok(marked)
    }

    #[test]
    fn c_shared_pack_and_unselected_section_stay_live_as_a_whole() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let shared = object("shared-pack", ObjectRole::Pack);
            let dead_pack = object("dead-pack", ObjectRole::Pack);
            let section = object("unselected-section", ObjectRole::Catalog);
            let head = object("head-state", ObjectRole::SyncState);
            let retired = object("old-state", ObjectRole::BackupBundle);
            let point = object("expired-point", ObjectRole::BackupPoint);
            let all = vec![shared.clone(), dead_pack.clone(), section.clone(), head.clone(), retired.clone(), point.clone()];
            let source = source(&all, &[
                (&head, vec![section.clone()]), (&section, vec![shared.clone()]),
                (&retired, vec![shared.clone(), dead_pack.clone()]),
            ]);
            let roots = Roots { head: Some(head), ..Roots::default() };
            let expired = vec![RetiredPoint { point: point.clone(), bundles: vec![retired.clone()] }];
            assert!(observe(root.path(), &source, 1000, roots.clone(), all.clone(), expired.clone()).await.unwrap().candidates.is_empty());
            let marked = observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, roots, all, expired).await.unwrap();
            let ids: Vec<_> = marked.candidates.iter().map(|object| object.object_id.as_str()).collect();
            assert!(!ids.contains(&shared.object_id.as_str()));
            assert!(!ids.contains(&section.object_id.as_str()));
            assert!(ids.iter().position(|id| *id == point.object_id).unwrap() < ids.iter().position(|id| *id == retired.object_id).unwrap());
            assert!(ids.iter().position(|id| *id == retired.object_id).unwrap() < ids.iter().position(|id| *id == dead_pack.object_id).unwrap());
        });
    }

    #[test]
    fn c_discovered_foreign_snapshots_do_not_expand_collection_ownership() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let foreign = object("foreign-state", ObjectRole::SyncState);
            let pack = object("foreign-pack", ObjectRole::Pack);
            let source = source(&[foreign.clone(), pack.clone()], &[(&foreign, vec![pack])]);
            observe(root.path(), &source, 1000, Roots::default(), vec![], vec![]).await.unwrap();
            assert!(observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, Roots::default(), vec![], vec![]).await.unwrap().candidates.is_empty());
        });
    }

    #[test]
    fn c_job_catalog_protects_reused_children_and_reachability_resets_age() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let catalog = object("job-catalog", ObjectRole::Catalog);
            let pack = object("old-pack", ObjectRole::Pack);
            let known = vec![catalog.clone(), pack.clone()];
            let source = source(&known, &[(&catalog, vec![pack.clone()])]);
            observe(root.path(), &source, 1000, Roots::default(), known.clone(), vec![]).await.unwrap();
            let roots = Roots { job_objects: vec![catalog.receipt.clone()], ..Roots::default() };
            let live = observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, roots, known.clone(), vec![]).await.unwrap();
            assert!(live.candidates.is_empty());
            assert!(live.reachable.contains(&locator_key(&pack.receipt.locator).unwrap()));
            assert!(observe(root.path(), &source, 1001 + UNREACHABLE_GRACE_MS, Roots::default(), known, vec![]).await.unwrap().candidates.is_empty());
        });
    }

    #[test]
    fn c_pinned_conflict_and_explicit_capture_roots_keep_shared_payloads() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let pack = object("shared", ObjectRole::Pack);
            let local = object("local-bundle", ObjectRole::BackupBundle);
            let remote = object("remote-bundle", ObjectRole::BackupBundle);
            let point = object("conflict-point", ObjectRole::BackupPoint);
            let all = vec![pack.clone(), local.clone(), remote.clone(), point.clone()];
            let source = source(&all, &[(&local, vec![pack.clone()]), (&remote, vec![pack.clone()])]);
            let roots = Roots {
                kept_points: vec![point], kept_bundles: vec![remote], job_references: vec![local],
                ..Roots::default()
            };
            observe(root.path(), &source, 1000, roots.clone(), all.clone(), vec![]).await.unwrap();
            let marked = observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, roots, all, vec![]).await.unwrap();
            assert!(marked.candidates.is_empty());
            assert_eq!(marked.reachable.len(), 4);
        });
    }

    #[test]
    fn c_incomplete_or_cyclic_graphs_cannot_extend_an_unreachable_interval() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let catalog = object("catalog", ObjectRole::Catalog);
            let source = source(&[catalog.clone()], &[]);
            observe(root.path(), &source, 1000, Roots::default(), vec![catalog.clone()], vec![]).await.unwrap();
            *source.fail.lock().unwrap() = Some(ErrorKind::Transient);
            assert!(observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, Roots::default(), vec![catalog.clone()], vec![]).await.is_err());
            assert!(observe(root.path(), &source, 1001 + UNREACHABLE_GRACE_MS, Roots::default(), vec![catalog.clone()], vec![]).await.unwrap().candidates.is_empty());
            let cyclic = self::source(&[catalog.clone()], &[(&catalog, vec![catalog.clone()])]);
            assert!(matches!(observe(root.path(), &cyclic, 2000 + UNREACHABLE_GRACE_MS, Roots::default(), vec![catalog], vec![]).await, Err(error) if error.kind == ErrorKind::Corrupt));
        });
    }

    #[test]
    fn c_a_younger_parent_defers_an_old_shared_child() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let pack = object("old-pack", ObjectRole::Pack);
            let parent = object("new-catalog", ObjectRole::Catalog);
            let source = source(&[pack.clone(), parent.clone()], &[(&parent, vec![pack.clone()])]);
            observe(root.path(), &source, 1000, Roots::default(), vec![pack.clone()], vec![]).await.unwrap();
            let marked = observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, Roots::default(), vec![pack, parent], vec![]).await.unwrap();
            assert!(marked.candidates.is_empty());
        });
    }

    #[test]
    fn c_parent_age_propagates_through_inherited_catalogs_without_claiming_them() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let pack = object("owned-pack", ObjectRole::Pack);
            let inherited = object("inherited-catalog", ObjectRole::Catalog);
            let parent = object("owned-parent", ObjectRole::Catalog);
            let source = source(&[pack.clone(), inherited.clone(), parent.clone()], &[
                (&parent, vec![inherited.clone()]), (&inherited, vec![pack.clone()]),
            ]);
            observe(root.path(), &source, 1000, Roots::default(), vec![pack.clone()], vec![]).await.unwrap();
            let marked = observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS,
                Roots::default(), vec![pack.clone(), parent.clone()], vec![]).await.unwrap();
            assert!(marked.candidates.is_empty());
            let marked = observe(root.path(), &source, 1000 + 2 * UNREACHABLE_GRACE_MS,
                Roots::default(), vec![pack, parent], vec![]).await.unwrap();
            assert_eq!(marked.candidates.iter().map(|object| object.object_id.as_str()).collect::<Vec<_>>(),
                ["owned-parent", "owned-pack"]);
            assert!(!marked.candidates.iter().any(|object| object.object_id == inherited.object_id));
        });
    }

    #[test]
    fn c_missing_and_recreated_objects_receive_a_new_unreachable_age() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let pack = object("pack", ObjectRole::Pack);
            let source = source(&[pack.clone()], &[]);
            observe(root.path(), &source, 1000, Roots::default(), vec![pack.clone()], vec![]).await.unwrap();
            source.absent.lock().unwrap().insert(pack.object_id.clone());
            assert!(observe(root.path(), &source, 1000 + UNREACHABLE_GRACE_MS, Roots::default(), vec![pack.clone()], vec![]).await.unwrap().candidates.is_empty());
            source.absent.lock().unwrap().clear();
            assert!(observe(root.path(), &source, 1001 + UNREACHABLE_GRACE_MS, Roots::default(), vec![pack], vec![]).await.unwrap().candidates.is_empty());
        });
    }

    #[test]
    fn c_unreadable_live_roots_and_wrong_scope_or_identity_never_produce_candidates() {
        runtime().block_on(async {
            let root = tempfile::tempdir().unwrap();
            let pack = object("pack", ObjectRole::Pack);
            let source = source(&[pack.clone()], &[]);
            let roots = Roots { job_references: vec![pack.clone()], ..Roots::default() };
            source.absent.lock().unwrap().insert(pack.object_id.clone());
            assert!(matches!(observe(root.path(), &source, 1000, roots, vec![pack.clone()], vec![]).await, Err(error) if error.kind == ErrorKind::NotFound));
            source.absent.lock().unwrap().clear();
            let mut wrong = pack.clone();
            wrong.repository_id = "other-format-repository".into();
            assert!(observe(root.path(), &source, 1000, Roots::default(), vec![wrong], vec![]).await.is_err());
            let mut wrong = pack.clone();
            wrong.receipt.locator.connection_identity = "other-account".into();
            assert!(observe(root.path(), &source, 1000, Roots::default(), vec![wrong], vec![]).await.is_err());
            let mut changed = pack.clone();
            changed.ciphertext_sha256 = "ab".repeat(32);
            assert!(observe(root.path(), &source, 1000, Roots::default(), vec![pack, changed], vec![]).await.is_err());
        });
    }
}

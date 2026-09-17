//! What a device announces in the repository while it is using it, and what it
//! looks for before it starts. A work lease says that another device's objects
//! are still in use; a delete marker says that a removal attempt is running.
//! Nothing here expires: age is never evidence that a request ended.
// Delete markers are placed by the removal path, which is wired next.
use super::{
    contract::{
        lease_object_id, parse_lease_object_id, Cancellation, Collection, ErrorKind, LeaseKind,
        ObjectIntent, ObjectRole, Provider, ProviderError, RemoteLocator, RepositoryHandle, Result,
        UploadResolution,
    },
    gc_store::{locator_key, GcStore, LeaseIntent, LeaseState},
    transfer::SpoolSource,
};
use risunest_external_storage_format::{
    content_identity::hash,
    control as wire_control,
    crypto::derive_key,
    format::Descriptor,
    snapshot::{seal_envelope, ObjectRole as EnvelopeRole, PublicObjectHeader},
};
use std::{
    collections::BTreeSet,
    io::Write,
    path::{Path, PathBuf},
};

const MAX_LEASE_PLAINTEXT: usize = 4 * 1024;
const MAX_LEASE_CIPHERTEXT: u64 = 8 * 1024;
const LEASE_PAGE: u16 = 100;

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient() -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}
fn local(_: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn staging_path(root: &Path) -> Result<TemporaryFile> {
    let directory = root.join("lease-staging");
    std::fs::create_dir_all(&directory).map_err(local)?;
    if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(&directory).map_err(local)?) {
        return Err(corrupt());
    }
    Ok(TemporaryFile(
        directory.join(format!("{}.tmp", uuid::Uuid::new_v4())),
    ))
}

fn wire_kind(kind: LeaseKind) -> wire_control::LeaseKind {
    match kind {
        LeaseKind::Work => wire_control::LeaseKind::Work,
        LeaseKind::Cleanup => wire_control::LeaseKind::Cleanup,
        LeaseKind::Deleting => wire_control::LeaseKind::Deleting,
    }
}

/// The last path segment of a lease locator. Adapters that keep a role folder
/// answer with the folder in front of the name; the name itself is what carries
/// the kind. Which device placed it is never read from here.
fn lease_name(locator: &RemoteLocator) -> &str {
    locator
        .object
        .rsplit('/')
        .next()
        .unwrap_or(&locator.object)
}

/// Everything one call needs to reach the repository and this device's record
/// of what it already placed there.
pub(crate) struct LeaseContext<'a> {
    pub root: &'a Path,
    pub connection_id: &'a str,
    pub writer_id: &'a str,
    pub descriptor: &'a Descriptor,
    pub root_key: &'a [u8; 32],
    pub provider: &'a dyn Provider,
    pub repository: &'a RepositoryHandle,
}

impl LeaseContext<'_> {
    /// A bookkeeping handle for one statement. It is opened per call because a
    /// borrowed database connection cannot cross an await point, and every
    /// step here sits between two remote requests.
    fn store(&self) -> Result<GcStore> {
        GcStore::open(self.root)
    }
}

/// A lease this device confirmed. `tag` is also the identity of the removal
/// attempt a delete marker stands for, which is what ties an outstanding
/// request to the marker that has to outlive it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseHandle {
    pub job_id: String,
    pub kind: LeaseKind,
    pub seq: u64,
    pub tag: String,
    pub locator: RemoteLocator,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedLease {
    pub locator: RemoteLocator,
    pub kind: LeaseKind,
    /// True when this device holds the intent row that placed it. Everything
    /// else belongs to another device, whatever its name reads like.
    pub mine: bool,
}

/// One full enumeration of the lease collection. A partial view is never one of
/// these: a page that could not be read ends the call with an error.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LeaseSurvey {
    pub leases: Vec<ObservedLease>,
}

impl LeaseSurvey {
    /// The delete markers that make this device wait. `own` is the marker the
    /// caller placed for the attempt it is running now; every other marker
    /// counts, including one an interrupted run of this device left behind.
    pub(crate) fn blocking_markers(&self, own: Option<&RemoteLocator>) -> Vec<&ObservedLease> {
        self.leases
            .iter()
            .filter(|lease| {
                lease.kind == LeaseKind::Deleting && own != Some(&lease.locator)
            })
            .collect()
    }
    /// The work and cleanup leases of other devices. A cleanup yields to any of
    /// them rather than removing something they may still be reading.
    pub(crate) fn foreign_work(&self) -> Vec<&ObservedLease> {
        self.leases
            .iter()
            .filter(|lease| !lease.mine && lease.kind != LeaseKind::Deleting)
            .collect()
    }
}

fn seal(ctx: &LeaseContext<'_>, object_id: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    ctx.descriptor.validate().map_err(|_| corrupt())?;
    let header = PublicObjectHeader::new(
        ctx.descriptor.repository_id.clone(),
        object_id.into(),
        EnvelopeRole::Lease,
        plaintext.len() as u64,
    )
    .map_err(|_| corrupt())?;
    let key = derive_key(ctx.root_key, &ctx.descriptor.repository_id, "metadata")
        .map_err(|_| corrupt())?;
    let mut sealed = Vec::new();
    seal_envelope(
        &mut std::io::Cursor::new(plaintext),
        &mut sealed,
        &key,
        &header,
    )
    .map_err(|_| corrupt())?;
    if sealed.len() as u64 > MAX_LEASE_CIPHERTEXT {
        return Err(ProviderError::new(ErrorKind::FileTooLarge));
    }
    Ok(sealed)
}

/// The request identity of one intent row. Nothing is built again here: the row
/// is the only source, so a retry after a lost answer writes the same name with
/// the same content.
fn request_for(ctx: &LeaseContext<'_>, row: &LeaseIntent) -> Result<ObjectIntent> {
    let intent = ObjectIntent {
        repository_id: ctx.repository.repository_id.clone(),
        job_id: row.job_id.clone(),
        object_id: lease_name(&row.locator).to_owned(),
        role: ObjectRole::Lease,
        byte_length: row.bytes.len() as u64,
        sha256: hex::encode(hash(&row.bytes)),
    };
    intent.validate(ctx.repository)?;
    Ok(intent)
}

async fn create(
    ctx: &LeaseContext<'_>,
    row: &LeaseIntent,
    intent: &ObjectIntent,
    cancel: &Cancellation,
) -> Result<RemoteLocator> {
    let temporary = staging_path(ctx.root)?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary.0)
        .map_err(local)?;
    file.write_all(&row.bytes).map_err(local)?;
    file.sync_all().map_err(local)?;
    drop(file);
    let source = SpoolSource::verified(&temporary.0, intent.byte_length, &intent.sha256)?;
    let resume = ctx.provider.begin_upload(ctx.repository, intent, cancel).await?;
    let receipt = match ctx
        .provider
        .create_object(ctx.repository, intent, &source, resume.as_ref(), cancel)
        .await
    {
        Ok(receipt) => receipt,
        Err(error) if error.kind == ErrorKind::Transient => {
            match ctx
                .provider
                .reconcile_upload(ctx.repository, intent, resume.as_ref(), cancel)
                .await?
            {
                UploadResolution::Complete(receipt) => receipt,
                // The name is already taken by different bytes, which only a
                // tag collision could produce.
                UploadResolution::Conflict => return Err(corrupt()),
                UploadResolution::Resumable(_) | UploadResolution::RestartRequired => {
                    return Err(error)
                }
            }
        }
        Err(error) => return Err(error),
    };
    accept(ctx, intent, receipt)
}

fn accept(
    ctx: &LeaseContext<'_>,
    intent: &ObjectIntent,
    receipt: super::contract::ObjectReceipt,
) -> Result<RemoteLocator> {
    if !receipt.complete || receipt.byte_length != intent.byte_length {
        return Err(corrupt());
    }
    receipt.locator.validate_for(ctx.repository)?;
    if lease_name(&receipt.locator) != intent.object_id {
        return Err(corrupt());
    }
    Ok(receipt.locator)
}

/// Brings one row to the point where its remote object is known to exist and
/// its locator is the one the repository answers with. A row that was written
/// before an answer arrived is resolved here, never guessed at.
async fn settle(
    ctx: &LeaseContext<'_>,
    row: &LeaseIntent,
    cancel: &Cancellation,
) -> Result<RemoteLocator> {
    if row.state != LeaseState::Pending {
        return Ok(row.locator.clone());
    }
    let intent = request_for(ctx, row)?;
    let confirmed = match ctx
        .provider
        .reconcile_upload(ctx.repository, &intent, None, cancel)
        .await?
    {
        UploadResolution::Complete(receipt) => accept(ctx, &intent, receipt)?,
        UploadResolution::RestartRequired => create(ctx, row, &intent, cancel).await?,
        UploadResolution::Conflict => return Err(corrupt()),
        UploadResolution::Resumable(_) => return Err(transient()),
    };
    ctx.store()?
        .confirm_lease_intent(ctx.connection_id, &row.locator, &confirmed)?;
    Ok(confirmed)
}

/// Places one lease. The intent is written first, so an answer this device
/// never sees still leaves a named object it can find and remove.
async fn place(
    ctx: &LeaseContext<'_>,
    job_id: &str,
    kind: LeaseKind,
    seq: u64,
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<LeaseHandle> {
    let tag = uuid::Uuid::new_v4().simple().to_string();
    let object_id = lease_object_id(kind, &tag)?;
    let document = wire_control::LeaseDocument::new(
        ctx.writer_id.to_owned(),
        job_id.to_owned(),
        wire_kind(kind),
        seq,
        now_ms,
    )
    .map_err(|_| corrupt())?;
    let plaintext = document
        .encode(MAX_LEASE_PLAINTEXT)
        .map_err(|_| corrupt())?;
    let row = LeaseIntent {
        locator: RemoteLocator {
            connection_identity: ctx.repository.connection_identity.clone(),
            collection: None,
            object: object_id.clone(),
        },
        kind,
        job_id: job_id.to_owned(),
        seq,
        bytes: seal(ctx, &object_id, &plaintext)?,
        state: LeaseState::Pending,
        created_at_ms: now_ms,
    };
    ctx.store()?.put_lease_intent(ctx.connection_id, &row)?;
    let intent = request_for(ctx, &row)?;
    let confirmed = create(ctx, &row, &intent, cancel).await?;
    ctx.store()?
        .confirm_lease_intent(ctx.connection_id, &row.locator, &confirmed)?;
    Ok(LeaseHandle {
        job_id: job_id.to_owned(),
        kind,
        seq,
        tag,
        locator: confirmed,
    })
}

/// Gives one lease back. The row is marked first, so an answer this device
/// never sees leaves a target it retries rather than an object it forgot.
async fn drop_lease(
    ctx: &LeaseContext<'_>,
    row: &LeaseIntent,
    cancel: &Cancellation,
) -> Result<()> {
    let locator = settle(ctx, row, cancel).await?;
    ctx.store()?
        .set_lease_state(ctx.connection_id, &locator, LeaseState::Releasing)?;
    ctx.provider
        .delete_object(ctx.repository, &locator, cancel)
        .await?;
    ctx.store()?.remove_lease_intent(ctx.connection_id, &locator)
}

/// Confirms this job's lease of one kind, renewing an existing one. The next
/// number is confirmed before the previous one is given back, so the repository
/// never shows this job without a lease.
pub(crate) async fn register(
    ctx: &LeaseContext<'_>,
    job_id: &str,
    kind: LeaseKind,
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<LeaseHandle> {
    let previous = ctx
        .store()?
        .lease_intents(ctx.connection_id)?
        .into_iter()
        .filter(|row| row.job_id == job_id && row.kind == kind)
        .max_by_key(|row| row.seq);
    let seq = previous.as_ref().map_or(0, |row| row.seq + 1);
    let handle = place(ctx, job_id, kind, seq, now_ms, cancel).await?;
    if let Some(previous) = previous {
        drop_lease(ctx, &previous, cancel).await?;
    }
    Ok(handle)
}

/// Reads the whole lease collection. A page this device cannot read or a name
/// it cannot classify ends the call, because a partial view cannot show that
/// no one is removing anything.
pub(crate) async fn survey(ctx: &LeaseContext<'_>, cancel: &Cancellation) -> Result<LeaseSurvey> {
    let mine = ctx
        .store()?
        .lease_intents(ctx.connection_id)?
        .iter()
        .map(|row| locator_key(&row.locator))
        .collect::<Result<BTreeSet<String>>>()?;
    let mut leases = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        cancel.check()?;
        let page = ctx
            .provider
            .list_objects(
                ctx.repository,
                Collection::Leases,
                cursor.as_deref(),
                LEASE_PAGE,
                cancel,
            )
            .await?;
        for receipt in page.objects {
            receipt.locator.validate_for(ctx.repository)?;
            let (kind, _) = parse_lease_object_id(lease_name(&receipt.locator))?;
            let mine = mine.contains(&locator_key(&receipt.locator)?);
            leases.push(ObservedLease {
                locator: receipt.locator,
                kind,
                mine,
            });
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(LeaseSurvey { leases })
}

/// The same reading for a removal, which needs more than "this page is what
/// the repository answered". No provider documents that a cursor walk keeps
/// every object visible across pages, so a collection that does not fit one
/// page answers `None` and the removal defers instead of trusting the walk.
/// One lease per device and job means the second page is the rare case.
pub(crate) async fn survey_single_page(
    ctx: &LeaseContext<'_>,
    cancel: &Cancellation,
) -> Result<Option<LeaseSurvey>> {
    let mine = ctx
        .store()?
        .lease_intents(ctx.connection_id)?
        .iter()
        .map(|row| locator_key(&row.locator))
        .collect::<Result<BTreeSet<String>>>()?;
    cancel.check()?;
    let page = ctx
        .provider
        .list_objects(ctx.repository, Collection::Leases, None, LEASE_PAGE, cancel)
        .await?;
    if page.next_cursor.is_some() {
        return Ok(None);
    }
    let mut leases = Vec::new();
    for receipt in page.objects {
        receipt.locator.validate_for(ctx.repository)?;
        let (kind, _) = parse_lease_object_id(lease_name(&receipt.locator))?;
        let mine = mine.contains(&locator_key(&receipt.locator)?);
        leases.push(ObservedLease {
            locator: receipt.locator,
            kind,
            mine,
        });
    }
    Ok(Some(LeaseSurvey { leases }))
}

/// What a publisher or a reader does before it asks for any data: confirm its
/// own lease, then read the whole collection and stop while anyone is removing.
/// The lease stays in place while this device waits.
pub(crate) async fn admit(
    ctx: &LeaseContext<'_>,
    job_id: &str,
    kind: LeaseKind,
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<LeaseHandle> {
    let handle = register(ctx, job_id, kind, now_ms, cancel).await?;
    if !survey(ctx, cancel).await?.blocking_markers(None).is_empty() {
        return Err(transient());
    }
    Ok(handle)
}

/// Gives back the work leases of a job whose remote requests have ended. The
/// caller decides that; this never reads a clock to decide it. A fresh
/// cancellation is used on purpose, because a cancelled job still has to be
/// able to hand its lease back.
pub(crate) async fn release(ctx: &LeaseContext<'_>, job_id: &str) -> Result<()> {
    let cancel = Cancellation::default();
    for row in ctx.store()?.lease_intents(ctx.connection_id)? {
        if row.job_id == job_id && row.kind != LeaseKind::Deleting {
            drop_lease(ctx, &row, &cancel).await?;
        }
    }
    Ok(())
}

/// Announces one removal attempt. The tag of the marker is the identity of the
/// attempt, so every request it sends is tied to the marker that outlives it.
pub(crate) async fn place_marker(
    ctx: &LeaseContext<'_>,
    job_id: &str,
    now_ms: u64,
    cancel: &Cancellation,
) -> Result<LeaseHandle> {
    place(ctx, job_id, LeaseKind::Deleting, 0, now_ms, cancel).await
}

/// Records that a removal request is about to be sent. The row exists before
/// the request does, which is what makes a lost answer detectable.
pub(crate) fn note_delete_sent(
    ctx: &LeaseContext<'_>,
    marker: &LeaseHandle,
    target: &RemoteLocator,
    now_ms: u64,
) -> Result<()> {
    ctx.store()?
        .record_delete_request(ctx.connection_id, target, &marker.tag, now_ms)
}

/// Records that the repository answered for one request. Only an answer does
/// this; a local timeout, a cancellation or a restart does not.
pub(crate) fn note_delete_finished(
    ctx: &LeaseContext<'_>,
    marker: &LeaseHandle,
    target: &RemoteLocator,
) -> Result<()> {
    ctx.store()?
        .finish_delete_request(ctx.connection_id, target, &marker.tag)
}

/// Removes a marker once every request of its attempt is known to have ended.
/// A request whose end is unknown keeps the marker, and the repository stays
/// closed to other work until it is known.
pub(crate) async fn clear_marker(ctx: &LeaseContext<'_>, marker: &LeaseHandle) -> Result<()> {
    if !ctx
        .store()?
        .unfinished_delete_requests(ctx.connection_id, Some(&marker.tag))?
        .is_empty()
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    let cancel = Cancellation::default();
    for row in ctx.store()?.lease_intents(ctx.connection_id)? {
        if row.kind == LeaseKind::Deleting && row.locator == marker.locator {
            drop_lease(ctx, &row, &cancel).await?;
        }
    }
    ctx.store()?
        .forget_finished_delete_requests(ctx.connection_id, &marker.tag)
}

/// Resolves what an interrupted run left behind, before this device places
/// anything new. `live_jobs` names the jobs that have not reached an end; a
/// lease of any other job is given back, and one whose removal attempt still
/// has an unanswered request is not.
pub(crate) async fn resume(
    ctx: &LeaseContext<'_>,
    live_jobs: &BTreeSet<String>,
    cancel: &Cancellation,
) -> Result<()> {
    for row in ctx.store()?.lease_intents(ctx.connection_id)? {
        match row.state {
            LeaseState::Pending => {
                settle(ctx, &row, cancel).await?;
            }
            LeaseState::Releasing => drop_lease(ctx, &row, cancel).await?,
            LeaseState::Confirmed => {}
        }
    }
    for row in ctx.store()?.lease_intents(ctx.connection_id)? {
        if live_jobs.contains(&row.job_id) {
            continue;
        }
        if row.kind == LeaseKind::Deleting {
            let (_, tag) = parse_lease_object_id(lease_name(&row.locator))?;
            if !ctx
                .store()?
                .unfinished_delete_requests(ctx.connection_id, Some(&tag))?
                .is_empty()
            {
                continue;
            }
            drop_lease(ctx, &row, cancel).await?;
            ctx.store()?
                .forget_finished_delete_requests(ctx.connection_id, &tag)?;
        } else {
            drop_lease(ctx, &row, cancel).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::fake::{self, DeleteFault, FakeProvider};
    use risunest_external_storage_format::format::Strategy;

    const DAY: u64 = 24 * 60 * 60 * 1000;
    const NOW: u64 = 1_000 * DAY;
    const CONNECTION: &str = "connection";

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn tag(index: u8) -> String {
        format!("{index:x}").repeat(32)
    }

    struct Harness {
        _directory: tempfile::TempDir,
        root: PathBuf,
        store: GcStore,
        provider: FakeProvider,
        repository: RepositoryHandle,
        descriptor: Descriptor,
        root_key: [u8; 32],
    }

    impl Harness {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            Self {
                store: GcStore::open(&root).unwrap(),
                _directory: directory,
                root,
                provider: FakeProvider::new(true),
                repository: fake::repository(),
                descriptor: Descriptor::new("synthetic-descriptor".into(), Some(Strategy::Cas))
                    .unwrap(),
                root_key: [7; 32],
            }
        }
        fn context(&self) -> LeaseContext<'_> {
            LeaseContext {
                root: &self.root,
                connection_id: CONNECTION,
                writer_id: "writer",
                descriptor: &self.descriptor,
                root_key: &self.root_key,
                provider: &self.provider,
                repository: &self.repository,
            }
        }
        /// A lease another device left behind. Only its name is readable from
        /// here, which is all a survey uses.
        fn foreign(&self, kind: LeaseKind, tag: &str) -> String {
            let object = lease_object_id(kind, tag).unwrap();
            self.provider
                .seed(&object, ObjectRole::Lease, b"foreign".to_vec());
            object
        }
        fn rows(&self) -> Vec<LeaseIntent> {
            self.store.lease_intents(CONNECTION).unwrap()
        }
        fn target(&self, object: &str) -> RemoteLocator {
            RemoteLocator {
                connection_identity: self.repository.connection_identity.clone(),
                collection: None,
                object: object.into(),
            }
        }
        fn stored_bytes(&self, object: &str) -> Vec<u8> {
            self.provider
                .state
                .lock()
                .unwrap()
                .objects
                .get(object)
                .unwrap()
                .0
                .clone()
        }
        async fn remove(&self, object: &str) {
            self.provider
                .delete_object(&self.repository, &self.target(object), &Cancellation::default())
                .await
                .unwrap();
        }
    }

    /// GC27: the next number is confirmed before the previous one is given
    /// back, so a renewal never shows this job without a lease.
    #[test]
    fn a_renewal_confirms_the_next_lease_before_it_removes_the_previous_one() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let first = register(&context, "job", LeaseKind::Work, NOW, &cancel)
                .await
                .unwrap();
            assert_eq!(first.seq, 0);
            assert!(harness.provider.holds(&first.locator.object));

            harness
                .provider
                .fail_delete(&first.locator.object, DeleteFault::Transient);
            let failed = register(&context, "job", LeaseKind::Work, NOW + 1, &cancel)
                .await
                .unwrap_err();
            assert_eq!(failed.kind, ErrorKind::Transient);
            let rows = harness.rows();
            assert_eq!(rows.len(), 2);
            let successor = rows.iter().find(|row| row.seq == 1).unwrap();
            assert_eq!(successor.state, LeaseState::Confirmed);
            assert!(harness.provider.holds(&successor.locator.object));
            assert_eq!(
                rows.iter().find(|row| row.seq == 0).unwrap().state,
                LeaseState::Releasing
            );
            assert!(harness.provider.holds(&first.locator.object));

            let live = BTreeSet::from(["job".to_owned()]);
            resume(&context, &live, &cancel).await.unwrap();
            assert!(!harness.provider.holds(&first.locator.object));
            assert_eq!(harness.rows().len(), 1);
        });
    }

    /// GC27: a retry after an answer that never arrived writes the same name
    /// with the bytes the row holds, and a second resume finds nothing to do.
    #[test]
    fn an_unanswered_registration_is_resolved_with_the_bytes_the_row_holds() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let object = lease_object_id(LeaseKind::Work, &tag(3)).unwrap();
            let document = wire_control::LeaseDocument::new(
                "writer".into(),
                "job".into(),
                wire_control::LeaseKind::Work,
                0,
                NOW,
            )
            .unwrap();
            let bytes = seal(
                &context,
                &object,
                &document.encode(MAX_LEASE_PLAINTEXT).unwrap(),
            )
            .unwrap();
            let row = LeaseIntent {
                locator: harness.target(&object),
                kind: LeaseKind::Work,
                job_id: "job".into(),
                seq: 0,
                bytes: bytes.clone(),
                state: LeaseState::Pending,
                created_at_ms: NOW,
            };
            harness.store.put_lease_intent(CONNECTION, &row).unwrap();

            let live = BTreeSet::from(["job".to_owned()]);
            resume(&context, &live, &cancel).await.unwrap();
            assert!(harness.provider.holds(&object));
            assert_eq!(harness.stored_bytes(&object), bytes);
            assert_eq!(harness.rows()[0].state, LeaseState::Confirmed);
            resume(&context, &live, &cancel).await.unwrap();
            assert_eq!(harness.rows().len(), 1);
            assert_eq!(harness.stored_bytes(&object), bytes);
        });
    }

    /// GC29: a job confirms its own lease, then refuses to ask for data while
    /// anyone is removing. Neither two minutes nor seven days ends that wait.
    #[test]
    fn a_job_waits_while_a_delete_marker_is_present_however_old_it_is() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let marker = harness.foreign(LeaseKind::Deleting, &tag(1));
            let yielded = admit(&context, "job", LeaseKind::Work, NOW, &cancel).await.unwrap_err();
            assert_eq!(yielded.kind, ErrorKind::Transient);
            assert_eq!(harness.rows().len(), 1, "the lease is kept while waiting");

            for later in [NOW + 2 * 60 * 1000, NOW + 7 * DAY, NOW + 400 * DAY] {
                assert_eq!(
                    admit(&context, "job", LeaseKind::Work, later, &cancel)
                        .await
                        .unwrap_err()
                        .kind,
                    ErrorKind::Transient
                );
                assert!(harness.provider.holds(&marker), "the wait removed a marker");
            }

            harness.remove(&marker).await;
            let admitted = admit(&context, "job", LeaseKind::Work, NOW + 401 * DAY, &cancel)
                .await
                .unwrap();
            assert_eq!(admitted.kind, LeaseKind::Work);
            assert!(harness.provider.holds(&admitted.locator.object));
        });
    }

    /// GC20: another device's work lease is never taken back by this one, and
    /// no amount of elapsed time changes that.
    #[test]
    fn another_device_s_work_lease_is_never_taken_back() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let foreign = harness.foreign(LeaseKind::Work, &tag(2));
            let live = BTreeSet::new();
            resume(&context, &live, &cancel).await.unwrap();
            assert!(harness.provider.holds(&foreign));
            assert!(harness.rows().is_empty());

            let observed = survey(&context, &cancel).await.unwrap();
            assert_eq!(observed.foreign_work().len(), 1);
            assert!(observed.blocking_markers(None).is_empty());

            register(&context, "job", LeaseKind::Cleanup, NOW + 400 * DAY, &cancel)
                .await
                .unwrap();
            let observed = survey(&context, &cancel).await.unwrap();
            assert_eq!(observed.leases.len(), 2);
            assert_eq!(observed.foreign_work().len(), 1);
            assert_eq!(observed.foreign_work()[0].locator.object, foreign);
            assert!(harness.provider.holds(&foreign));
        });
    }

    /// GC19: a cleanup finds the work and cleanup leases of other devices and
    /// does not count its own.
    #[test]
    fn a_cleanup_sees_every_other_device_s_lease() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            harness.foreign(LeaseKind::Work, &tag(4));
            harness.foreign(LeaseKind::Cleanup, &tag(5));
            let own = register(&context, "job", LeaseKind::Cleanup, NOW, &cancel)
                .await
                .unwrap();
            let observed = survey(&context, &cancel).await.unwrap();
            assert_eq!(observed.foreign_work().len(), 2);
            assert!(
                observed
                    .leases
                    .iter()
                    .find(|lease| lease.locator == own.locator)
                    .unwrap()
                    .mine
            );
        });
    }

    /// GC31: a request whose remote end is unknown keeps the marker, through a
    /// cancellation, a lost answer and a restart. Another job stays out until
    /// the request is known to have ended.
    #[test]
    fn an_unanswered_removal_keeps_its_marker_and_the_repository_closed() {
        let harness = Harness::new();
        runtime().block_on(async {
            let context = harness.context();
            harness
                .provider
                .seed("pack-a", ObjectRole::Pack, b"pack".to_vec());
            harness
                .provider
                .seed("pack-b", ObjectRole::Pack, b"pack".to_vec());
            let marker = place_marker(&context, "cleanup", NOW, &Cancellation::default())
                .await
                .unwrap();

            let first = harness.target("pack-a");
            note_delete_sent(&context, &marker, &first, NOW).unwrap();
            harness
                .provider
                .fail_delete("pack-a", DeleteFault::Unanswered);
            let pending = Cancellation::default();
            let (answer, ()) = tokio::join!(
                harness
                    .provider
                    .delete_object(&harness.repository, &first, &pending),
                async {
                    pending.cancel();
                }
            );
            assert_eq!(answer.unwrap_err().kind, ErrorKind::Cancelled);

            let second = harness.target("pack-b");
            note_delete_sent(&context, &marker, &second, NOW).unwrap();
            harness
                .provider
                .fail_delete("pack-b", DeleteFault::AppliedThenLost);
            assert_eq!(
                harness
                    .provider
                    .delete_object(&harness.repository, &second, &Cancellation::default())
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Transient
            );
            assert!(!harness.provider.holds("pack-b"));

            assert_eq!(
                clear_marker(&context, &marker).await.unwrap_err().kind,
                ErrorKind::PreconditionFailed
            );
            assert_eq!(
                admit(&context, "publisher", LeaseKind::Work, NOW + 7 * DAY, &Cancellation::default())
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Transient
            );
            let live = BTreeSet::from(["publisher".to_owned()]);
            resume(&context, &live, &Cancellation::default())
                .await
                .unwrap();
            assert!(harness.provider.holds(&marker.locator.object));

            // Only an answer ends a request. Removal is idempotent, so the
            // applied one answers the same way the outstanding one does.
            for pack in ["pack-a", "pack-b"] {
                harness.remove(pack).await;
                note_delete_finished(&context, &marker, &harness.target(pack)).unwrap();
            }
            clear_marker(&context, &marker).await.unwrap();
            assert!(!harness.provider.holds(&marker.locator.object));
            assert!(harness
                .store
                .unfinished_delete_requests(CONNECTION, None)
                .unwrap()
                .is_empty());
            admit(&context, "publisher", LeaseKind::Work, NOW + 8 * DAY, &Cancellation::default())
                .await
                .unwrap();
        });
    }

    /// GC20: a marker an interrupted run left behind is kept while any request
    /// of its attempt is unanswered, and given back once none is.
    #[test]
    fn a_marker_of_a_finished_job_goes_only_when_no_request_is_outstanding() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let marker = place_marker(&context, "cleanup", NOW, &cancel)
                .await
                .unwrap();
            let target = harness.target("pack-a");
            note_delete_sent(&context, &marker, &target, NOW).unwrap();

            let live = BTreeSet::new();
            resume(&context, &live, &cancel).await.unwrap();
            assert!(harness.provider.holds(&marker.locator.object));

            note_delete_finished(&context, &marker, &target).unwrap();
            resume(&context, &live, &cancel).await.unwrap();
            assert!(!harness.provider.holds(&marker.locator.object));
            assert!(harness.rows().is_empty());
        });
    }

    /// A name this device cannot classify ends the survey. A view it cannot
    /// read in full never shows that no one is removing anything.
    #[test]
    fn a_lease_name_that_cannot_be_classified_ends_the_survey() {
        let harness = Harness::new();
        runtime().block_on(async {
            let context = harness.context();
            harness
                .provider
                .seed("not-a-lease", ObjectRole::Lease, b"x".to_vec());
            assert_eq!(
                survey(&context, &Cancellation::default())
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt
            );
            assert_eq!(
                admit(&context, "job", LeaseKind::Work, NOW, &Cancellation::default())
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Corrupt
            );
        });
    }

    /// A job that reached an end gives its work lease back; a delete marker is
    /// not one and stays where it is.
    #[test]
    fn releasing_a_job_leaves_a_delete_marker_alone() {
        let harness = Harness::new();
        let cancel = Cancellation::default();
        runtime().block_on(async {
            let context = harness.context();
            let work = admit(&context, "job", LeaseKind::Work, NOW, &cancel).await.unwrap();
            let marker = place_marker(&context, "job", NOW, &cancel).await.unwrap();
            release(&context, "job").await.unwrap();
            assert!(!harness.provider.holds(&work.locator.object));
            assert!(harness.provider.holds(&marker.locator.object));
            assert_eq!(harness.rows().len(), 1);
        });
    }

    /// A cancelled job still hands its lease back, so a cancellation does not
    /// close the repository to every other device.
    #[test]
    fn a_cancelled_job_can_still_hand_its_lease_back() {
        let harness = Harness::new();
        runtime().block_on(async {
            let context = harness.context();
            let cancel = Cancellation::default();
            let work = admit(&context, "job", LeaseKind::Work, NOW, &cancel).await.unwrap();
            cancel.cancel();
            release(&context, "job").await.unwrap();
            assert!(!harness.provider.holds(&work.locator.object));
            assert!(harness.rows().is_empty());
        });
    }
}

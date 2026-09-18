//! Authenticated, finite repository protection owned by one active execution.
//! A lost response leaves a TTL lease, never a durable delete protocol.
use super::{
    contract::{
        lease_object_id, parse_lease_object_id, Cancellation, Collection, ErrorKind, LeaseKind,
        ObjectIntent, ObjectReceipt, ObjectRole, Provider, ProviderError, ReadReceipt,
        RemoteLocator, RepositoryHandle, Result, UploadResolution,
    },
    gc_store::locator_key,
    journal::validate_receipt,
    transfer::{SpoolSink, SpoolSource},
};
use risunest_external_storage_format::{
    content_identity::hash,
    control::{LeaseDocument, LeaseKind as WireLeaseKind},
    crypto::derive_key,
    format::Descriptor,
    snapshot as wire,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io::{Cursor, Write},
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex, OnceLock,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub(crate) const MINUTE_MS: u64 = 60_000;
pub(crate) const LEASE_TTL_MS: u64 = 60 * MINUTE_MS;
pub(crate) const RENEW_AFTER_MS: u64 = 15 * MINUTE_MS;
pub(crate) const RELATIVE_CLOCK_ERROR_MS: u64 = 10 * MINUTE_MS;
pub(crate) const CONTROL_DEADLINE_MS: u64 = MINUTE_MS;
pub(crate) const SAFETY_MARGIN_MS: u64 = 12 * MINUTE_MS;
pub(crate) const CLEANUP_RUN_LIMIT_MS: u64 = 45 * MINUTE_MS;
pub(crate) const UNREACHABLE_GRACE_MS: u64 = 7 * 24 * 60 * MINUTE_MS;
pub(crate) const CACHE_REUSE_LIMIT_MS: u64 = 5 * 24 * 60 * MINUTE_MS;
const MAX_LEASE_PLAINTEXT: usize = 4096;
const MAX_LEASE_CIPHERTEXT: u64 = 8192;
const PAGE_SIZE: u16 = 100;

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn transient() -> ProviderError {
    ProviderError::new(ErrorKind::Transient)
}

pub(crate) fn checked_expiry(created_at_ms: u64) -> Option<u64> {
    created_at_ms.checked_add(LEASE_TTL_MS)
}
pub(crate) fn foreign_lease_expired(expires_at_ms: u64, trusted_now_ms: u64) -> bool {
    expires_at_ms.checked_add(RELATIVE_CLOCK_ERROR_MS)
        .is_some_and(|expiry| trusted_now_ms >= expiry)
}
pub(crate) fn can_start_control_request(remaining_monotonic_ms: u64, clock_is_trusted: bool) -> bool {
    clock_is_trusted && remaining_monotonic_ms > SAFETY_MARGIN_MS
}

/// Facts from one successful HTTP response. Cache bypass is established before
/// request signing; neither an old receipt nor a cached Date is a clock sample.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TimeSample {
    pub date_ms: Option<u64>,
    pub local_before_ms: u64,
    pub local_after_ms: u64,
    pub round_trip_ms: u64,
    pub cache_bypassed: bool,
    pub cache_hit: bool,
    pub age_ms: Option<u64>,
    pub status: u16,
}
impl TimeSample {
    pub(crate) fn trusted(&self) -> bool {
        let Some(date_ms) = self.date_ms else { return false; };
        if !self.cache_bypassed || self.cache_hit || self.age_ms.is_some_and(|age| age > 0)
            || !(200..300).contains(&self.status) || self.local_after_ms < self.local_before_ms
        {
            return false;
        }
        let elapsed = i128::from(self.local_after_ms - self.local_before_ms);
        if (elapsed - i128::from(self.round_trip_ms)).abs() > 1000 {
            return false;
        }
        let local_mid = (i128::from(self.local_before_ms) + i128::from(self.local_after_ms)) / 2;
        let server_mid = i128::from(date_ms) + 500;
        let uncertainty = (i128::from(self.round_trip_ms) + 1) / 2 + 500;
        (server_mid - local_mid).abs() + uncertainty <= i128::from(5 * MINUTE_MS)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ClockReading {
    pub wall_ms: u64,
    pub monotonic_ms: u64,
    pub epoch: u64,
    pub foreground: bool,
    pub trusted: bool,
}
pub(crate) trait LeaseClock: Send + Sync {
    fn reading(&self) -> ClockReading;
}

struct ClockState {
    previous: Option<(u64, u64)>,
    verified_at: Option<u64>,
    foreground: bool,
    epoch: u64,
}
impl ClockState {
    fn invalidate(&mut self) {
        self.verified_at = None;
        match self.epoch.checked_add(1) {
            Some(epoch) => self.epoch = epoch,
            None => self.foreground = false,
        }
    }
    fn observe_reading(&mut self, wall_ms: u64, monotonic_ms: u64) {
        if let Some((before_wall, before_monotonic)) = self.previous {
            if wall_ms.checked_sub(before_wall).zip(monotonic_ms.checked_sub(before_monotonic))
                .is_none_or(|(wall, monotonic)| (i128::from(wall) - i128::from(monotonic)).abs() > 1000)
            {
                self.invalidate();
            }
        }
        self.previous = Some((wall_ms, monotonic_ms));
    }
}

/// The HTTP boundary supplies samples. The actual lifecycle owner invalidates
/// this clock on suspend, even on platforms whose Instant advances during sleep.
pub(crate) struct SystemLeaseClock {
    origin: Instant,
    state: Mutex<ClockState>,
}
impl Default for SystemLeaseClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
            state: Mutex::new(ClockState {
                previous: None, verified_at: None, foreground: true, epoch: 0,
            }),
        }
    }
}
impl SystemLeaseClock {
    pub(crate) fn observe(&self, sample: TimeSample) {
        let now = self.reading();
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        state.verified_at = if sample.trusted() && state.foreground
            && now.wall_ms.checked_sub(sample.local_after_ms)
                .is_some_and(|age| age <= CONTROL_DEADLINE_MS)
        {
            Some(now.monotonic_ms)
        } else {
            None
        };
    }
    pub(crate) fn set_foreground(&self, foreground: bool) {
        let mut state = self.state.lock().unwrap_or_else(|poison| poison.into_inner());
        if !foreground || state.foreground != foreground {
            state.invalidate();
        }
        state.foreground = foreground && state.epoch != u64::MAX;
    }
    pub(crate) fn invalidate(&self) {
        self.state.lock().unwrap_or_else(|poison| poison.into_inner()).invalidate();
    }
}
impl LeaseClock for SystemLeaseClock {
    fn reading(&self) -> ClockReading {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH)
            .ok().and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
        let monotonic = u64::try_from(self.origin.elapsed().as_millis()).ok();
        let wall_ms = wall.unwrap_or(0);
        let monotonic_ms = monotonic.unwrap_or(u64::MAX);
        let mut state = self.state.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.invalidate();
            state
        });
        state.observe_reading(wall_ms, monotonic_ms);
        ClockReading {
            wall_ms,
            monotonic_ms,
            epoch: state.epoch,
            foreground: state.foreground && wall.is_some() && monotonic.is_some(),
            trusted: state.foreground && state.verified_at.is_some_and(|verified| {
                monotonic_ms.checked_sub(verified).is_some_and(|age| age < RENEW_AFTER_MS)
            }),
        }
    }
}

static SYSTEM_LEASE_CLOCK: OnceLock<SystemLeaseClock> = OnceLock::new();

pub(crate) fn system_clock() -> &'static SystemLeaseClock {
    SYSTEM_LEASE_CLOCK.get_or_init(SystemLeaseClock::default)
}

pub(crate) fn observe_time_sample(sample: TimeSample) {
    system_clock().observe(sample);
}

pub(crate) fn set_system_foreground(foreground: bool) {
    system_clock().set_foreground(foreground);
}

/// Only small control operations use this deadline. Payload transfers retain
/// their provider timeout. Dropping this future does not cancel a remote DELETE.
pub(crate) async fn control_request<T>(
    cancel: &Cancellation,
    operation: impl Future<Output = Result<T>>,
) -> Result<T> {
    cancel.check()?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(ProviderError::new(ErrorKind::Cancelled)),
        result = tokio::time::timeout(Duration::from_millis(CONTROL_DEADLINE_MS), operation) => {
            result.map_err(|_| transient())?
        }
    }
}

/// A complete collection cannot contain duplicate locators or loop its cursor.
/// There is no page-count cutoff that turns a partial walk into a complete one.
#[derive(Default)]
pub(crate) struct PageTracker {
    cursors: BTreeSet<String>,
    locators: BTreeSet<String>,
    empty_page_seen: bool,
}
impl PageTracker {
    pub(crate) fn accept(
        &mut self,
        repository: &RepositoryHandle,
        objects: &[ObjectReceipt],
        next: Option<&str>,
    ) -> Result<()> {
        if objects.is_empty() && next.is_some() {
            if self.empty_page_seen { return Err(corrupt()); }
            self.empty_page_seen = true;
        }
        for object in objects {
            object.locator.validate_for(repository)?;
            if !object.complete || object.byte_length == 0
                || !self.locators.insert(locator_key(&object.locator)?)
            {
                return Err(corrupt());
            }
        }
        if let Some(cursor) = next {
            if cursor.is_empty() || cursor.len() > 64 * 1024
                || cursor.bytes().any(|byte| matches!(byte, 0 | b'\r' | b'\n'))
                || !self.cursors.insert(cursor.to_owned())
            {
                return Err(corrupt());
            }
        }
        Ok(())
    }
}

pub(crate) struct LeaseContext<'a> {
    pub root: &'a Path,
    pub connection_id: &'a str,
    pub writer_id: &'a str,
    pub descriptor: &'a Descriptor,
    pub root_key: &'a [u8; 32],
    pub provider: &'a dyn Provider,
    pub repository: &'a RepositoryHandle,
    pub clock: &'a dyn LeaseClock,
    pub protection_supported: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LeaseHandle {
    pub job_id: String,
    pub kind: LeaseKind,
    pub seq: u64,
    pub object_id: String,
    pub locator: RemoteLocator,
    pub created_at_ms: u64,
    pub expires_at_ms: u64,
    issued_monotonic_ms: u64,
    clock_epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservedLease {
    pub locator: RemoteLocator,
    pub object_id: Option<String>,
    pub document: Option<LeaseDocument>,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LeaseSurvey {
    pub leases: Vec<ObservedLease>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum YieldReason {
    ForeignWork,
    ForeignCleanup,
    ForeignDeletion,
    UnknownProtection,
    Suspended,
    ProtectionLost,
}
pub(crate) enum Admission {
    Admitted(LeaseOwner),
    Yield { reason: YieldReason },
    UnsupportedProtection,
}
impl LeaseSurvey {
    pub(crate) fn blocker(&self, owned: &BTreeSet<String>, now: ClockReading) -> Option<YieldReason> {
        for lease in &self.leases {
            let Some(document) = &lease.document else { return Some(YieldReason::UnknownProtection); };
            let Some(object_id) = &lease.object_id else { return Some(YieldReason::UnknownProtection); };
            if owned.contains(object_id) { continue; }
            if now.trusted && foreign_lease_expired(document.expires_at_ms, now.wall_ms) { continue; }
            return Some(match document.kind {
                WireLeaseKind::Work => YieldReason::ForeignWork,
                WireLeaseKind::Cleanup => YieldReason::ForeignCleanup,
                WireLeaseKind::Deleting => YieldReason::ForeignDeletion,
            });
        }
        None
    }
}

fn wire_kind(kind: LeaseKind) -> WireLeaseKind {
    match kind {
        LeaseKind::Work => WireLeaseKind::Work,
        LeaseKind::Cleanup => WireLeaseKind::Cleanup,
        LeaseKind::Deleting => WireLeaseKind::Deleting,
    }
}
fn staging(root: &Path) -> Result<tempfile::TempDir> {
    std::fs::create_dir_all(root).map_err(|_| transient())?;
    if crate::trust_boundary::is_link_like(&std::fs::symlink_metadata(root).map_err(|_| transient())?) {
        return Err(corrupt());
    }
    tempfile::tempdir_in(root).map_err(|_| transient())
}
fn seal(context: &LeaseContext<'_>, object_id: &str, document: &LeaseDocument) -> Result<Vec<u8>> {
    context.descriptor.validate().map_err(|_| corrupt())?;
    let plaintext = document.encode(MAX_LEASE_PLAINTEXT).map_err(|_| corrupt())?;
    let header = wire::PublicObjectHeader::new(
        context.descriptor.repository_id.clone(), object_id.into(), wire::ObjectRole::Lease,
        plaintext.len() as u64,
    ).map_err(|_| corrupt())?;
    let key = derive_key(context.root_key, &context.descriptor.repository_id, "metadata")
        .map_err(|_| corrupt())?;
    let mut bytes = Vec::new();
    wire::seal_envelope(&mut Cursor::new(plaintext), &mut bytes, &key, &header)
        .map_err(|_| corrupt())?;
    if bytes.len() as u64 > MAX_LEASE_CIPHERTEXT { return Err(corrupt()); }
    Ok(bytes)
}
async fn read_lease(
    context: &LeaseContext<'_>, receipt: &ObjectReceipt, cancel: &Cancellation,
) -> Result<(String, LeaseDocument)> {
    receipt.locator.validate_for(context.repository)?;
    if !receipt.complete || receipt.byte_length == 0 || receipt.byte_length > MAX_LEASE_CIPHERTEXT {
        return Err(corrupt());
    }
    let temporary = staging(context.root)?;
    let path = temporary.path().join("lease");
    let mut sink = SpoolSink::create(&path, MAX_LEASE_CIPHERTEXT)?;
    let current = match control_request(cancel, context.provider.read_object(
        context.repository, &receipt.locator, None, &mut sink, cancel,
    )).await? {
        ReadReceipt::Body(current) => current,
        ReadReceipt::NotModified(_) => return Err(corrupt()),
    };
    if !current.complete || current.locator != receipt.locator
        || current.byte_length != receipt.byte_length || !sink.is_verified()
    {
        return Err(corrupt());
    }
    let key = derive_key(context.root_key, &context.descriptor.repository_id, "metadata")
        .map_err(|_| corrupt())?;
    let mut input = crate::trust_boundary::open_regular_source(&path).map_err(|_| transient())?;
    let mut plaintext = Vec::new();
    let header = wire::open_envelope(&mut input, &mut plaintext, &key, MAX_LEASE_PLAINTEXT as u64)
        .map_err(|_| corrupt())?;
    let document = LeaseDocument::decode(&plaintext, MAX_LEASE_PLAINTEXT).map_err(|_| corrupt())?;
    let (kind, _) = parse_lease_object_id(&header.object_id)?;
    if header.repository_id != context.descriptor.repository_id || header.role != wire::ObjectRole::Lease
        || header.plaintext_length != plaintext.len() as u64 || document.kind != wire_kind(kind)
    {
        return Err(corrupt());
    }
    Ok((header.object_id, document))
}

pub(crate) async fn survey(context: &LeaseContext<'_>, cancel: &Cancellation) -> Result<LeaseSurvey> {
    let mut survey = LeaseSurvey::default();
    let mut tracker = PageTracker::default();
    let mut object_ids = BTreeSet::new();
    let mut cursor: Option<String> = None;
    loop {
        let page = control_request(cancel, context.provider.list_objects(
            context.repository, Collection::Leases, cursor.as_deref(), PAGE_SIZE, cancel,
        )).await?;
        tracker.accept(context.repository, &page.objects, page.next_cursor.as_deref())?;
        for receipt in page.objects {
            match read_lease(context, &receipt, cancel).await {
                Ok((object_id, document)) => {
                    if !object_ids.insert(object_id.clone()) { return Err(corrupt()); }
                    survey.leases.push(ObservedLease {
                        locator: receipt.locator, object_id: Some(object_id), document: Some(document),
                    });
                }
                Err(error) if error.kind == ErrorKind::Corrupt => survey.leases.push(ObservedLease {
                    locator: receipt.locator, object_id: None, document: None,
                }),
                // Disappearance or a failed read breaks this complete observation.
                Err(error) => return Err(error),
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => return Ok(survey),
        }
    }
}

async fn create_lease(
    context: &LeaseContext<'_>, job_id: &str, kind: LeaseKind, seq: u64, cancel: &Cancellation,
) -> Result<LeaseHandle> {
    let before = context.clock.reading();
    if !before.foreground { return Err(transient()); }
    checked_expiry(before.wall_ms).ok_or_else(corrupt)?;
    let object_id = lease_object_id(kind, &uuid::Uuid::new_v4().simple().to_string())?;
    let document = LeaseDocument::new(
        context.writer_id.into(), job_id.into(), wire_kind(kind), seq, before.wall_ms,
    ).map_err(|_| corrupt())?;
    let bytes = seal(context, &object_id, &document)?;
    let intent = ObjectIntent {
        repository_id: context.repository.repository_id.clone(), job_id: job_id.into(),
        object_id: object_id.clone(), role: ObjectRole::Lease,
        byte_length: bytes.len() as u64, sha256: hex::encode(hash(&bytes)),
    };
    intent.validate(context.repository)?;
    let temporary = staging(context.root)?;
    let path = temporary.path().join("lease");
    let mut file = std::fs::OpenOptions::new().create_new(true).write(true).open(&path)
        .map_err(|_| transient())?;
    file.write_all(&bytes).map_err(|_| transient())?;
    file.sync_all().map_err(|_| transient())?;
    drop(file);
    let source = SpoolSource::verified(&path, intent.byte_length, &intent.sha256)?;
    let resume = control_request(cancel, context.provider.begin_upload(context.repository, &intent, cancel)).await?;
    let receipt = match control_request(cancel, context.provider.create_object(
        context.repository, &intent, &source, resume.as_ref(), cancel,
    )).await {
        Ok(receipt) => receipt,
        Err(error) if error.kind == ErrorKind::Transient => {
            match control_request(cancel, context.provider.reconcile_upload(
                context.repository, &intent, resume.as_ref(), cancel,
            )).await? {
                UploadResolution::Complete(receipt) => receipt,
                UploadResolution::Conflict => return Err(corrupt()),
                _ => return Err(error),
            }
        }
        Err(error) => return Err(error),
    };
    validate_receipt(&intent, context.repository, &receipt)?;
    let (confirmed_id, confirmed) = read_lease(context, &receipt, cancel).await?;
    let after = context.clock.reading();
    if confirmed_id != object_id || confirmed != document { return Err(corrupt()); }
    if !after.foreground || before.epoch != after.epoch
        || after.monotonic_ms.checked_sub(before.monotonic_ms)
            .is_none_or(|elapsed| elapsed >= LEASE_TTL_MS - SAFETY_MARGIN_MS)
    {
        return Err(transient());
    }
    Ok(LeaseHandle {
        job_id: job_id.into(), kind, seq, object_id, locator: receipt.locator,
        created_at_ms: document.created_at_ms, expires_at_ms: document.expires_at_ms,
        issued_monotonic_ms: before.monotonic_ms, clock_epoch: before.epoch,
    })
}

async fn release(context: &LeaseContext<'_>, lease: &LeaseHandle, cancel: &Cancellation) -> Result<()> {
    lease.locator.validate_for(context.repository)?;
    match control_request(cancel, context.provider.delete_object(context.repository, &lease.locator, cancel)).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind == ErrorKind::NotFound || error.http_status == Some(404) => Ok(()),
        Err(error) => Err(error),
    }
}

pub(crate) async fn admit(
    context: &LeaseContext<'_>, job_id: &str, kind: LeaseKind, cancel: &Cancellation,
) -> Result<Admission> {
    cancel.check()?;
    if !context.protection_supported { return Ok(Admission::UnsupportedProtection); }
    if kind == LeaseKind::Deleting { return Err(corrupt()); }
    if !context.clock.reading().foreground {
        return Ok(Admission::Yield { reason: YieldReason::Suspended });
    }
    let before = match survey(context, cancel).await {
        Ok(before) => before,
        Err(error) if error.kind == ErrorKind::Unsupported => return Ok(Admission::UnsupportedProtection),
        Err(error) => return Err(error),
    };
    if let Some(reason) = before.blocker(&BTreeSet::new(), context.clock.reading()) {
        return Ok(Admission::Yield { reason });
    }
    let sequence = before.leases.iter().filter_map(|lease| lease.document.as_ref())
        .filter(|lease| lease.writer_id == context.writer_id && lease.job_id == job_id && lease.kind == wire_kind(kind))
        .map(|lease| lease.seq).max().map(|seq| seq.checked_add(1).ok_or_else(corrupt)).transpose()?.unwrap_or(0);
    let lease = match create_lease(context, job_id, kind, sequence, cancel).await {
        Ok(lease) => lease,
        Err(error) if error.kind == ErrorKind::Unsupported => return Ok(Admission::UnsupportedProtection),
        Err(error) => return Err(error),
    };
    let owner = LeaseOwner::new(lease);
    match owner.recheck(context, cancel).await {
        Ok(None) => Ok(Admission::Admitted(owner)),
        Ok(Some(reason)) => {
            owner.release_all(context).await;
            Ok(Admission::Yield { reason })
        }
        Err(error) => {
            owner.release_all(context).await;
            Err(error)
        }
    }
}

struct OwnerState {
    primary: LeaseHandle,
    marker: Option<LeaseHandle>,
    held: BTreeMap<String, LeaseHandle>,
}

/// This non-cloneable value belongs to the actual repository worker, not its
/// durable job summary. Exactly one renewal future runs alongside that worker.
pub(crate) struct LeaseOwner {
    state: Mutex<OwnerState>,
    renewal: tokio::sync::Mutex<()>,
    running: AtomicBool,
    closed: AtomicBool,
    delete_in_flight: AtomicBool,
}
impl LeaseOwner {
    fn new(primary: LeaseHandle) -> Self {
        Self {
            state: Mutex::new(OwnerState {
                held: [(primary.object_id.clone(), primary.clone())].into_iter().collect(),
                primary, marker: None,
            }),
            renewal: tokio::sync::Mutex::new(()),
            running: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            delete_in_flight: AtomicBool::new(false),
        }
    }
    fn state(&self) -> Result<std::sync::MutexGuard<'_, OwnerState>> {
        self.state.lock().map_err(|_| corrupt())
    }
    pub(crate) fn primary(&self) -> Result<LeaseHandle> {
        Ok(self.state()?.primary.clone())
    }
    pub(crate) fn remaining_ms(&self, context: &LeaseContext<'_>, destructive: bool) -> Result<u64> {
        let now = context.clock.reading();
        if self.closed.load(Ordering::Acquire) || !now.foreground { return Err(transient()); }
        let state = self.state()?;
        let remaining = |lease: &LeaseHandle| -> Result<u64> {
            if lease.clock_epoch != now.epoch { return Err(transient()); }
            let elapsed = now.monotonic_ms.checked_sub(lease.issued_monotonic_ms).ok_or_else(transient)?;
            Ok(LEASE_TTL_MS.saturating_sub(elapsed))
        };
        let mut left = remaining(&state.primary)?;
        if destructive {
            if let Some(marker) = &state.marker { left = left.min(remaining(marker)?); }
        }
        Ok(left)
    }
    pub(crate) fn check_control(&self, context: &LeaseContext<'_>, destructive: bool) -> Result<()> {
        if !can_start_control_request(
            self.remaining_ms(context, destructive)?, !destructive || context.clock.reading().trusted,
        ) {
            return Err(transient());
        }
        Ok(())
    }
    pub(crate) fn set_delete_in_flight(&self, value: bool) {
        self.delete_in_flight.store(value, Ordering::Release);
    }
    pub(crate) async fn recheck(
        &self, context: &LeaseContext<'_>, cancel: &Cancellation,
    ) -> Result<Option<YieldReason>> {
        if self.check_control(context, false).is_err() {
            return Ok(Some(YieldReason::ProtectionLost));
        }
        let survey = survey(context, cancel).await?;
        let state = self.state()?;
        let owned = state.held.keys().cloned().collect();
        for current in std::iter::once(&state.primary).chain(state.marker.iter()) {
            if !survey.leases.iter().any(|lease| {
                lease.object_id.as_ref() == Some(&current.object_id) && lease.locator == current.locator
                    && lease.document.as_ref().is_some_and(|document| {
                        document.writer_id == context.writer_id && document.job_id == current.job_id
                            && document.kind == wire_kind(current.kind) && document.seq == current.seq
                            && document.created_at_ms == current.created_at_ms
                            && document.expires_at_ms == current.expires_at_ms
                    })
            }) {
                return Ok(Some(YieldReason::ProtectionLost));
            }
        }
        drop(state);
        if self.check_control(context, false).is_err() {
            return Ok(Some(YieldReason::ProtectionLost));
        }
        Ok(survey.blocker(&owned, context.clock.reading()))
    }
    pub(crate) async fn place_marker(&self, context: &LeaseContext<'_>, cancel: &Cancellation) -> Result<()> {
        let _renewing = self.renewal.lock().await;
        self.check_control(context, true)?;
        let primary = self.primary()?;
        if primary.kind != LeaseKind::Cleanup || self.state()?.marker.is_some() { return Err(corrupt()); }
        let marker = create_lease(context, &primary.job_id, LeaseKind::Deleting, 0, cancel).await?;
        let mut state = self.state()?;
        state.held.insert(marker.object_id.clone(), marker.clone());
        state.marker = Some(marker);
        Ok(())
    }
    pub(crate) async fn renew_if_due(&self, context: &LeaseContext<'_>, cancel: &Cancellation) -> Result<()> {
        let _renewing = self.renewal.lock().await;
        cancel.check()?;
        self.check_control(context, false)?;
        let leases = {
            let state = self.state()?;
            std::iter::once(state.primary.clone()).chain(state.marker.clone()).collect::<Vec<_>>()
        };
        for previous in leases {
            let now = context.clock.reading();
            if now.epoch != previous.clock_epoch { return Err(transient()); }
            let elapsed = now.monotonic_ms.checked_sub(previous.issued_monotonic_ms).ok_or_else(transient)?;
            if elapsed < RENEW_AFTER_MS { continue; }
            let seq = previous.seq.checked_add(1).ok_or_else(corrupt)?;
            let next = create_lease(context, &previous.job_id, previous.kind, seq, cancel).await?;
            {
                let mut state = self.state()?;
                state.held.insert(next.object_id.clone(), next.clone());
                if next.kind == LeaseKind::Deleting { state.marker = Some(next); }
                else { state.primary = next; }
            }
            // Authenticate and install the successor before giving back its predecessor.
            if release(context, &previous, cancel).await.is_ok() {
                self.state()?.held.remove(&previous.object_id);
            }
        }
        self.check_control(context, false)
    }
    async fn renewal_loop(&self, context: &LeaseContext<'_>, cancel: &Cancellation) -> Result<()> {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(ProviderError::new(ErrorKind::Cancelled)),
                _ = tokio::time::sleep(Duration::from_millis(MINUTE_MS)) => {}
            }
            if let Err(error) = self.renew_if_due(context, cancel).await {
                // A temporary renewal failure does not erase an otherwise valid
                // lease or the provider's upload session. Never extend its deadline.
                if error.kind != ErrorKind::Transient || self.check_control(context, false).is_err() {
                    return Err(error);
                }
            }
        }
    }
    pub(crate) async fn release_all(&self, context: &LeaseContext<'_>) {
        self.closed.store(true, Ordering::Release);
        let leases = {
            let Ok(state) = self.state() else { return; };
            state.held.values().cloned().collect::<Vec<_>>()
        };
        let cancel = Cancellation::default();
        // Shutdown must not wait one deadline for every failed old renewal.
        // Any unreleased object is finite and expires without local replay.
        let _ = tokio::time::timeout(Duration::from_millis(CONTROL_DEADLINE_MS), async {
            for lease in leases {
                if lease.kind == LeaseKind::Deleting && self.delete_in_flight.load(Ordering::Acquire) { continue; }
                if release(context, &lease, &cancel).await.is_ok() {
                    if let Ok(mut state) = self.state() { state.held.remove(&lease.object_id); }
                }
            }
        }).await;
    }
    pub(crate) async fn run<T>(
        &self, context: &LeaseContext<'_>, cancel: &Cancellation,
        operation: impl Future<Output = Result<T>>,
    ) -> Result<T> {
        if self.running.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_err() {
            return Err(corrupt());
        }
        struct Running<'a>(&'a LeaseOwner);
        impl Drop for Running<'_> {
            fn drop(&mut self) {
                self.0.closed.store(true, Ordering::Release);
                self.0.running.store(false, Ordering::Release);
            }
        }
        let _running = Running(self);
        let result = match self.check_control(context, false) {
            Err(error) => Err(error),
            Ok(()) => tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(ProviderError::new(ErrorKind::Cancelled)),
                result = operation => result,
                result = self.renewal_loop(context, cancel) => {
                    match result { Err(error) => Err(error), Ok(()) => Err(transient()) }
                }
            },
        };
        self.release_all(context).await;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_storage::fake::{self, DeleteFault, FakeLeaseClock, FakeProvider};
    use risunest_external_storage_format::format::Strategy;

    pub(super) const NOW: u64 = 1000 * 24 * 60 * MINUTE_MS;
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }
    struct Harness {
        root: tempfile::TempDir,
        provider: FakeProvider,
        repository: RepositoryHandle,
        descriptor: Descriptor,
        clock: FakeLeaseClock,
        key: [u8; 32],
    }
    impl Harness {
        fn new() -> Self {
            Self {
                root: tempfile::tempdir().unwrap(), provider: FakeProvider::new(true),
                repository: fake::repository(),
                descriptor: Descriptor::new("format-repository".into(), Some(Strategy::Cas)).unwrap(),
                clock: FakeLeaseClock::new(NOW), key: [7; 32],
            }
        }
        fn context(&self) -> LeaseContext<'_> {
            LeaseContext {
                root: self.root.path(), connection_id: "connection", writer_id: "writer",
                descriptor: &self.descriptor, root_key: &self.key,
                provider: &self.provider, repository: &self.repository,
                clock: &self.clock, protection_supported: true,
            }
        }
        fn foreign(&self, kind: LeaseKind, index: u64, now: u64) -> (String, Vec<u8>) {
            let id = lease_object_id(kind, &format!("{index:032x}")).unwrap();
            let document = LeaseDocument::new("foreign".into(), "job".into(), wire_kind(kind), 0, now).unwrap();
            let bytes = seal(&self.context(), &id, &document).unwrap();
            (id, bytes)
        }
        async fn owner(&self, kind: LeaseKind) -> LeaseOwner {
            match admit(&self.context(), "job", kind, &Cancellation::default()).await.unwrap() {
                Admission::Admitted(owner) => owner,
                _ => panic!("expected admission"),
            }
        }
    }

    #[test]
    fn c_ttl_relative_expiry_and_safety_boundaries_are_exact() {
        assert_eq!(LEASE_TTL_MS, risunest_external_storage_format::control::LEASE_TTL_MS);
        assert_eq!(checked_expiry(NOW), Some(NOW + 60 * MINUTE_MS));
        assert_eq!(checked_expiry(u64::MAX), None);
        let expiry = checked_expiry(NOW).unwrap();
        assert!(!foreign_lease_expired(expiry, NOW + 60 * MINUTE_MS));
        assert!(!foreign_lease_expired(expiry, NOW + 70 * MINUTE_MS - 1));
        assert!(foreign_lease_expired(expiry, NOW + 70 * MINUTE_MS));
        assert!(!foreign_lease_expired(u64::MAX, u64::MAX));
        assert!(!can_start_control_request(SAFETY_MARGIN_MS, true));
        assert!(can_start_control_request(SAFETY_MARGIN_MS + 1, true));
        assert!(!can_start_control_request(LEASE_TTL_MS, false));
    }

    #[test]
    fn c_clock_samples_include_second_precision_rtt_and_cache_uncertainty() {
        let sample = TimeSample {
            date_ms: Some(NOW), local_before_ms: NOW + 500, local_after_ms: NOW + 500,
            round_trip_ms: 0, cache_bypassed: true, cache_hit: false, age_ms: None, status: 200,
        };
        assert!(sample.trusted());
        for offset in [299_500_i64, -299_500] {
            let local = (i128::from(NOW + 500) + i128::from(offset)) as u64;
            assert!(TimeSample { local_before_ms: local, local_after_ms: local, ..sample }.trusted());
            let outside = (i128::from(local) + i128::from(offset.signum())) as u64;
            assert!(!TimeSample { local_before_ms: outside, local_after_ms: outside, ..sample }.trusted());
        }
        for invalid in [
            TimeSample { date_ms: None, ..sample }, TimeSample { cache_bypassed: false, ..sample },
            TimeSample { cache_hit: true, ..sample }, TimeSample { age_ms: Some(1), ..sample },
            TimeSample { status: 304, ..sample }, TimeSample { status: 401, ..sample },
            TimeSample { local_after_ms: NOW, ..sample },
            TimeSample { local_after_ms: NOW + 500 + 600_000, round_trip_ms: 600_000, ..sample },
            TimeSample { round_trip_ms: 1001, ..sample },
        ] {
            assert!(!invalid.trusted(), "{invalid:?}");
        }
    }

    #[test]
    fn c_wall_clock_discontinuities_invalidate_instead_of_extending_protection() {
        let mut state = ClockState { previous: None, verified_at: Some(0), foreground: true, epoch: 0 };
        state.observe_reading(NOW, 0);
        state.observe_reading(NOW + MINUTE_MS, MINUTE_MS);
        assert_eq!(state.epoch, 0);
        state.observe_reading(NOW + 2 * MINUTE_MS + 1001, 2 * MINUTE_MS);
        assert_eq!(state.epoch, 1);
        assert_eq!(state.verified_at, None);
        state.observe_reading(NOW, 3 * MINUTE_MS);
        assert_eq!(state.epoch, 2);
    }

    #[test]
    fn c_foreign_work_cleanup_and_deleting_yield_before_any_upload() {
        runtime().block_on(async {
            for (kind, reason) in [
                (LeaseKind::Work, YieldReason::ForeignWork),
                (LeaseKind::Cleanup, YieldReason::ForeignCleanup),
                (LeaseKind::Deleting, YieldReason::ForeignDeletion),
            ] {
                let h = Harness::new();
                let (id, bytes) = h.foreign(kind, 1, NOW);
                h.provider.seed(&id, ObjectRole::Lease, bytes);
                let result = admit(&h.context(), "job", LeaseKind::Work, &Cancellation::default()).await.unwrap();
                assert!(matches!(result, Admission::Yield { reason: actual } if actual == reason));
                assert!(h.provider.uploaded_ids().is_empty());
                assert_eq!(h.provider.delete_attempts(&id), 0);
            }
        });
    }

    #[test]
    fn c_registration_race_yields_and_releases_only_its_own_lease() {
        runtime().block_on(async {
            let h = Harness::new();
            let (id, bytes) = h.foreign(LeaseKind::Work, 2, NOW);
            h.provider.seed_after_list(1, &id, ObjectRole::Lease, bytes);
            let result = admit(&h.context(), "job", LeaseKind::Work, &Cancellation::default()).await.unwrap();
            assert!(matches!(result, Admission::Yield { reason: YieldReason::ForeignWork }));
            assert!(h.provider.holds(&id));
            assert_eq!(h.provider.delete_attempts(&id), 0);
            let uploaded = h.provider.uploaded_ids();
            assert_eq!(uploaded.len(), 1);
            assert!(!h.provider.holds(&uploaded[0]));
        });
    }

    #[test]
    fn c_expired_lease_needs_trusted_time_and_future_or_corrupt_lease_is_not_ignored() {
        runtime().block_on(async {
            let h = Harness::new();
            let (id, bytes) = h.foreign(LeaseKind::Work, 1, NOW);
            h.provider.seed(&id, ObjectRole::Lease, bytes);
            h.clock.advance(70 * MINUTE_MS);
            h.clock.set_trusted(false);
            assert!(matches!(admit(&h.context(), "job", LeaseKind::Work, &Cancellation::default()).await.unwrap(), Admission::Yield { .. }));
            h.clock.set_trusted(true);
            let owner = h.owner(LeaseKind::Work).await;
            owner.release_all(&h.context()).await;
            let (future, bytes) = h.foreign(LeaseKind::Work, 2, NOW + 100 * LEASE_TTL_MS);
            h.provider.seed(&future, ObjectRole::Lease, bytes);
            assert!(matches!(admit(&h.context(), "job", LeaseKind::Work, &Cancellation::default()).await.unwrap(), Admission::Yield { .. }));
            h.provider.forget(&future);
            h.provider.seed("unknown", ObjectRole::Lease, b"not authenticated".to_vec());
            assert!(matches!(admit(&h.context(), "job", LeaseKind::Work, &Cancellation::default()).await.unwrap(), Admission::Yield { reason: YieldReason::UnknownProtection }));
        });
    }

    #[test]
    fn c_full_lease_enumeration_supports_multiple_pages_and_rejects_repeated_cursors() {
        runtime().block_on(async {
            let h = Harness::new();
            for index in 0..101 {
                let (id, bytes) = h.foreign(LeaseKind::Work, index, NOW);
                h.provider.seed(&id, ObjectRole::Lease, bytes);
            }
            let survey = survey(&h.context(), &Cancellation::default()).await.unwrap();
            assert_eq!(survey.leases.len(), 101);
            h.provider.script_page(Collection::Leases, Ok(super::super::contract::ObjectPage {
                objects: vec![], next_cursor: Some("repeat".into()),
            }));
            h.provider.script_page(Collection::Leases, Ok(super::super::contract::ObjectPage {
                objects: vec![], next_cursor: Some("repeat".into()),
            }));
            assert_eq!(super::survey(&h.context(), &Cancellation::default()).await.unwrap_err().kind, ErrorKind::Corrupt);
        });
    }

    #[test]
    fn c_renewal_confirms_a_new_lease_before_releasing_the_predecessor() {
        runtime().block_on(async {
            let h = Harness::new();
            let owner = h.owner(LeaseKind::Work).await;
            let old = owner.primary().unwrap();
            h.clock.advance(RENEW_AFTER_MS - 1);
            owner.renew_if_due(&h.context(), &Cancellation::default()).await.unwrap();
            assert_eq!(owner.primary().unwrap(), old);
            h.clock.advance(1);
            h.provider.fail_delete(&old.object_id, DeleteFault::Transient);
            owner.renew_if_due(&h.context(), &Cancellation::default()).await.unwrap();
            let next = owner.primary().unwrap();
            assert_ne!(old.object_id, next.object_id);
            assert_eq!(next.seq, old.seq + 1);
            assert_eq!(next.expires_at_ms, next.created_at_ms + LEASE_TTL_MS);
            assert!(h.provider.holds(&old.object_id));
            assert!(h.provider.holds(&next.object_id));
            assert_eq!(owner.recheck(&h.context(), &Cancellation::default()).await.unwrap(), None);
            owner.release_all(&h.context()).await;
        });
    }

    #[test]
    fn c_suspend_and_lost_remote_lease_require_new_admission() {
        runtime().block_on(async {
            let h = Harness::new();
            let owner = h.owner(LeaseKind::Work).await;
            h.clock.suspend();
            h.clock.foreground();
            h.clock.set_trusted(true);
            assert!(owner.check_control(&h.context(), false).is_err());
            owner.release_all(&h.context()).await;
            let fresh = h.owner(LeaseKind::Work).await;
            h.provider.forget(&fresh.primary().unwrap().object_id);
            assert_eq!(fresh.recheck(&h.context(), &Cancellation::default()).await.unwrap(), Some(YieldReason::ProtectionLost));
        });
    }

    #[test]
    fn c_one_owner_runs_once_and_cancellation_does_not_execute_the_body() {
        runtime().block_on(async {
            let h = Harness::new();
            let owner = h.owner(LeaseKind::Work).await;
            let cancel = Cancellation::default();
            owner.running.store(true, Ordering::Release);
            let duplicate: Result<()> = owner.run(&h.context(), &cancel, async { panic!("second worker") }).await;
            assert_eq!(duplicate.unwrap_err().kind, ErrorKind::Corrupt);
            owner.running.store(false, Ordering::Release);
            cancel.cancel();
            let result: Result<()> = owner.run(&h.context(), &cancel, async { panic!("cancelled worker") }).await;
            assert_eq!(result.unwrap_err().kind, ErrorKind::Cancelled);
            assert!(owner.check_control(&h.context(), false).is_err());
        });
    }

    #[test]
    fn c_execution_future_can_be_owned_by_the_native_worker() {
        fn require_send<T: Send>(_: T) {}
        runtime().block_on(async {
            let h = Harness::new();
            let owner = h.owner(LeaseKind::Work).await;
            let context = h.context();
            let cancel = Cancellation::default();
            require_send(owner.run(&context, &cancel, async { Ok(()) }));
            owner.release_all(&context).await;
        });
    }

    #[test]
    fn c_unsupported_protection_does_not_send_remote_requests() {
        runtime().block_on(async {
            let h = Harness::new();
            let mut context = h.context();
            context.protection_supported = false;
            assert!(matches!(admit(&context, "job", LeaseKind::Work, &Cancellation::default()).await.unwrap(), Admission::UnsupportedProtection));
            assert!(h.provider.uploaded_ids().is_empty());
            assert_eq!(h.provider.listing_count(), 0);
        });
    }
}

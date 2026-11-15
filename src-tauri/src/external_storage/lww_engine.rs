use super::{
    contract::*,
    http::HttpClockSample,
    lww_segment::{self as segment, LargeBody, Segment},
};
#[cfg(test)]
use crate::persistent_store::lww::ApplyReceive;
use crate::persistent_store::{
    external_lww::{
        publication_directory, sealed_body_path, AssetReference, FrozenAsset, FrozenAssetReference, FrozenControl,
        FrozenControlCatalog, SealedBody, SealedPublication, SegmentReferences, UploadState,
    },
    lww::{Change, Header, MessageLocator, Progress, StageReceive},
    PersistentStore,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use risunest_sync_wire::{library_format, stamp::{DecimalU64, Stamp}, unit::UnitValue};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Cursor,
    pin::Pin,
    sync::Arc,
    time::Instant,
};
#[cfg(test)]
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
#[cfg(test)]
use super::worker_observation::spawn_blocking;
#[cfg(not(test))]
use tokio::task::spawn_blocking;

/// The largest listing page every provider accepts.
pub(crate) const LISTING_PAGE: u16 = 1000;
const RECEIVE_SEGMENT_LIMIT: usize = 4;
#[cfg(test)]
#[path = "lww_scheduling_tests.rs"]
mod scheduling_tests;
/// Providers name a locator's collection after a role folder or a release tag.
pub(crate) const MAX_LOCATOR_COLLECTION_BYTES: usize = 128;
/// JSON bytes of changes in one receive page. One change is far smaller, and
/// a larger one would still travel alone.
pub(crate) const RECEIVE_PAGE_BYTES: usize = 4 * 1024 * 1024;
#[cfg(test)]
thread_local! {
    static TEST_RECEIVE_PAGE_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(RECEIVE_PAGE_BYTES) };
}
#[cfg(test)]
pub(crate) fn set_receive_page_bytes_for_test(bytes: usize) {
    TEST_RECEIVE_PAGE_BYTES.with(|value| value.set(bytes));
}
fn receive_page_bytes() -> usize {
    #[cfg(test)]
    return TEST_RECEIVE_PAGE_BYTES.with(std::cell::Cell::get);
    #[cfg(not(test))]
    RECEIVE_PAGE_BYTES
}
/// Whether a change of `length` bytes starts a new page after `used` bytes.
fn page_break(used: usize, length: usize, budget: usize, empty: bool) -> bool {
    used.saturating_add(length) > budget && !empty
}
fn change_length(change: &Change) -> Result<usize> {
    Ok(serde_json::to_vec(change).map_err(|_| segment::corrupt())?.len() + 1)
}
/// Splits changes in order into pages within the page budget. There is
/// always at least one page, so a group without changes still carries its
/// progress.
#[cfg(test)]
pub(crate) fn receive_pages(changes: Vec<Change>, budget: usize) -> Result<Vec<Vec<Change>>> {
    let mut pages = vec![Vec::new()];
    let mut used = 0usize;
    for change in changes {
        let length = change_length(&change)?;
        if page_break(used, length, budget, pages.last().is_none_or(Vec::is_empty)) {
            pages.push(Vec::new());
            used = 0;
        }
        used = used.saturating_add(length);
        pages.last_mut().ok_or_else(segment::corrupt)?.push(change);
    }
    Ok(pages)
}
/// Stores one group's pages as its changes arrive and collects the ids of
/// those not yet finished. Only the last page carries the group's progress;
/// earlier pages repeat the writer's current cursor, so progress moves once
/// the whole group is applied.
struct PageWriter<'a> {
    target: &'a str,
    id: String,
    authority: DecimalU64,
    progress: Progress,
    current: DecimalU64,
    admitted_time_upper_ms: DecimalU64,
    page: Vec<Change>,
    used: usize,
    index: usize,
}
impl<'a> PageWriter<'a> {
    fn new(target: &'a str, id: String, authority: DecimalU64, progress: Progress, current: DecimalU64, admitted_time_upper_ms: DecimalU64) -> Self {
        Self { target, id, authority, progress, current, admitted_time_upper_ms, page: Vec::new(), used: 0, index: 0 }
    }
    fn push(&mut self, store: &PersistentStore, offered: &mut Vec<String>, change: Change) -> Result<()> {
        let length = change_length(&change)?;
        if page_break(self.used, length, receive_page_bytes(), self.page.is_empty()) {
            self.store_page(store, offered, self.current)?;
        }
        self.used = self.used.saturating_add(length);
        self.page.push(change);
        Ok(())
    }
    fn store_page(&mut self, store: &PersistentStore, offered: &mut Vec<String>, cursor: DecimalU64) -> Result<()> {
        let request = StageReceive {
            header: Header { binding_authority: self.authority, request_id: format!("{}-{}", self.id, self.index) },
            changes: std::mem::take(&mut self.page),
            progress: Progress { cursor, ..self.progress.clone() },
            admitted_time_upper_ms: self.admitted_time_upper_ms,
        };
        self.used = 0;
        self.index += 1;
        offered.extend(store.external_lww_stable_receive(self.target, &request).map_err(store_error)?);
        Ok(())
    }
    fn finish(mut self, store: &PersistentStore, offered: &mut Vec<String>) -> Result<()> {
        let cursor = self.progress.cursor;
        self.store_page(store, offered, cursor)
    }
}
/// Segment names by writer and sequence. A sequence listed under two names is
/// refused from the names alone.
pub(crate) fn segment_groups(objects: Vec<ObjectReceipt>) -> Result<BTreeMap<(String, u64), (String, ObjectReceipt)>> {
    let mut groups = BTreeMap::new();
    for object in objects {
        let name = object.locator.object.rsplit('/').next().ok_or_else(segment::corrupt)?;
        let (writer, seq, hash) = parse_segment_object_id(name)?;
        let hash = hash.to_owned();
        match groups.entry((writer.to_owned(), seq)) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((hash, object));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                if entry.get().0 != hash {
                    return Err(segment::corrupt());
                }
            }
        }
    }
    Ok(groups)
}
/// The sequence each writer reaches from `current` through consecutive names.
fn reachable(current: &BTreeMap<String, DecimalU64>, groups: &BTreeMap<(String, u64), (String, ObjectReceipt)>) -> BTreeMap<String, u64> {
    let mut reached = current.iter().map(|(writer, cursor)| (writer.clone(), cursor.0)).collect::<BTreeMap<_, _>>();
    for (writer, seq) in groups.keys() {
        let prefix = reached.entry(writer.clone()).or_insert(0);
        if prefix.checked_add(1) == Some(*seq) {
            *prefix = *seq;
        }
    }
    reached
}
fn behind(covered: &BTreeMap<String, DecimalU64>, reached: &BTreeMap<String, u64>) -> bool {
    covered.iter().any(|(writer, prefix)| reached.get(writer).copied().unwrap_or(0) < prefix.0)
}
/// What cleanup reads from a segment, kept so it need not download the
/// segment again.
pub(crate) fn segment_references(byte_length: u64, document: &Segment) -> Result<SegmentReferences> {
    let plaintext = document.encode()?;
    Ok(SegmentReferences {
        byte_length,
        plaintext_length: plaintext.len() as u64,
        plaintext_sha256: segment::digest(&plaintext),
        newest_physical_ms: document.changes.iter().map(|change| change.stamp.physical_ms.0).max().unwrap_or(0),
        data_catalogs: document.data_catalogs.clone(),
        asset_catalogs: document.asset_catalogs.clone(),
        large_bodies: document.large_bodies.clone(),
    })
}
/// A captured body read once while it is hashed and counted.
struct CheckedBody<'a, R> {
    input: R,
    digest: Sha256,
    length: u64,
    check: &'a mut dyn FnMut() -> std::io::Result<()>,
}
impl<R: std::io::Read> std::io::Read for CheckedBody<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        (self.check)()?;
        let read = std::io::Read::read(&mut self.input, buffer)?;
        self.digest.update(&buffer[..read]);
        self.length = self.length.saturating_add(read as u64);
        Ok(read)
    }
}
/// A sealed body file hashed as it is written.
struct HashingFile {
    file: std::fs::File,
    digest: Sha256,
    length: u64,
}
impl std::io::Write for HashingFile {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = std::io::Write::write(&mut self.file, buffer)?;
        self.digest.update(&buffer[..written]);
        self.length = self.length.saturating_add(written as u64);
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::Write::flush(&mut self.file)
    }
}

/// The length of a segment under assembly, kept per entry.
#[derive(Clone, Copy, Default)]
struct AssemblyLength {
    fixed: usize,
    pages: usize,
    page_bytes: usize,
    small_assets: usize,
    placeholders: usize,
}
impl AssemblyLength {
    fn total(&self, controls: bool, locator: usize) -> Result<usize> {
        let catalogs = usize::from(controls) + usize::from(self.small_assets > 0);
        self.fixed
            .checked_add(self.pages)
            .ok_or_else(segment::corrupt)?
            .checked_add(reserved_length(catalogs, self.placeholders, locator)?)
            .ok_or_else(segment::corrupt)
    }
}
fn reserved_length(catalogs: usize, placeholders: usize, locator: usize) -> Result<usize> {
    risunest_external_storage_format::snapshot::MAX_METADATA_BYTES
        .checked_mul(catalogs)
        .zip(locator.checked_mul(placeholders))
        .and_then(|(catalogs, bodies)| catalogs.checked_add(bodies))
        .ok_or_else(segment::corrupt)
}
fn admits(length: usize) -> Result<bool> {
    match segment::sealed_length(length) {
        Ok(_) => Ok(true),
        Err(error) if error.kind == ErrorKind::FileTooLarge => Ok(false),
        Err(error) => Err(error),
    }
}
fn move_pages(payload: &mut Segment, controls: &mut BTreeMap<String, String>, length: &mut AssemblyLength, moved: &mut Vec<String>) {
    for (hash, page) in std::mem::take(&mut payload.message_pages) {
        moved.push(hash.clone());
        controls.insert(hash, page);
    }
    length.pages = 0;
    length.page_bytes = 0;
}
/// Control bodies written to disk while a publication is assembled. The
/// directory goes away unless the publication is persisted.
struct StagedControls {
    root: std::path::PathBuf,
    created: bool,
}
impl StagedControls {
    fn new(root: std::path::PathBuf) -> Self {
        Self { root, created: false }
    }
    fn freeze(&mut self, store: &PersistentStore, hash: &str, encoded: &str, cancel: &Cancellation) -> Result<FrozenControl> {
        cancel.check()?;
        let directory = self.root.join("controls");
        if !self.created {
            std::fs::create_dir_all(&directory).map_err(transient)?;
            self.created = true;
        }
        let source = directory.join(hash);
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&source).map_err(transient)?;
        let byte_length = if encoded.is_empty() {
            use std::io::{Read, Write};
            use sha2::{Digest, Sha256};
            let mut input = store.lww_native_object(hash).map_err(store_error)?.ok_or_else(segment::corrupt)?;
            let mut digest = Sha256::new();
            let mut length = 0u64;
            let mut buffer = [0; 64 * 1024];
            loop {
                cancel.check()?;
                let count = input.read(&mut buffer).map_err(transient)?;
                if count == 0 { break; }
                file.write_all(&buffer[..count]).map_err(transient)?;
                digest.update(&buffer[..count]); length += count as u64;
            }
            if hex::encode(digest.finalize()) != hash { return Err(segment::corrupt()); }
            length
        } else {
            let bytes = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| segment::corrupt())?;
            std::io::Write::write_all(&mut file, &bytes).map_err(transient)?;
            bytes.len() as u64
        };
        file.sync_all().map_err(transient)?;
        Ok(FrozenControl { content_hash: hash.into(), byte_length, source })
    }
    fn keep(&mut self) {
        self.created = false;
    }
}
impl Drop for StagedControls {
    fn drop(&mut self) {
        if self.created {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
}

/// What the repository holds for a segment this device sent.
enum Settlement {
    /// Nothing sent is waiting for an answer.
    Nothing,
    Landed,
    Missing,
    Conflict,
}
pub(crate) fn store_error(error: crate::persistent_store::StoreError) -> ProviderError {
    segment::corrupt().caused(&error)
}
fn transient(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient).caused(&error)
}
pub(crate) struct BytesSource(pub Vec<u8>);
impl TransferSource for BytesSource {
    fn byte_length(&self) -> u64 {
        self.0.len() as u64
    }
    fn open<'a>(
        &'a self,
        offset: u64,
        length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncRead + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            let start = usize::try_from(offset).map_err(|_| segment::corrupt())?;
            let end = usize::try_from(offset.checked_add(length).ok_or_else(segment::corrupt)?)
                .map_err(|_| segment::corrupt())?;
            let bytes = self
                .0
                .get(start..end)
                .ok_or_else(segment::corrupt)?
                .to_vec();
            Ok(Box::pin(Cursor::new(bytes)) as Pin<Box<dyn AsyncRead + Send>>)
        })
    }
}
struct BytesSink {
    bytes: Arc<std::sync::Mutex<Vec<u8>>>,
    verified: bool,
}
struct BytesWriter(Arc<std::sync::Mutex<Vec<u8>>>);
impl AsyncWrite for BytesWriter {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.0.lock() {
            Ok(mut out) => {
                out.extend_from_slice(bytes);
                std::task::Poll::Ready(Ok(bytes.len()))
            }
            Err(_) => std::task::Poll::Ready(Err(std::io::Error::other("receive lock"))),
        }
    }
    fn poll_flush(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        self: Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
impl TransferSink for BytesSink {
    fn open<'a>(
        &'a mut self,
        offset: u64,
        _length: u64,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Pin<Box<dyn AsyncWrite + Send>>> {
        Box::pin(async move {
            cancel.check()?;
            if offset != 0 {
                return Err(segment::corrupt());
            }
            self.bytes.lock().map_err(transient)?.clear();
            Ok(Box::pin(BytesWriter(self.bytes.clone())) as Pin<Box<dyn AsyncWrite + Send>>)
        })
    }
    fn finish<'a>(&'a mut self, length: u64, hash: &'a str) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let bytes = self.bytes.lock().map_err(transient)?;
            if bytes.len() as u64 != length || segment::digest(&bytes) != hash {
                return Err(segment::corrupt());
            }
            self.verified = true;
            Ok(())
        })
    }
}
pub(crate) async fn read_bytes(
    provider: &dyn Provider,
    repository: &RepositoryHandle,
    locator: &RemoteLocator,
    cancel: &Cancellation,
) -> Result<Vec<u8>> {
    let mut sink = BytesSink {
        bytes: Arc::new(std::sync::Mutex::new(Vec::new())),
        verified: false,
    };
    if !matches!(
        provider
            .read_object(repository, locator, None, &mut sink, cancel)
            .await?,
        ReadReceipt::Body(_)
    ) || !sink.verified
    {
        return Err(segment::corrupt());
    }
    let bytes = sink.bytes.lock().map_err(transient)?.clone();
    Ok(bytes)
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct Admission {
    pub upper_ms: u64,
    at: Instant,
    wall_ms: u64,
    #[cfg(test)]
    reading: Option<(u64, u64)>,
}
impl Admission {
    pub(crate) fn from_sample(sample: &HttpClockSample) -> Result<Self> {
        sample.validate()?;
        let age = sample.observed_at.elapsed().as_millis();
        if age > u128::from(super::clock::SAMPLE_LIFETIME_MS) {
            return Err(super::clock::failure("admission", "sample-expired",
                format_args!("sample_age_ms={age} limit_ms={}", super::clock::SAMPLE_LIFETIME_MS)));
        }
        let upper = sample
            .date_ms
            .ok_or_else(segment::corrupt)?
            .checked_add(1000)
            .and_then(|v| v.checked_add(sample.round_trip_ms.div_ceil(2)))
            .ok_or_else(segment::corrupt)?;
        Ok(Self {
            upper_ms: upper,
            at: sample.observed_at,
            wall_ms: sample.local_after_ms,
            #[cfg(test)]
            reading: None,
        })
    }
    #[cfg(test)]
    pub(crate) fn synthetic(now: u64) -> Self {
        Self {
            upper_ms: now,
            at: Instant::now(),
            wall_ms: now,
            reading: Some((now, 0)),
        }
    }
    fn upper(self) -> Result<u64> {
        #[cfg(test)]
        if let Some((now, elapsed)) = self.reading {
            return self.upper_at(now, elapsed);
        }
        let elapsed = u64::try_from(self.at.elapsed().as_millis()).map_err(transient)?;
        let now = super::runtime::now_ms();
        self.upper_at(now, elapsed)
    }
    fn upper_at(self, now: u64, elapsed: u64) -> Result<u64> {
        if elapsed > super::clock::SAMPLE_LIFETIME_MS {
            return Err(super::clock::failure("admission", "sample-expired",
                format_args!("sample_age_ms={elapsed} limit_ms={}", super::clock::SAMPLE_LIFETIME_MS)));
        }
        let delta = super::clock::movement(self.wall_ms, now, elapsed);
        if delta.abs() > i128::from(super::clock::MAX_DISCONTINUITY_MS) {
            return Err(super::clock::failure("admission", "wall-clock-jump",
                format_args!("delta_ms={delta} limit_ms={} sample_age_ms={elapsed}", super::clock::MAX_DISCONTINUITY_MS)));
        }
        self.upper_ms
            .checked_add(elapsed)
            .ok_or_else(segment::corrupt)
    }
    #[cfg(test)]
    pub(crate) fn age_for_test(&mut self, elapsed: Duration) {
        self.at -= elapsed;
        self.wall_ms -= elapsed.as_millis() as u64;
        self.upper_ms -= elapsed.as_millis() as u64;
        if let Some((_, age)) = self.reading.as_mut() {
            *age += elapsed.as_millis() as u64;
        }
    }
    #[cfg(test)]
    pub(crate) fn set_reading_for_test(&mut self, wall_ms: u64, elapsed_ms: u64) {
        self.reading = Some((wall_ms, elapsed_ms));
    }
}
#[cfg(test)]
mod admission_clock_tests {
    use super::*;

    #[test]
    fn synthetic_admission_uses_controlled_wall_and_elapsed_time() {
        let mut admission = Admission::synthetic(1_700_000_000_000);
        assert_eq!(admission.upper().unwrap(), 1_700_000_000_000);
        admission.set_reading_for_test(1_700_000_060_000, 60_000);
        assert_eq!(admission.upper().unwrap(), 1_700_000_060_000);
        admission.age_for_test(Duration::from_secs(16 * 60));
        assert_eq!(admission.upper().unwrap_err().kind, ErrorKind::ClockSkew);
    }

    #[test]
    fn admission_rejects_forward_and_backward_jumps_beyond_five_seconds() {
        let start = 1_700_000_000_000;
        for (offset, accepted) in [(-5001_i64, false), (-5000, true), (-4999, true), (0, true), (4999, true), (5000, true), (5001, false)] {
            let mut admission = Admission::synthetic(start);
            admission.set_reading_for_test((start as i64 + 60_000 + offset) as u64, 60_000);
            let result = admission.upper();
            if accepted {
                assert_eq!(result.unwrap(), start + 60_000, "{offset}");
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind, ErrorKind::ClockSkew, "{offset}");
                let detail = error.cause.0.unwrap();
                assert!(detail.contains("stage=admission reason=wall-clock-jump"));
                assert!(detail.contains(&format!("delta_ms={offset}")));
            }
        }
        let mut admission = Admission::synthetic(start);
        admission.set_reading_for_test(start - 4900, 100);
        assert_eq!(admission.upper().unwrap(), start + 100);
    }

    #[test]
    fn admission_expires_only_after_fifteen_minutes() {
        let start = 1_700_000_000_000;
        let mut admission = Admission::synthetic(start);
        admission.set_reading_for_test(start + 900_000, 900_000);
        assert_eq!(admission.upper().unwrap(), start + 900_000);
        admission.set_reading_for_test(start + 900_001, 900_001);
        assert_eq!(admission.upper().unwrap_err().kind, ErrorKind::ClockSkew);
    }

    #[test]
    fn synthetic_cycles_do_not_share_lease_clock_trust() {
        let mut first = super::super::lww_tests::CycleFixture::new();
        let second = super::super::lww_tests::CycleFixture::new();
        let clock = Arc::new(super::super::fake::FakeLeaseClock::new(1_700_000_000_000));
        first.sender.test_lease_clock = Some(clock.clone());
        assert_eq!(first.sender.trusted_control_time(), Some(1_700_000_000_000));
        clock.set_trusted(false);
        assert_eq!(first.sender.trusted_control_time(), None);
        assert!(second.sender.trusted_control_time().is_some());
        clock.set_trusted(true);
        clock.advance(60_000);
        assert_eq!(first.sender.trusted_control_time(), Some(1_700_000_060_000));
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicationResult {
    pub segments: DecimalU64,
    pub units: DecimalU64,
    pub bytes: DecimalU64,
    pub more: bool,
}
/// The receive pages one listing produced.
#[derive(Debug)]
pub(crate) struct ReceiveBatch {
    /// Ids of the unfinished pages in order.
    pub pages: Vec<String>,
    /// The listing holds segments after the ones this batch stopped at.
    pub more: bool,
}
impl Default for PublicationResult {
    fn default() -> Self {
        Self {
            segments: DecimalU64(0),
            units: DecimalU64(0),
            bytes: DecimalU64(0),
            more: false,
        }
    }
}
#[cfg(test)]
pub(crate) mod cycle_keys {
    use risunest_sync_wire::unit::UnitKey;
    use std::{cell::RefCell, collections::BTreeSet};

    #[derive(Debug, Default)]
    pub(crate) struct CycleKeys {
        pub selected_keys: BTreeSet<UnitKey>,
        pub attempted_keys: BTreeSet<UnitKey>,
        pub emitted_keys: BTreeSet<UnitKey>,
        pub affected_keys: BTreeSet<UnitKey>,
        pub held_keys: BTreeSet<UnitKey>,
        pub deferred_keys: BTreeSet<UnitKey>,
        pub segment_attempts: usize,
        pub accepted_publications: usize,
        pub receive_applies: usize,
        pub failed_operations: usize,
        pub pending_operations: usize,
        pub buffered_control_bytes: usize,
    }
    impl CycleKeys {
        pub(crate) fn complete(&self) -> bool {
            self.failed_operations == 0 && self.pending_operations == 0
        }
    }
    thread_local! {
        static WORK: RefCell<CycleKeys> = RefCell::new(CycleKeys::default());
    }
    pub(crate) fn reset() {
        WORK.with(|work| {
            let mut work = work.borrow_mut();
            assert_eq!(work.pending_operations, 0, "cycle-key operation is still running");
            *work = CycleKeys::default();
        });
    }
    pub(crate) fn take() -> CycleKeys {
        WORK.with(|work| {
            let mut work = work.borrow_mut();
            let result = std::mem::take(&mut *work);
            work.pending_operations = result.pending_operations;
            result
        })
    }
    pub(super) fn observe(observe: impl FnOnce(&mut CycleKeys)) {
        WORK.with(|work| observe(&mut work.borrow_mut()));
    }
    pub(super) struct Operation {
        finished: bool,
    }
    impl Operation {
        pub(super) fn start() -> Self {
            observe(|work| work.pending_operations += 1);
            Self { finished: false }
        }
        pub(super) fn finish(mut self) {
            self.finished = true;
        }
    }
    impl Drop for Operation {
        fn drop(&mut self) {
            observe(|work| {
                work.pending_operations -= 1;
                if !self.finished {
                    work.failed_operations += 1;
                }
            });
        }
    }
}
pub(crate) struct ExternalLwwEngine {
    pub provider: Arc<dyn Provider>,
    pub repository: RepositoryHandle,
    pub library: String,
    pub root_key: zeroize::Zeroizing<[u8; 32]>,
    pub admission: Option<Admission>,
    #[cfg(test)]
    pub test_lease_clock: Option<Arc<dyn super::leases::LeaseClock>>,
    pub connection_id: String,
    pub connection_root: std::path::PathBuf,
    pub capabilities: super::capabilities::Capabilities,
    pub descriptor: risunest_external_storage_format::format::Descriptor,
}
impl ExternalLwwEngine {
    pub(crate) fn target_scope(&self) -> String {
        format!("{}:{}", self.library, self.repository.connection_identity)
    }
    pub(crate) fn admit(&mut self, sample: &HttpClockSample) -> Result<()> {
        self.admission = Some(Admission::from_sample(sample)?);
        Ok(())
    }
    pub(crate) fn invalidate_clock(&mut self) {
        self.admission = None;
    }
    pub(crate) fn admitted_upper(&self) -> Result<u64> {
        self.admission
            .ok_or_else(|| super::clock::failure("admission", "sample-missing", ""))?
            .upper()
    }
    pub(super) fn intent(&self, id: &str, role: ObjectRole, bytes: &[u8]) -> ObjectIntent {
        ObjectIntent {
            job_id: self.library.clone(),
            repository_id: self.repository.repository_id.clone(),
            object_id: id.into(),
            role,
            byte_length: bytes.len() as u64,
            sha256: segment::digest(bytes),
        }
    }
    fn resume(state: &UploadState) -> ResumeState {
        ResumeState {
            data: state.data.clone(),
            confirmed_offset: state.confirmed_offset.0,
            expires_at_ms: state.expires_at_ms.map(|v| v.0),
        }
    }
    fn saved(state: ResumeState) -> UploadState {
        UploadState {
            data: state.data,
            confirmed_offset: DecimalU64(state.confirmed_offset),
            expires_at_ms: state.expires_at_ms.map(DecimalU64),
        }
    }
    #[cfg(test)]
    pub(crate) async fn publish(
        &mut self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        generating: &[MessageLocator],
        cancel: &Cancellation,
    ) -> Result<PublicationResult> {
        self.publish_limited(store, authority, generating, None, cancel).await
    }
    pub(crate) async fn publish_limited(
        &mut self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        generating: &[MessageLocator],
        turn_limit: Option<u32>,
        cancel: &Cancellation,
    ) -> Result<PublicationResult> {
        let mut result = PublicationResult::default();
        self.publish_accumulating(store, authority, generating, turn_limit, cancel, &mut result).await?;
        Ok(result)
    }
    pub(crate) async fn publish_accumulating(
        &mut self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        generating: &[MessageLocator],
        turn_limit: Option<u32>,
        cancel: &Cancellation,
        result: &mut PublicationResult,
    ) -> Result<()> {
        self.publish_segments(store, authority, generating, turn_limit, false, cancel, result).await
    }
    /// Publishes a segment that holds only the format level unit, after any
    /// segment already captured.
    pub(crate) async fn publish_format_level(
        &mut self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        cancel: &Cancellation,
    ) -> Result<PublicationResult> {
        let mut result = PublicationResult::default();
        self.publish_segments(store, authority, &[], None, true, cancel, &mut result).await?;
        Ok(result)
    }
    /// The format level unit every segment carries, so a segment declares the
    /// level its content needs. Its stamp stays within the bound receivers
    /// admit, as the stamps of the entries beside it do.
    fn format_level_change(store: &mut PersistentStore, upper: u64) -> Result<Change> {
        let mut stamp = store.lww_reserve_stamp().map_err(store_error)?;
        if stamp.physical_ms.0 > upper {
            stamp = Stamp { physical_ms: DecimalU64(upper), logical: 0, writer_id: stamp.writer_id };
        }
        Ok(Change {
            key: library_format::key(),
            stamp,
            value: library_format::value(crate::server_sync::library_format::LEVEL).map_err(|_| segment::corrupt())?,
        })
    }
    /// Stops before anything in `changes` is used when they declare a format
    /// level above this app's, and records that level so sync stays stopped
    /// until the app is updated.
    pub(crate) fn require_readable_format(&self, changes: &[Change]) -> Result<()> {
        let level = changes
            .iter()
            .filter(|change| library_format::is_key(&change.key))
            .filter_map(|change| library_format::level(&change.value))
            .max();
        match level {
            Some(level) if level > crate::server_sync::library_format::LEVEL => {
                super::connection_store::ConnectionStore::open(&self.connection_root)?
                    .record_format_level(&self.connection_id, level)?;
                Err(ProviderError::new(ErrorKind::AppUpdateRequired))
            }
            _ => Ok(()),
        }
    }
    #[allow(clippy::too_many_arguments)]
    async fn publish_segments(
        &mut self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        generating: &[MessageLocator],
        turn_limit: Option<u32>,
        level_only: bool,
        cancel: &Cancellation,
        result: &mut PublicationResult,
    ) -> Result<()> {
        let mut level_built = false;
        if turn_limit == Some(0) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        #[cfg(test)]
        let key_operation = cycle_keys::Operation::start();
        let writer = store.lww_clock_state().map_err(store_error)?.writer_id;
        loop {
            cancel.check()?;
            let upper = self
                .admitted_upper()?
                .checked_add(300_000)
                .ok_or_else(segment::corrupt)?;
            let pending = store
                .external_lww_pending(&self.target_scope(), &writer)
                .map_err(store_error)?;
            let (publication, sealed) = match pending {
                Some(value) => value,
                None => {
                    let entries = if level_only {
                        if std::mem::replace(&mut level_built, true) {
                            break;
                        }
                        Vec::new()
                    } else {
                        store
                            .lww_read_outbox_generating(authority, 4096, generating)
                            .map_err(store_error)?
                            .entries
                    };
                    if entries.is_empty() && !level_only {
                        break;
                    }
                    if entries.iter().any(|e| e.stamp.physical_ms.0 > upper) {
                        self.repair_unsent(store, authority, &entries)?;
                        continue;
                    }
                    let seq = store
                        .external_lww_next_sequence(&self.target_scope(), &writer)
                        .map_err(store_error)?;
                    let mut payload = Segment::new(&self.library, &writer, seq);
                    let empty = payload.encode_capture()?.len();
                    payload.changes.push(Self::format_level_change(store, upper)?);
                    let reserve = Self::locator_reserve(&self.repository)?;
                    let job_id = uuid::Uuid::new_v4().to_string();
                    let mut staged = StagedControls::new(
                        store.repository_root().join("external-storage").join("lww-publications").join(&job_id),
                    );
                    let mut length = AssemblyLength { fixed: payload.encode_capture()?.len(), ..Default::default() };
                    let mut selected = Vec::new();
                    let mut bodies = Vec::new();
                    let mut assets = BTreeMap::new();
                    let mut controls = BTreeMap::new();
                    let mut frozen_controls = BTreeMap::new();
                    let mut reused_controls = BTreeMap::new();
                    let mut reused_assets = BTreeMap::new();
                    let mut remote_bodies = super::lww_residency::RemoteBodies::deferred(store.repository_root());
                    let mut server_bodies = None;
                    for entry in entries {
                        // The level unit the segment carries stands for a queued one.
                        if library_format::is_key(&entry.key) {
                            selected.push(entry);
                            continue;
                        }
                        let before = length;
                        let marks = (
                            payload.changes.len(), payload.message_pages.len(), payload.data_catalogs.len(),
                            payload.asset_catalogs.len(), payload.large_bodies.len(), bodies.len(),
                        );
                        let fresh = self.include_objects(
                            store,
                            &entry.key,
                            &entry.value,
                            &mut payload,
                            &mut bodies,
                            &mut assets,
                            &mut controls,
                            &mut reused_controls,
                            &mut reused_assets,
                            &mut remote_bodies,
                            &mut server_bodies,
                            cancel,
                        )?;
                        // Earlier entries are never encoded again: the entry's own
                        // items are encoded alone and joined to the running length.
                        let mut delta = Segment::new(&self.library, &writer, seq);
                        delta.changes.push(Change {
                            key: entry.key.clone(),
                            stamp: entry.stamp.clone(),
                            value: entry.value.clone(),
                        });
                        delta.data_catalogs.extend_from_slice(&payload.data_catalogs[marks.2..]);
                        delta.asset_catalogs.extend_from_slice(&payload.asset_catalogs[marks.3..]);
                        let mut pages = Segment::new(&self.library, &writer, seq);
                        for hash in &fresh {
                            if let Some(body) = payload.large_bodies.get(hash) {
                                delta.large_bodies.insert(hash.clone(), body.clone());
                            }
                            if let Some(page) = payload.message_pages.get(hash) {
                                pages.message_pages.insert(hash.clone(), page.clone());
                            }
                            if assets.get(hash).is_some_and(|asset: &FrozenAsset| asset.byte_length <= segment::SMALL_BODY_BYTES as u64) {
                                length.small_assets += 1;
                            }
                        }
                        length.placeholders += delta.large_bodies.values().filter(|body| body.locator.is_none()).count();
                        let joined = |count: usize, added: usize| usize::from(count > 0 && added > 0);
                        length.fixed = (delta.encode_capture()?.len() - empty
                            + joined(marks.0, 1)
                            + joined(marks.2, delta.data_catalogs.len())
                            + joined(marks.3, delta.asset_catalogs.len())
                            + joined(marks.4, delta.large_bodies.len()))
                            .checked_add(length.fixed).ok_or_else(segment::corrupt)?;
                        if !pages.message_pages.is_empty() {
                            length.pages = (pages.encode_capture()?.len() - empty + joined(marks.1, pages.message_pages.len()))
                                .checked_add(length.pages).ok_or_else(segment::corrupt)?;
                            length.page_bytes = pages.message_pages.values().try_fold(length.page_bytes, |total, body| {
                                total.checked_add(body.len().checked_mul(3).ok_or_else(segment::corrupt)? / 4).ok_or_else(segment::corrupt)
                            })?;
                        }
                        payload.changes.append(&mut delta.changes);
                        let mut moved = Vec::new();
                        if length.page_bytes > segment::SMALL_BODY_BYTES {
                            move_pages(&mut payload, &mut controls, &mut length, &mut moved);
                        }
                        let mut fits = admits(length.total(!controls.is_empty(), reserve)?)?;
                        if !fits && !payload.message_pages.is_empty() {
                            move_pages(&mut payload, &mut controls, &mut length, &mut moved);
                            fits = admits(length.total(!controls.is_empty(), reserve)?)?;
                        }
                        if !fits {
                            for hash in moved {
                                if let Some(page) = controls.remove(&hash) {
                                    payload.message_pages.insert(hash, page);
                                }
                            }
                            for hash in &fresh {
                                payload.message_pages.remove(hash);
                                controls.remove(hash);
                                assets.remove(hash);
                                reused_assets.remove(hash);
                                payload.large_bodies.remove(hash);
                            }
                            payload.changes.truncate(marks.0);
                            for catalog in payload.data_catalogs.drain(marks.2..) {
                                reused_controls.remove(&hex::encode(catalog.ciphertext_sha256));
                            }
                            payload.asset_catalogs.truncate(marks.3);
                            bodies.truncate(marks.5);
                            length = before;
                            if selected.is_empty() {
                                return Err(ProviderError::new(ErrorKind::FileTooLarge));
                            }
                            break;
                        }
                        for hash in fresh.iter().chain(&moved) {
                            if frozen_controls.contains_key(hash) {
                                continue;
                            }
                            if let Some(encoded) = controls.get_mut(hash) {
                                let control = staged.freeze(store, hash, &std::mem::take(encoded), cancel)?;
                                frozen_controls.insert(hash.clone(), control);
                            }
                        }
                        selected.push(entry);
                        #[cfg(test)]
                        cycle_keys::observe(|work| work.buffered_control_bytes = work.buffered_control_bytes.max(controls.values().map(String::len).sum()));
                    }
                    let plaintext = payload.encode_capture()?;
                    let captured = plaintext.len().checked_add(Self::sealing_reserve(&payload, &controls, &assets, &self.repository)?)
                        .ok_or_else(segment::corrupt)?;
                    debug_assert_eq!(Some(captured), length.total(!controls.is_empty(), reserve).ok());
                    segment::sealed_length(captured).map_err(|_| segment::corrupt())?;
                    let payload_sha256 = segment::digest(&plaintext);
                    let bytes = Vec::new();
                    let sha256 = String::new();
                    let object_id = String::new();
                    let assets = assets.into_values().collect::<Vec<_>>();
                    let (asset_job, mut asset_pins) = if assets.is_empty() && controls.is_empty() && reused_controls.is_empty() && reused_assets.is_empty() {
                        (None, None)
                    } else {
                        let job = super::journal::JobIdentity {
                            job_id,
                            connection_id: self.connection_id.clone(),
                            repository_id: self.repository.repository_id.clone(),
                            capture_id: format!("lww:{writer}:{seq}:{payload_sha256}"),
                            capture: store.external_identity().map_err(store_error)?,
                        };
                        let root = store.repository_root().to_owned();
                        let cas = crate::asset_repository::PayloadCas::new(&root).map_err(transient)?;
                        let mut pins = crate::asset_repository::job_pins::DurableCasJob::begin(
                            &root, &job.job_id,
                            crate::asset_repository::job_pins::CasJobKind::OfficialPublicationOrExportPreparation,
                            crate::asset_repository::job_pins::CasJobOwner::external_publication(&job.job_id),
                            super::runtime::now_ms() as i64,
                        ).map_err(transient)?;
                        let sealed = pins.pin_existing_batch(&cas, &assets.iter().filter(|asset| asset.local_pin).map(|asset| (
                            asset.content_hash.clone(), asset.byte_length,
                            crate::asset_repository::job_pins::CasObjectRole::DirectObject,
                        )).collect::<Vec<_>>()).and_then(|()| pins.seal(store, super::runtime::now_ms() as i64));
                        if let Err(error) = sealed {
                            let _ = pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted);
                            return Err(transient(error));
                        }
                        (Some(job), Some(pins))
                    };
                    let frozen_controls = frozen_controls.into_values().collect::<Vec<_>>();
                    let publication = SealedPublication {
                        target: self.target_scope(),
                        writer: writer.clone(),
                        seq: DecimalU64(seq),
                        authority,
                        captured_at_ms: self.trusted_control_time(),
                        object_id,
                        sha256,
                        payload_sha256,
                        payload: URL_SAFE_NO_PAD.encode(plaintext),
                        sealed: false,
                        entries: selected,
                        bodies,
                        assets,
                        controls: frozen_controls,
                        reused_control_catalogs: reused_controls.into_values().collect(),
                        reused_assets: reused_assets.into_values().collect(),
                        asset_job,
                        data_catalogs: Vec::new(),
                        asset_catalogs: Vec::new(),
                        resume: None,
                        dispatched: false,
                        complete: false,
                    };
                    if let Err(error) = store.external_lww_persist(&publication, &bytes) {
                        if let Some(pins) = asset_pins.as_mut() {
                            let _ = pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted);
                        }
                        return Err(store_error(error));
                    }
                    drop(asset_pins);
                    staged.keep();
                    (publication, bytes)
                }
            };
            if publication.authority != authority {
                if store.lww_binding_authority().map_err(store_error)? != authority {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                self.settle_detached(store, publication, sealed, cancel).await?;
                continue;
            }
            if let Some(future) = publication
                .entries
                .iter()
                .find(|e| e.stamp.physical_ms.0 > upper)
            {
                if publication.dispatched {
                    return Err(super::clock::future_change("publish", future.stamp.physical_ms.0, upper));
                }
                self.repair_unsent(store, authority, &publication.entries)?;
                store
                    .external_lww_abandon_unsent(&self.target_scope(), &writer)
                    .map_err(store_error)?;
                if let Some(job) = &publication.asset_job {
                    crate::asset_repository::job_pins::DurableCasJob::open(store.repository_root(), &job.job_id)
                        .and_then(|mut pins| pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Aborted))
                        .map_err(transient)?;
                }
                continue;
            }
            #[cfg(test)]
            cycle_keys::observe(|work| {
                work.selected_keys.extend(publication.entries.iter().map(|entry| entry.key.clone()));
            });
            let completed = if let Some(job) = &publication.asset_job {
                let protection = super::leases::LeaseContext {
                    root: &self.connection_root,
                    connection_id: &self.connection_id,
                    writer_id: &writer,
                    descriptor: &self.descriptor,
                    root_key: &self.root_key,
                    provider: self.provider.as_ref(),
                    repository: &self.repository,
                    clock: self.lease_clock(),
                    protection_supported: self.capabilities.lease_operations,
                    ledger: None,
                };
                let owner = match super::leases::admit_shared_work(&protection, &job.job_id, cancel).await? {
                    super::leases::Admission::Admitted(owner) => owner,
                    super::leases::Admission::Yield { reason } => return Err(super::leases::yield_error(reason)),
                    super::leases::Admission::UnsupportedProtection => return Err(ProviderError::new(ErrorKind::Unsupported)),
                };
                owner.run(&protection, cancel, self.publish_captured(store, publication, sealed, cancel, Some((&owner, &protection)))).await?
            } else {
                self.publish_captured(store, publication, sealed, cancel, None).await?
            };
            result.segments.0 += completed.segments.0;
            result.units.0 += completed.units.0;
            result.bytes.0 += completed.bytes.0;
            if turn_limit.is_some_and(|limit| result.segments.0 >= u64::from(limit)) {
                break;
            }
        }
        result.more = !store.lww_read_outbox(authority, 1).map_err(store_error)?.entries.is_empty();
        #[cfg(test)]
        key_operation.finish();
        Ok(())
    }
    /// The plaintext length a captured segment can reach once sealing adds its
    /// catalogs and fills its large-body placeholders.
    #[cfg(test)]
    pub(super) fn capture_length(payload:&Segment,controls:&BTreeMap<String,String>,assets:&BTreeMap<String,FrozenAsset>,repository:&RepositoryHandle)->Result<usize> {
        payload.encode_capture()?.len().checked_add(Self::sealing_reserve(payload,controls,assets,repository)?).ok_or_else(segment::corrupt)
    }
    fn sealing_reserve(payload:&Segment,controls:&BTreeMap<String,String>,assets:&BTreeMap<String,FrozenAsset>,repository:&RepositoryHandle)->Result<usize> {
        let catalogs=usize::from(!controls.is_empty())+usize::from(assets.values().any(|asset|asset.byte_length<=segment::SMALL_BODY_BYTES as u64));
        let placeholders=payload.large_bodies.values().filter(|body|body.locator.is_none()).count();
        reserved_length(catalogs,placeholders,Self::locator_reserve(repository)?)
    }
    /// What filling one large-body placeholder can add: the longest locator
    /// `seal` accepts, the sealed digest and the sealed length.
    fn locator_reserve(repository:&RepositoryHandle)->Result<usize> {
        let placeholder=LargeBody{object_id:String::new(),sha256:"0".repeat(64),byte_length:DecimalU64(0),plaintext_byte_length:DecimalU64(0),locator:None};
        let filled=LargeBody{sha256:"f".repeat(64),byte_length:DecimalU64(u64::MAX),locator:Some(RemoteLocator{
            connection_identity:repository.connection_identity.clone(),
            collection:Some("\u{1}".repeat(MAX_LOCATOR_COLLECTION_BYTES)),
            object:"\u{1}".repeat(MAX_LOCATOR_OBJECT_BYTES),
        }),..placeholder.clone()};
        let length=|body:&LargeBody|risunest_sync_wire::canonical::encode(body).map(|bytes|bytes.len()).map_err(|_|segment::corrupt());
        length(&filled)?.checked_sub(length(&placeholder)?).ok_or_else(segment::corrupt)
    }
    async fn publish_captured(
        &self,
        store: &mut PersistentStore,
        mut publication: SealedPublication,
        mut sealed: Vec<u8>,
        cancel: &Cancellation,
        protection: Option<(&super::leases::LeaseOwner, &super::leases::LeaseContext<'_>)>,
    ) -> Result<PublicationResult> {
        let mut result = PublicationResult::default();
        Self::check_publication_authority(store, &publication)?;
        if publication.dispatched {
            if !publication.sealed {return Err(segment::corrupt());}
            self.reconcile_publication(&mut publication,&sealed,cancel).await?;
        }
        if let Some(job) = &publication.asset_job {
            if job.connection_id != self.connection_id || job.repository_id != self.repository.repository_id {
                return Err(segment::corrupt());
            }
            let root = store.repository_root().to_owned();
            let pins = crate::asset_repository::job_pins::DurableCasJob::open(&root, &job.job_id).map_err(transient)?;
            let roots = pins.root_set().map_err(transient)?;
            if publication.assets.iter().filter(|asset| asset.local_pin).any(|asset| !roots.object_hashes.contains(&asset.content_hash)) {
                return Err(segment::corrupt());
            }
            if !publication.complete {
                for proof in &publication.reused_control_catalogs {
                    if self.trusted_control_time().and_then(|now|now.checked_sub(proof.rooted_at_ms))
                        .is_none_or(|age|age>super::leases::CACHE_REUSE_LIMIT_MS) {
                        super::snapshot_restore::revalidate_catalog(
                            &proof.catalog,risunest_external_storage_format::snapshot::CatalogKind::Records,
                            &self.root_key,self.provider.as_ref(),&self.repository,&root,cancel,
                        ).await?;
                    }
                }
                let mut checked_assets=BTreeSet::new();
                for proof in &publication.reused_assets {
                    if self.trusted_control_time().and_then(|now|now.checked_sub(proof.rooted_at_ms))
                        .is_none_or(|age|age>super::leases::CACHE_REUSE_LIMIT_MS) {
                        match &proof.reference {
                            AssetReference::Catalog(catalog)=>{
                                if checked_assets.insert(hex::encode(catalog.ciphertext_sha256)) {
                                    super::snapshot_restore::revalidate_catalog(catalog,risunest_external_storage_format::snapshot::CatalogKind::Assets,
                                        &self.root_key,self.provider.as_ref(),&self.repository,&root,cancel).await?;
                                }
                            }
                            AssetReference::Standalone(body)=>{
                                let intent=ObjectIntent{job_id:self.library.clone(),repository_id:self.repository.repository_id.clone(),
                                    object_id:body.object_id.clone(),role:ObjectRole::Pack,byte_length:body.byte_length.0,sha256:body.sha256.clone()};
                                match self.provider.reconcile_upload(&self.repository,&intent,None,cancel).await? {
                                    UploadResolution::Complete(receipt)=>{
                                        Self::validate_receipt(&intent,&receipt)?;
                                        if body.locator.as_ref()!=Some(&receipt.locator) {return Err(segment::corrupt());}
                                    }
                                    UploadResolution::Conflict=>return Err(segment::corrupt()),
                                    UploadResolution::RestartRequired|UploadResolution::Resumable(_)=>return Err(ProviderError::new(ErrorKind::NotFound)),
                                }
                            }
                        }
                    }
                }
                if self.trusted_control_time().and_then(|now|publication.captured_at_ms.and_then(|captured|now.checked_sub(captured)))
                    .is_none_or(|age|age>super::leases::CACHE_REUSE_LIMIT_MS) {
                    for catalog in &publication.data_catalogs {
                        super::snapshot_restore::revalidate_catalog(catalog,risunest_external_storage_format::snapshot::CatalogKind::Records,
                            &self.root_key,self.provider.as_ref(),&self.repository,&root,cancel).await?;
                    }
                    for catalog in &publication.asset_catalogs {
                        super::snapshot_restore::revalidate_catalog(catalog,risunest_external_storage_format::snapshot::CatalogKind::Assets,
                            &self.root_key,self.provider.as_ref(),&self.repository,&root,cancel).await?;
                    }
                    for body in publication.bodies.iter().filter(|body|body.complete) {
                        let intent=ObjectIntent{job_id:self.library.clone(),repository_id:self.repository.repository_id.clone(),
                            object_id:body.object_id.clone(),role:ObjectRole::Pack,byte_length:body.byte_length,sha256:body.sha256.clone()};
                        let resume=body.resume.as_ref().map(Self::resume);
                        match self.provider.reconcile_upload(&self.repository,&intent,resume.as_ref(),cancel).await? {
                            UploadResolution::Complete(receipt)=>{
                                Self::validate_receipt(&intent,&receipt)?;
                                if body.locator.as_ref()!=Some(&receipt.locator) {return Err(segment::corrupt());}
                            }
                            UploadResolution::Conflict=>return Err(segment::corrupt()),
                            UploadResolution::RestartRequired|UploadResolution::Resumable(_)=>return Err(ProviderError::new(ErrorKind::NotFound)),
                        }
                    }
                }
            }
            if publication.data_catalogs.is_empty() && !publication.controls.is_empty() {
                let mut journal=super::journal::TransferJournal::open(
                    &root.join("external-storage").join("lww-publications").join(&job.job_id).join("data"),job.clone(),
                ).map_err(transient)?;
                journal.set_spool_budget(super::runtime::spool_budget(&root, &job.job_id));
                let _execution = journal.begin_spool_execution();
                let sources=publication.controls.iter().map(|control|super::packaging::AssetCatalogSource {
                    content_hash:control.content_hash.clone(),byte_length:control.byte_length,
                    // Written under the data root of the run that froze it.
                    source:super::content_store::ObjectSource::File(crate::app_paths::relocated_external_storage_path(&control.source,&root)),
                }).collect();
                let completed=super::packaging::package_and_upload_control_catalog(
                    sources,&root,&self.connection_root.join("external-storage").join("package-cache").join(&self.connection_id),
                    &self.root_key,super::packaging::PackageLimits::from_capabilities(&self.capabilities)?,
                    &mut journal,self.provider.as_ref(),&self.repository,
                    &self.provider.preparation_progress(),cancel,protection,
                ).await?;
                publication.data_catalogs.push(completed.catalog);
                store.external_lww_persist(&publication,&sealed).map_err(store_error)?;
            }
            for catalog in &publication.data_catalogs {
                store.external_lww_witness_data_catalog(&self.target_scope(),catalog,
                    &publication.controls.iter().map(|control|control.content_hash.clone()).collect::<Vec<_>>()).map_err(store_error)?;
            }
            if publication.asset_catalogs.is_empty() && publication.assets.iter().any(|asset| asset.byte_length <= segment::SMALL_BODY_BYTES as u64) {
                let mut journal = super::journal::TransferJournal::open(
                    &root.join("external-storage").join("lww-publications").join(&job.job_id),
                    job.clone(),
                ).map_err(transient)?;
                journal.set_spool_budget(super::runtime::spool_budget(&root, &job.job_id));
                let _execution = journal.begin_spool_execution();
                let mut spools = Vec::new();
                let mut sources = Vec::new();
                for asset in publication.assets.iter().filter(|asset| asset.byte_length <= segment::SMALL_BODY_BYTES as u64) {
                    Self::check_publication_authority(store, &publication)?;
                    let source = if let Some(proof) = &asset.remote_source {
                        let spool = super::lww_residency::spool_frozen_remote_body(
                            &proof.clone().rooted(&root), &root.join("external-storage").join("lww-publications").join(&job.job_id), cancel,
                        ).await.map_err(Self::held_body_error)?.into_temp_path();
                        Self::check_publication_authority(store, &publication)?;
                        let source = super::content_store::ObjectSource::File(spool.to_path_buf());
                        spools.push(spool);
                        source
                    } else if asset.server_source.is_some() {
                        let mut body = Self::open_frozen_server_body(store, asset, job, publication.authority, cancel).await?;
                        let scratch = root.join("external-storage").join("lww-publications").join(&job.job_id);
                        let mut spool = tempfile::NamedTempFile::new_in(&scratch).map_err(transient)?;
                        let mut chunk = [0; 64 * 1024];
                        loop {
                            Self::check_frozen_body_authority(store, publication.authority, cancel).map_err(|error| Self::body_source_error(error, cancel))?;
                            let read = std::io::Read::read(&mut body, &mut chunk).map_err(transient)?;
                            if read == 0 { break; }
                            std::io::Write::write_all(&mut spool, &chunk[..read]).map_err(transient)?;
                        }
                        let spool = spool.into_temp_path();
                        let source = super::content_store::ObjectSource::File(spool.to_path_buf());
                        spools.push(spool);
                        source
                    } else { super::content_store::ObjectSource::Library(asset.content_hash.clone()) };
                    sources.push(super::packaging::AssetCatalogSource {
                        content_hash: asset.content_hash.clone(), byte_length: asset.byte_length, source,
                    });
                }
                let completed = super::packaging::package_and_upload_asset_catalog(
                    sources, &root,
                    &self.connection_root.join("external-storage").join("package-cache").join(&self.connection_id),
                    &self.root_key, super::packaging::PackageLimits::from_capabilities(&self.capabilities)?,
                    &mut journal, self.provider.as_ref(), &self.repository,
                    &self.provider.preparation_progress(), cancel, protection,
                ).await?;
                publication.asset_catalogs.push(completed.catalog);
                store.external_lww_persist(&publication, &sealed).map_err(store_error)?;
            }
        } else if !publication.assets.is_empty() || !publication.asset_catalogs.is_empty()
            || !publication.controls.is_empty() || !publication.data_catalogs.is_empty()
            || !publication.reused_assets.is_empty() || !publication.reused_control_catalogs.is_empty() {
            return Err(segment::corrupt());
        }
        use futures::{stream::FuturesUnordered, StreamExt};
        let mut next_body = 0;
        while next_body < publication.bodies.len() {
            let limit = self.provider.transfer_concurrency().max(1);
            let end = (next_body + limit).min(publication.bodies.len());
            let mut ready = Vec::new();
            for index in next_body..end {
                cancel.check()?;
                if publication.bodies[index].complete { continue; }
                let job = publication.asset_job.clone().ok_or_else(segment::corrupt)?;
                let path = sealed_body_path(store.repository_root(), &job.job_id, &publication.bodies[index].object_id);
                if publication.bodies[index].sha256.is_empty() {
                    let body = &publication.bodies[index];
                    let asset = publication.assets.iter().find(|asset| asset.content_hash == body.content_hash)
                        .ok_or_else(segment::corrupt)?;
                    let (byte_length, sha256) = self.seal_frozen_standalone(store, asset, &job, &body.object_id, publication.authority, &path, cancel).await?;
                    publication.bodies[index].sha256 = sha256;
                    publication.bodies[index].byte_length = byte_length;
                    store.external_lww_persist(&publication, &sealed).map_err(store_error)?;
                }
                let body = publication.bodies[index].clone();
                let source = super::transfer::SpoolSource::verified(&path, body.byte_length, &body.sha256)?;
                let intent = ObjectIntent {
                    job_id: self.library.clone(),
                    repository_id: self.repository.repository_id.clone(),
                    object_id: body.object_id.clone(),
                    role: ObjectRole::Pack,
                    byte_length: body.byte_length,
                    sha256: body.sha256.clone(),
                };
                let mut finished = None;
                {
                    let saved = publication.bodies[index].resume.as_ref().map(Self::resume);
                    match self.provider.reconcile_upload(&self.repository, &intent, saved.as_ref(), cancel).await? {
                        UploadResolution::Complete(receipt) => finished = Some(receipt),
                        UploadResolution::Resumable(state) => publication.bodies[index].resume = Some(Self::saved(state)),
                        UploadResolution::RestartRequired => publication.bodies[index].resume = None,
                        UploadResolution::Conflict => return Err(segment::corrupt()),
                    }
                    store
                        .external_lww_persist(&publication, &sealed)
                        .map_err(store_error)?;
                }
                if let Some(receipt) = finished {
                    Self::validate_receipt(&intent, &receipt)?;
                    publication.bodies[index].locator = Some(receipt.locator);
                    publication.bodies[index].complete = true;
                    store.external_lww_persist(&publication, &sealed).map_err(store_error)?;
                    continue;
                }
                if publication.bodies[index].resume.is_none() {
                    publication.bodies[index].resume = self.provider
                        .begin_upload(&self.repository, &intent, cancel).await?.map(Self::saved);
                }
                // The sealed identity and session are durable before dispatch,
                // including providers whose uploads have no session state.
                store.external_lww_persist(&publication, &sealed).map_err(store_error)?;
                let resume = publication.bodies[index].resume.as_ref().map(Self::resume);
                ready.push((index, intent, source, resume));
            }
            let mut active = FuturesUnordered::new();
            for (index, intent, source, resume) in ready {
                active.push(async move {
                    let outcome = self.provider.create_object(&self.repository, &intent, &source,
                        resume.as_ref(), cancel).await;
                    (index, intent, outcome)
                });
            }
            let mut failure = None;
            while let Some((index, intent, outcome)) = active.next().await {
                let settled = (|| -> Result<()> {
                    let receipt = outcome?;
                    Self::validate_receipt(&intent, &receipt)?;
                    publication.bodies[index].locator = Some(receipt.locator);
                    publication.bodies[index].complete = true;
                    store.external_lww_persist(&publication, &sealed).map_err(store_error)?;
                    Ok(())
                })();
                if let Err(error) = settled {
                    if failure.is_none() { failure = Some(error); }
                }
            }
            if let Some(error) = failure { return Err(error); }
            next_body = end;
        }
        if !publication.sealed {
            let plain = URL_SAFE_NO_PAD
                .decode(&publication.payload)
                .map_err(|_| segment::corrupt())?;
            let mut payload = Segment::decode_capture(&plain)?;
            payload.data_catalogs.extend(publication.data_catalogs.clone());
            payload.asset_catalogs.extend(publication.asset_catalogs.clone());
            for (hash,body) in &mut payload.large_bodies {
                if let Some(proof)=publication.reused_assets.iter().find(|proof|proof.content_hash==*hash) {
                    match &proof.reference {
                        AssetReference::Standalone(original) if original==&*body=>continue,
                        _=>return Err(segment::corrupt()),
                    }
                }
                let prepared = publication
                    .bodies
                    .iter()
                    .find(|item| item.object_id == body.object_id)
                    .filter(|item| item.complete && !item.sha256.is_empty() && item.byte_length > 0)
                    .ok_or_else(segment::corrupt)?;
                body.sha256 = prepared.sha256.clone();
                body.byte_length = DecimalU64(prepared.byte_length);
                body.locator = prepared.locator.clone();
                let locator = body.locator.as_ref().ok_or_else(segment::corrupt)?;
                locator.validate_for(&self.repository)?;
                if locator.collection.as_ref().is_some_and(|collection| collection.len() > MAX_LOCATOR_COLLECTION_BYTES) {
                    return Err(segment::corrupt());
                }
            }
            let (bytes, payload_hash) = segment::seal(&payload, &self.root_key)?;
            sealed = bytes;
            publication.payload_sha256 = payload_hash;
            publication.sha256 = segment::digest(&sealed);
            publication.object_id =
                segment_object_id(&publication.writer, publication.seq.0, &publication.sha256)?;
            publication.sealed = true;
            publication.payload = URL_SAFE_NO_PAD.encode(payload.encode()?);
            store
                .external_lww_persist(&publication, &sealed)
                .map_err(store_error)?;
        }
        let intent = ObjectIntent {
            job_id: self.library.clone(),
            repository_id: self.repository.repository_id.clone(),
            object_id: publication.object_id.clone(),
            role: ObjectRole::Segment,
            byte_length: sealed.len() as u64,
            sha256: publication.sha256.clone(),
        };
        if !publication.complete {
            Self::check_publication_authority(store, &publication)?;
            if let Some((owner, context)) = protection {
                owner.renew_if_due(context, cancel).await?;
                if let Some(reason) = owner.recheck(context, cancel).await? {
                    return Err(super::leases::yield_error(reason));
                }
            }
            if publication.resume.is_none() {
                publication.resume = self
                    .provider
                    .begin_upload(&self.repository, &intent, cancel)
                    .await?
                    .map(Self::saved);
                store
                    .external_lww_persist(&publication, &sealed)
                    .map_err(store_error)?;
            }
            publication.dispatched = true;
            store
                .external_lww_persist(&publication, &sealed)
                .map_err(store_error)?;
            let resume = publication.resume.as_ref().map(Self::resume);
            #[cfg(test)]
            cycle_keys::observe(|work| {
                work.segment_attempts += 1;
                work.attempted_keys.extend(publication.entries.iter().map(|entry| entry.key.clone()));
            });
            let receipt = self
                .provider
                .create_object(
                    &self.repository,
                    &intent,
                    &BytesSource(sealed.clone()),
                    resume.as_ref(),
                    cancel,
                )
                .await?;
            Self::validate_receipt(&intent, &receipt)?;
            publication.complete = true;
        }
        let payload = segment::open(
            &sealed,
            &self.library,
            &publication.writer,
            publication.seq.0,
            &self.root_key,
        )?;
        let references = segment_references(sealed.len() as u64, &payload)?;
        Self::check_publication_authority(store, &publication)?;
        store
            .external_lww_verify_versions(&self.target_scope(), &publication.writer, publication.seq.0, &payload.changes)
            .map_err(store_error)?;
        self.admit_objects(store, &payload, &publication.object_id, cancel).await?;
        if let Some(rooted_at_ms)=self.trusted_control_time() {
            self.authorize_dependencies(store,&payload,rooted_at_ms)?;
        }
        for hash in payload
            .message_pages
            .keys()
        {
            store
                .external_lww_witness_object(&self.target_scope(), hash)
                .map_err(store_error)?;
        }
        store
            .external_lww_finish_publication(&publication, &sealed, &references)
            .map_err(store_error)?;
        if let Some(job) = &publication.asset_job {
            crate::asset_repository::job_pins::DurableCasJob::open(store.repository_root(), &job.job_id)
                .and_then(|mut pins| pins.release(crate::asset_repository::job_pins::CasReleaseOutcome::Committed))
                .map_err(transient)?;
        }
        #[cfg(test)]
        cycle_keys::observe(|work| {
            work.accepted_publications += 1;
            // Cycle keys follow library content, not the format level every segment declares.
            work.emitted_keys.extend(payload.changes.iter().map(|change| change.key.clone()).filter(|key| !library_format::is_key(key)));
        });
        result.segments.0 += 1;
        result.units.0 += publication.entries.len() as u64;
        result.bytes.0 += sealed.len() as u64;
        Ok(result)
    }
    async fn reconcile_publication(&self,publication:&mut SealedPublication,bytes:&[u8],cancel:&Cancellation)->Result<()> {
        if publication.complete {return Ok(());}
        let intent=ObjectIntent{job_id:self.library.clone(),repository_id:self.repository.repository_id.clone(),
            object_id:publication.object_id.clone(),role:ObjectRole::Segment,byte_length:bytes.len() as u64,sha256:publication.sha256.clone()};
        let resume=publication.resume.as_ref().map(Self::resume);
        match self.provider.reconcile_upload(&self.repository,&intent,resume.as_ref(),cancel).await? {
            UploadResolution::Complete(receipt)=>{Self::validate_receipt(&intent,&receipt)?; publication.complete=true;}
            UploadResolution::Resumable(state)=>publication.resume=Some(Self::saved(state)),
            UploadResolution::Conflict=>return Err(segment::corrupt()),
            UploadResolution::RestartRequired=>{},
        }
        Ok(())
    }
    fn check_publication_authority(store: &PersistentStore, publication: &SealedPublication) -> Result<()> {
        if store.lww_binding_authority().map_err(store_error)? != publication.authority {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }
    fn lease_clock(&self)->&dyn super::leases::LeaseClock {
        #[cfg(test)]
        if let Some(clock)=&self.test_lease_clock { return clock.as_ref(); }
        super::leases::system_clock()
    }
    fn trusted_control_time(&self)->Option<u64> {
        let reading=self.lease_clock().reading();
        (reading.trusted && reading.foreground).then_some(reading.wall_ms)
    }
    fn check_frozen_body_authority(store: &PersistentStore, authority: DecimalU64, cancel: &Cancellation) -> crate::server_sync::Result<()> {
        cancel.check().map_err(|_| crate::server_sync::SyncError::new("operation-cancelled", 499))?;
        if store.lww_binding_authority().map_err(|_| crate::server_sync::SyncError::new("binding-authority-unavailable", 409))? != authority {
            return Err(crate::server_sync::SyncError::new("binding-authority-changed", 409));
        }
        Ok(())
    }
    fn body_source_error(error: crate::server_sync::SyncError, cancel: &Cancellation) -> ProviderError {
        ProviderError::new(if cancel.check().is_err() { ErrorKind::Cancelled }
            else if error.code == "binding-authority-changed" || error.code == "binding-authority-unavailable" { ErrorKind::PreconditionFailed }
            else { ErrorKind::Transient })
    }
    /// A body the server or external storage that holds it could not provide.
    fn held_body_error(error: ProviderError) -> ProviderError {
        match error.kind {
            ErrorKind::Cancelled | ErrorKind::PreconditionFailed | ErrorKind::Corrupt
                | ErrorKind::LocalStorageFull | ErrorKind::LocalPermissionDenied | ErrorKind::LocalFailure => error,
            _ => ProviderError { kind: ErrorKind::PreviousStorageUnavailable, http_status: None, retry_at_ms: None,
                oauth_error: None, oauth_error_description: None, cause: error.cause },
        }
    }
    fn held_server_body_error(error: crate::server_sync::SyncError, cancel: &Cancellation) -> ProviderError {
        if error.code == "local-storage-full" { return ProviderError::new(ErrorKind::LocalStorageFull); }
        let cause = ErrorCause(Some(error.code.clone()));
        Self::held_body_error(ProviderError { cause, ..Self::body_source_error(error, cancel) })
    }
    async fn open_frozen_server_body(
        store: &mut PersistentStore, asset: &FrozenAsset, job: &super::journal::JobIdentity,
        authority: DecimalU64, cancel: &Cancellation,
    ) -> Result<crate::server_sync::residency::TransientBody> {
        let proof = asset.server_source.as_ref().ok_or_else(segment::corrupt)?.clone();
        if proof.hash != asset.content_hash || proof.size != asset.byte_length { return Err(segment::corrupt()); }
        let root = store.repository_root().to_owned();
        let scratch = root.join("external-storage").join("lww-publications").join(&job.job_id);
        let store = store.open_native_job_store().map_err(store_error)?;
        let cancel = cancel.clone();
        #[cfg(test)]
        let hash_scope = crate::server_sync::hash_metrics::capture();
        spawn_blocking(move || {
            #[cfg(test)]
            let _hash_scope = crate::server_sync::hash_metrics::enter(hash_scope);
            std::fs::create_dir_all(&scratch).map_err(transient)?;
            let check = || Self::check_frozen_body_authority(&store, authority, &cancel);
            crate::server_sync::residency::open_transient_server_proof_with_check(&root, &scratch, &proof, &check)
                .map_err(|error| Self::held_server_body_error(error, &cancel))?.ok_or_else(segment::corrupt)
        }).await.map_err(transient)?
    }
    /// Seals one captured standalone body into `path`, reading it once without
    /// holding it in memory, and returns the sealed length and digest.
    #[allow(clippy::too_many_arguments)]
    async fn seal_frozen_standalone(
        &self,
        store: &mut PersistentStore,
        asset: &FrozenAsset,
        job: &super::journal::JobIdentity,
        object_id: &str,
        authority: DecimalU64,
        path: &std::path::Path,
        cancel: &Cancellation,
    ) -> Result<(u64, String)> {
        Self::check_frozen_body_authority(store, authority, cancel).map_err(|error| Self::body_source_error(error, cancel))?;
        std::fs::create_dir_all(path.parent().ok_or_else(segment::corrupt)?).map_err(transient)?;
        let mut failure = None;
        let sealed = if asset.local_pin {
            let mut check = || cancel.check().map_err(|error| {
                failure = Some(error);
                std::io::Error::other("cancelled")
            });
            let cas = crate::asset_repository::PayloadCas::new(store.repository_root()).map_err(transient)?;
            match cas.open_object(&asset.content_hash).map_err(transient)? {
                Some(file) => self.seal_checked(file, asset, object_id, path, &mut check),
                None => {
                    let input = store.lww_native_object(&asset.content_hash).map_err(store_error)?.ok_or_else(segment::corrupt)?;
                    self.seal_checked(input, asset, object_id, path, &mut check)
                }
            }
        } else if let Some(proof) = &asset.remote_source {
            let spool = super::lww_residency::spool_frozen_remote_body(
                &proof.clone().rooted(store.repository_root()), &publication_directory(store.repository_root(), &job.job_id), cancel,
            ).await.map_err(Self::held_body_error)?;
            Self::check_frozen_body_authority(store, authority, cancel).map_err(|error| Self::body_source_error(error, cancel))?;
            #[cfg(test)]
            let _scope = crate::asset_repository::body_io::object_scope(&asset.content_hash);
            let input = crate::trust_boundary::open_regular_source(spool.path());
            #[cfg(test)]
            crate::asset_repository::body_io::open_result("managed", &input);
            let input = input.map_err(transient)?;
            #[cfg(test)]
            let input = crate::asset_repository::body_io::TrackedBodyFile::new(input, &asset.content_hash);
            let mut check = || cancel.check().map_err(|error| {
                failure = Some(error);
                std::io::Error::other("cancelled")
            });
            self.seal_checked(input, asset, object_id, path, &mut check)
        } else {
            let body = Self::open_frozen_server_body(store, asset, job, authority, cancel).await?;
            let store = &*store;
            let mut check = || Self::check_frozen_body_authority(store, authority, cancel).map_err(|error| {
                failure = Some(Self::body_source_error(error, cancel));
                std::io::Error::other("held")
            });
            self.seal_checked(body, asset, object_id, path, &mut check)
        };
        match (sealed, failure) {
            (Err(_), Some(error)) => Err(error),
            (sealed, _) => sealed,
        }
    }
    fn seal_checked(
        &self,
        input: impl std::io::Read,
        asset: &FrozenAsset,
        object_id: &str,
        path: &std::path::Path,
        check: &mut dyn FnMut() -> std::io::Result<()>,
    ) -> Result<(u64, String)> {
        let mut input = CheckedBody { input, digest: Sha256::new(), length: 0, check };
        let file = std::fs::File::create(path).map_err(transient)?;
        let mut output = HashingFile { file, digest: Sha256::new(), length: 0 };
        segment::seal_body_stream(&mut input, &mut output, &self.library, object_id, &self.root_key, asset.byte_length)?;
        if input.length != asset.byte_length || hex::encode(input.digest.finalize()) != asset.content_hash {
            return Err(segment::corrupt());
        }
        output.file.sync_all().map_err(transient)?;
        Ok((output.length, hex::encode(output.digest.finalize())))
    }
    fn validate_receipt(intent: &ObjectIntent, receipt: &ObjectReceipt) -> Result<()> {
        if !receipt.complete
            || receipt.byte_length != intent.byte_length
            || receipt.checksum.as_ref().is_some_and(|checksum| {
                checksum.algorithm != "sha256" || checksum.value != intent.sha256
            })
        {
            return Err(segment::corrupt());
        }
        Ok(())
    }
    fn repair_unsent(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        _entries: &[crate::persistent_store::lww::OutboxEntry],
    ) -> Result<()> {
        let header = Header {
            binding_authority: authority,
            request_id: format!("external-clock-repair-{}", uuid::Uuid::new_v4()),
        };
        let proof = format!("external-unsent-{}", header.request_id);
        let entries = store
            .external_lww_unpublished_entries(authority)
            .map_err(store_error)?;
        let acks = entries
            .iter()
            .map(|entry| {
                let identity = entry.value.identity();
                #[cfg(test)]
                segment::observe_value_identity(
                    "publish-repair.descriptor",
                    "publish-repair.identity",
                    &entry.value,
                    identity.is_ok(),
                );
                Ok(crate::persistent_store::lww::AckEntry {
                    key: entry.key.clone(),
                    stamp: entry.stamp.clone(),
                    version: entry.version.clone(),
                    value_identity: identity.map_err(|_| segment::corrupt())?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        store
            .lww_record_unpublished_proof(&header, &proof, &acks)
            .map_err(store_error)?;
        store
            .lww_retry_unpublished(&header, &proof, DecimalU64(self.admitted_upper()?))
            .map_err(super::clock::repair_failure)?;
        Ok(())
    }
    pub(crate) async fn settle_publication(
        &self,
        store: &mut PersistentStore,
        cancel: &Cancellation,
    ) -> Result<()> {
        match self.reconcile_pending(store, cancel).await? {
            Settlement::Nothing | Settlement::Landed => Ok(()),
            Settlement::Missing | Settlement::Conflict => Err(ProviderError::new(ErrorKind::PreconditionFailed)),
        }
    }
    /// Stops depending on this repository before the binding changes. A sent
    /// segment that the repository cannot be asked about, or never received,
    /// does not hold the change back: a switch that keeps the pending state
    /// sends it to this repository again, and one that drops it settles it the
    /// next time this repository is bound.
    pub(crate) async fn fence_binding_change(
        &self,
        store: &mut PersistentStore,
        new_device: bool,
        cancel: &Cancellation,
    ) -> Result<()> {
        match self.reconcile_pending(store, cancel).await {
            Ok(Settlement::Conflict) => Err(segment::corrupt()),
            Ok(_) => Ok(()),
            Err(error) => Self::fence_outcome(error, new_device),
        }
    }
    /// Only an unreachable or missing repository lets a binding change go
    /// ahead without an answer, and a refusal of this device does too when the
    /// change makes it a new device.
    pub(crate) fn fence_outcome(error: ProviderError, new_device: bool) -> Result<()> {
        match error.kind {
            ErrorKind::Transient
            | ErrorKind::RateLimited
            | ErrorKind::DailyQuotaExhausted
            | ErrorKind::EndpointRejected
            | ErrorKind::NotFound => Ok(()),
            ErrorKind::Unauthorized | ErrorKind::ReauthRequired if new_device => Ok(()),
            _ => Err(error),
        }
    }
    async fn reconcile_pending(
        &self,
        store: &mut PersistentStore,
        cancel: &Cancellation,
    ) -> Result<Settlement> {
        let writer = store.lww_clock_state().map_err(store_error)?.writer_id;
        let Some((publication, bytes)) = store
            .external_lww_pending(&self.target_scope(), &writer)
            .map_err(store_error)?
        else {
            return Ok(Settlement::Nothing);
        };
        if !publication.dispatched || publication.complete {
            return Ok(Settlement::Nothing);
        }
        self.reconcile_sent(&publication, &bytes, cancel).await
    }
    async fn reconcile_sent(
        &self,
        publication: &SealedPublication,
        bytes: &[u8],
        cancel: &Cancellation,
    ) -> Result<Settlement> {
        let intent = self.intent(&publication.object_id, ObjectRole::Segment, bytes);
        let resume = publication.resume.as_ref().map(Self::resume);
        let receipt = match self
            .provider
            .reconcile_upload(&self.repository, &intent, resume.as_ref(), cancel)
            .await?
        {
            UploadResolution::Complete(receipt) => receipt,
            UploadResolution::Conflict => return Ok(Settlement::Conflict),
            UploadResolution::Resumable(_) | UploadResolution::RestartRequired => {
                return Ok(Settlement::Missing)
            }
        };
        Self::validate_receipt(&intent, &receipt)?;
        let received = read_bytes(
            self.provider.as_ref(),
            &self.repository,
            &receipt.locator,
            cancel,
        )
        .await?;
        if received != bytes {
            return Err(segment::corrupt());
        }
        segment::open(
            &received,
            &self.library,
            &publication.writer,
            publication.seq.0,
            &self.root_key,
        )?;
        Ok(Settlement::Landed)
    }
    /// Settles a segment captured under an earlier binding of this repository
    /// that the switch did not carry. Its versions are never sent again: a
    /// segment the repository holds keeps its sequence, and any other is
    /// dropped so the next segment takes the sequence.
    async fn settle_detached(
        &self,
        store: &mut PersistentStore,
        publication: SealedPublication,
        sealed: Vec<u8>,
        cancel: &Cancellation,
    ) -> Result<()> {
        let landed = if publication.complete {
            true
        } else if publication.dispatched {
            match self.reconcile_sent(&publication, &sealed, cancel).await? {
                Settlement::Landed => true,
                Settlement::Conflict => return Err(segment::corrupt()),
                Settlement::Nothing => false,
                Settlement::Missing => self.landed_before_cleanup(store, &publication, cancel).await?,
            }
        } else {
            false
        };
        let references = if landed {
            let payload = segment::open(&sealed, &self.library, &publication.writer, publication.seq.0, &self.root_key)?;
            store
                .external_lww_verify_versions(&self.target_scope(), &publication.writer, publication.seq.0, &payload.changes)
                .map_err(store_error)?;
            Some(segment_references(sealed.len() as u64, &payload)?)
        } else {
            None
        };
        store
            .external_lww_settle_detached(&publication, references.as_ref())
            .map_err(store_error)?;
        if let Some(job) = &publication.asset_job {
            let outcome = if landed {
                crate::asset_repository::job_pins::CasReleaseOutcome::Committed
            } else {
                crate::asset_repository::job_pins::CasReleaseOutcome::Aborted
            };
            crate::persistent_store::external_lww::release_cas_job(store.repository_root(), &job.job_id, outcome)
                .map_err(store_error)?;
        }
        Ok(())
    }
    /// A sent segment the repository no longer holds may have landed and been
    /// removed after a checkpoint covered it. Its sequence is reused only when
    /// no checkpoint covers it and this device has not received it either.
    async fn landed_before_cleanup(
        &self,
        store: &mut PersistentStore,
        publication: &SealedPublication,
        cancel: &Cancellation,
    ) -> Result<bool> {
        let authority = store.lww_binding_authority().map_err(store_error)?;
        let received = store
            .lww_receive_progress(authority)
            .map_err(store_error)?
            .into_iter()
            .any(|progress| {
                progress.kind == "external"
                    && progress.writer_id.as_deref() == Some(publication.writer.as_str())
                    && progress.cursor >= publication.seq
            });
        if received {
            return Ok(true);
        }
        Ok(self.checkpoints(cancel).await?.into_iter().any(|(_, checkpoint)| {
            checkpoint
                .covered_prefixes
                .get(&publication.writer)
                .is_some_and(|prefix| *prefix >= publication.seq)
        }))
    }
    fn include_objects(
        &self,
        store: &mut PersistentStore,
        key: &risunest_sync_wire::unit::UnitKey,
        value: &UnitValue,
        payload: &mut Segment,
        bodies: &mut Vec<SealedBody>,
        assets: &mut BTreeMap<String, FrozenAsset>,
        controls: &mut BTreeMap<String,String>,
        reused_controls: &mut BTreeMap<String,FrozenControlCatalog>,
        reused_assets: &mut BTreeMap<String,FrozenAssetReference>,
        remote_bodies: &mut super::lww_residency::RemoteBodies,
        server_bodies: &mut Option<crate::server_sync::residency::Residency>,
        cancel: &Cancellation,
    ) -> Result<Vec<String>> {
        let mut fresh = Vec::new();
        let mut hashes = BTreeSet::new();
        // The parts of a structured plugin value travel as raw bytes in a
        // control catalog, whatever their size.
        let parts = crate::persistent_store::external_capture::plugin_value_parts(key, value);
        if let UnitValue::Object { descriptor, .. } = value {
            hashes.insert(descriptor.object_hash.clone());
            hashes.extend(descriptor.dependencies.iter().cloned());
            if let Some(root) = &descriptor.dependency_root {
                risunest_sync_wire::descriptor::visit_reference_tree(
                    root,
                    false,
                    |hash| {
                        let bytes = store
                            .lww_object_body(hash)
                            .map_err(|_| risunest_sync_wire::WireError("object-read"))?
                            .ok_or(risunest_sync_wire::WireError("object-missing"))?;
                        #[cfg(test)]
                        segment::observe_hash_input("publish-reference-tree.page", bytes.len());
                        Ok(bytes)
                    },
                    |hash, _| {
                        hashes.insert(hash.into());
                        Ok(())
                    },
                )
                .map_err(|_| segment::corrupt())?;
            }
        } else if let UnitValue::Inline { bytes } = value {
            let bytes = URL_SAFE_NO_PAD
                .decode(bytes)
                .map_err(|_| segment::corrupt())?;
            let value: serde_json::Value =
                serde_json::from_slice(&bytes).map_err(|_| segment::corrupt())?;
            if let Some(hash) = value.get("objectHash").and_then(serde_json::Value::as_str) {
                hashes.insert(hash.into());
            }
        }
        for hash in hashes {
            cancel.check()?;
            if payload.message_pages.contains_key(&hash)
                || controls.contains_key(&hash)
                || assets.contains_key(&hash)
                || reused_assets.contains_key(&hash)
                || payload.large_bodies.contains_key(&hash)
            {
                continue;
            }
            fresh.push(hash.clone());
            if store.external_lww_object_is_control(&hash).map_err(store_error)? {
                if let Some(now)=self.trusted_control_time() {
                    if let Some(proof)=store.external_lww_reusable_control_catalog(&self.target_scope(),&hash,now).map_err(store_error)? {
                        if let Some(previous)=payload.data_catalogs.iter().find(|root|root.header.object_id==proof.catalog.header.object_id) {
                            if previous!=&proof.catalog {return Err(segment::corrupt());}
                        } else {payload.data_catalogs.push(proof.catalog.clone());}
                        reused_controls.insert(hex::encode(proof.catalog.ciphertext_sha256),proof);
                        continue;
                    }
                }
                if parts.contains(&hash)
                    || store.lww_native_object(&hash).map_err(store_error)?.is_some_and(|body| body.len() as usize > 8 * 1024 * 1024)
                {
                    // An empty entry asks the accepted publication to freeze the native body directly.
                    controls.insert(hash, String::new());
                    continue;
                }
                let bytes = store.lww_object_body(&hash).map_err(store_error)?.ok_or_else(segment::corrupt)?;
                let encoded=URL_SAFE_NO_PAD.encode(&bytes);
                if bytes.len()>segment::SMALL_BODY_BYTES { controls.insert(hash,encoded); }
                else { payload.message_pages.insert(hash,encoded); }
                continue;
            }
            if let Some(now)=self.trusted_control_time() {
                if let Some(proof)=store.external_lww_reusable_asset(&self.target_scope(),&hash,now).map_err(store_error)? {
                    match &proof.reference {
                        AssetReference::Catalog(catalog)=>{
                            if let Some(previous)=payload.asset_catalogs.iter().find(|root|root.header.object_id==catalog.header.object_id) {
                                if previous!=catalog {return Err(segment::corrupt());}
                            } else {payload.asset_catalogs.push(catalog.clone());}
                        }
                        AssetReference::Standalone(body)=>{payload.large_bodies.insert(hash.clone(),body.clone());}
                    }
                    reused_assets.insert(hash,proof);
                    continue;
                }
            }
            let root = store.repository_root();
            let local_size = crate::asset_repository::PayloadCas::new(root).map_err(transient)?
                .stat_object(&hash).map_err(transient)?;
            let remote_source = if local_size.is_none() {
                remote_bodies.frozen(&hash)?
            } else { None };
            let server_source = if local_size.is_none() && remote_source.is_none() {
                if server_bodies.is_none() {
                    if !crate::server_sync::residency::Residency::exists(root) { return Err(segment::corrupt()); }
                    *server_bodies = Some(crate::server_sync::residency::Residency::open(root).map_err(|error| transient(error.code))?);
                }
                Some(server_bodies.as_ref().ok_or_else(segment::corrupt)?
                    .object(&hash, None).map_err(|_| ProviderError::new(ErrorKind::Transient))?
                    .ok_or_else(segment::corrupt)?)
            } else { None };
            let byte_length = match local_size {
                Some(size) => size,
                None => match &remote_source {
                    Some(source) => source.byte_length(),
                    None => server_source.as_ref().ok_or_else(segment::corrupt)?.size,
                },
            };
            assets.insert(hash.clone(), FrozenAsset {
                content_hash: hash.clone(), byte_length, local_pin: local_size.is_some(), remote_source, server_source,
            });
            if byte_length > segment::SMALL_BODY_BYTES as u64 {
                let id = uuid::Uuid::new_v4().to_string();
                payload.large_bodies.insert(hash.clone(), LargeBody {
                    object_id: id.clone(), sha256: "0".repeat(64), byte_length: DecimalU64(0),
                    plaintext_byte_length: DecimalU64(byte_length), locator: None,
                });
                bodies.push(SealedBody {
                    object_id: id, content_hash: hash, byte_length: 0,
                    sha256: String::new(), resume: None,
                    complete: false, locator: None,
                });
            }
        }
        Ok(fresh)
    }
    pub(crate) async fn listing(&self, cancel: &Cancellation) -> Result<Vec<ObjectReceipt>> {
        let mut objects = Vec::new();
        let mut cursor = None;
        let mut cursors = BTreeSet::new();
        loop {
            let page = self
                .provider
                .list_objects(
                    &self.repository,
                    Collection::Segments,
                    cursor.as_deref(),
                    LISTING_PAGE,
                    cancel,
                )
                .await?;
            objects.extend(page.objects);
            match page.next_cursor {
                Some(next) => {
                    if !cursors.insert(next.clone()) {
                        return Err(segment::corrupt());
                    }
                    cursor = Some(next)
                }
                None => break,
            }
        }
        Ok(objects)
    }
    async fn admit_objects(
        &self,
        store: &mut PersistentStore,
        payload: &Segment,
        physical_segment: &str,
        cancel: &Cancellation,
    ) -> Result<()> {
        for (hash, encoded) in &payload.message_pages {
            let bytes = URL_SAFE_NO_PAD
                .decode(encoded)
                .map_err(|_| segment::corrupt())?;
            store.lww_put_object(hash, &bytes).map_err(store_error)?;
            store
                .external_lww_witness_object(&self.target_scope(), hash)
                .map_err(store_error)?;
        }
        let hashes=super::snapshot_restore::admit_data_catalogs(
            &payload.data_catalogs,store,&self.target_scope(),&self.root_key,
            Some(&super::runtime::connection_directory(&self.connection_root,&self.connection_id).join("package-cache")),self.provider.as_ref(),&self.repository,cancel,
        ).await?;
        for hash in hashes {
            store.external_lww_witness_object(&self.target_scope(),&hash).map_err(store_error)?;
        }
        super::snapshot_restore::admit_asset_catalogs(
            &payload.asset_catalogs, store, &self.target_scope(), &self.library, physical_segment, &self.connection_id,
            &self.connection_root, &self.root_key, self.provider.as_ref(), &self.repository, cancel,
        ).await?;
        self.register_sources(store, payload, physical_segment)?;
        if let Some(rooted_at_ms)=payload.changes.iter().map(|change|change.stamp.physical_ms.0).max() {
            // Original unit clocks bound publication age; downloading does not renew it.
            let upper=self.admitted_upper()?.checked_add(300_000).ok_or_else(segment::corrupt)?;
            if rooted_at_ms>upper {return Err(super::clock::future_change("receive-objects",rooted_at_ms,upper));}
            self.authorize_dependencies(store,payload,rooted_at_ms)?;
        }
        Ok(())
    }
    fn authorize_dependencies(&self,store:&PersistentStore,payload:&Segment,rooted_at_ms:u64)->Result<()> {
        for catalog in &payload.data_catalogs {
            store.external_lww_authorize_data_catalog(&self.target_scope(),catalog,rooted_at_ms).map_err(store_error)?;
        }
        for catalog in &payload.asset_catalogs {
            store.external_lww_authorize_asset_catalog(&self.target_scope(),catalog,rooted_at_ms).map_err(store_error)?;
        }
        for (hash,body) in &payload.large_bodies {
            store.external_lww_authorize_standalone(&self.target_scope(),hash,body,rooted_at_ms).map_err(store_error)?;
        }
        Ok(())
    }
    fn register_sources(
        &self,
        store: &PersistentStore,
        payload: &Segment,
        physical_segment: &str,
    ) -> Result<()> {
        for (hash, body) in &payload.large_bodies {
            let locator = body.locator.as_ref().ok_or_else(segment::corrupt)?;
            locator.validate_for(&self.repository)?;
            store
                .external_lww_register_source(&super::lww_residency::Source {
                    hash: hash.clone(),
                    library_id: self.library.clone(),
                    connection_id: self.connection_id.clone(),
                    connection_root: self.connection_root.clone(),
                    protected_segment: physical_segment.into(),
                    body: body.clone(),
                })
                .map_err(store_error)?;
            store
                .external_lww_witness_standalone(&self.target_scope(), hash, body)
                .map_err(store_error)?;
        }
        Ok(())
    }
    pub(crate) async fn stage_binding(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        inspection: &str,
        cancel: &Cancellation,
    ) -> Result<crate::persistent_store::lww::BindingUnitStage> {
        let directory=super::leftovers::managed_scratch(store.repository_root(),"lww-published-")?;
        let mut published=self.published_state(store,directory.path(),cancel).await?;
        published.require_complete()?;
        self.stage_published_objects(store,&mut published,directory.path(),cancel).await?;
        self.provider.progress_stage("applying");
        let mut source_failure = None;
        let mut cancellation_failure = None;
        let report_staging = super::progress::staging_reporter();
        let stage = store
            .lww_stage_binding_units_stream_observed(
                header,
                inspection,
                DecimalU64(
                    self.admitted_upper()?
                        .checked_add(300_000)
                        .ok_or_else(segment::corrupt)?,
                ),
                |emit| published.catalog.visit_changes(&mut |change| {
                    cancel.check()?;
                    emit(change).map_err(store_error)
                }).map_err(|error| {
                    source_failure = Some(error);
                    crate::persistent_store::StoreError::Validation { message: "Published binding source is unavailable".into() }
                }),
                |progress| {
                    if let Err(error) = cancel.check() {
                        cancellation_failure = Some(error);
                        return Err(crate::persistent_store::StoreError::Validation { message: "Binding staging cancelled".into() });
                    }
                    report_staging(Some(progress));
                    Ok(())
                },
            )
            .map_err(|error| cancellation_failure.or(source_failure).unwrap_or_else(|| store_error(error)))?;
        store
            .external_lww_record_stage(&self.target_scope(), header, &published.catalog.coverage, &published.covered())
            .map_err(store_error)?;
        Ok(stage)
    }
    pub(crate) async fn prepare_new_device(
        &self,
        store: &mut PersistentStore,
        header: &Header,
        staging: &str,
        cancel: &Cancellation,
    ) -> Result<crate::persistent_store::lww::NewDevicePreparation> {
        self.settle_publication(store, cancel).await?;
        let directory=super::leftovers::managed_scratch(store.repository_root(),"lww-published-")?;
        self.published_state(store,directory.path(),cancel).await?.require_complete()?;
        let preparation = store
            .prepare_lww_new_device(header, staging)
            .map_err(store_error)?;
        store
            .authorize_lww_new_device(&preparation.authorization_id)
            .map_err(store_error)
    }
    #[cfg(test)]
    pub(crate) async fn receive_requests(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        cancel: &Cancellation,
    ) -> Result<Vec<StageReceive>> {
        let ids = self.receive_requests_cached(store, authority, &Default::default(), cancel).await?.pages;
        ids.iter()
            .map(|id| store.external_lww_unfinished_receive(id).map_err(store_error)?.ok_or_else(segment::corrupt))
            .collect()
    }
    /// Stores the receive pages of what this device has not applied and
    /// returns the ids of the unfinished ones in order, and whether the
    /// listing holds more segments than the batch received. Covered segments
    /// are never read, and each segment's pages are stored before the next one
    /// is read. Snapshots `checkpoints` has classified are not read again.
    pub(crate) async fn receive_requests_cached(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        checkpoints: &tokio::sync::Mutex<super::lww_compaction::CheckpointSummaries>,
        cancel: &Cancellation,
    ) -> Result<ReceiveBatch> {
        self.receive_requests_limited(store, authority, checkpoints, RECEIVE_SEGMENT_LIMIT, cancel).await
    }
    async fn receive_requests_limited(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        checkpoints: &tokio::sync::Mutex<super::lww_compaction::CheckpointSummaries>,
        segment_limit: usize,
        cancel: &Cancellation,
    ) -> Result<ReceiveBatch> {
        let target = self.target_scope();
        store.external_lww_seed_activation(&target, authority).map_err(store_error)?;
        let progress=store.lww_receive_progress(authority).map_err(store_error)?;
        let current=progress.into_iter().filter(|p|p.kind=="external").filter_map(|p|p.writer_id.map(|w|(w,p.cursor))).collect::<BTreeMap<_,_>>();
        let mut covered=BTreeMap::new();
        for snapshot in self.checkpoint_summaries(checkpoints, cancel).await? {
            for (writer,prefix) in snapshot.covered_prefixes {
                let value=covered.entry(writer).or_insert(DecimalU64(0));
                *value=(*value).max(prefix);
            }
        }
        let groups = segment_groups(self.listing(cancel).await?)?;
        store
            .external_lww_observe_listing(&target, &groups.keys().cloned().collect(), &covered)
            .map_err(store_error)?;
        let upper=DecimalU64(self.admitted_upper()?.checked_add(300_000).ok_or_else(segment::corrupt)?);
        let mut offered = Vec::new();
        let listed = reachable(&current, &groups);
        if !behind(&covered, &listed) {
            let (reached, stopped) = self.receive_segments(store, authority, &current, groups, upper, &mut offered, segment_limit, cancel).await?;
            if stopped {
                let more = listed.iter().any(|(writer, seq)| reached.get(writer).copied().unwrap_or(0) < *seq);
                return Ok(ReceiveBatch { pages: offered, more });
            }
            if !behind(&covered, &reached) {
                store.external_lww_retain_receives(&target, &offered.iter().cloned().collect()).map_err(store_error)?;
                return Ok(ReceiveBatch { pages: offered, more: false });
            }
            offered.clear();
        }
        self.receive_published(store, authority, &current, upper, &mut offered, cancel).await?;
        store.external_lww_retain_receives(&target, &offered.iter().cloned().collect()).map_err(store_error)?;
        Ok(ReceiveBatch { pages: offered, more: false })
    }
    /// Receives each writer's next consecutive segments and returns the
    /// sequence each writer reached. A writer whose clock runs ahead of the
    /// admitted bound waits without holding back the other writers; its later
    /// segments wait behind it.
    #[allow(clippy::too_many_arguments)]
    async fn receive_segments(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        current: &BTreeMap<String, DecimalU64>,
        groups: BTreeMap<(String, u64), (String, ObjectReceipt)>,
        upper: DecimalU64,
        offered: &mut Vec<String>,
        segment_limit: usize,
        cancel: &Cancellation,
    ) -> Result<(BTreeMap<String, u64>, bool)> {
        let target = self.target_scope();
        let mut prefixes = current.iter().map(|(writer, cursor)| (writer.clone(), cursor.0)).collect::<BTreeMap<_, _>>();
        let mut held = BTreeSet::new();
        let mut future_error = None;
        let mut received = 0;
        let mut staged = 0;
        for ((writer, seq), (hash, receipt)) in groups {
            if held.contains(&writer) {
                continue;
            }
            let prefix = prefixes.get(&writer).copied().unwrap_or(0);
            if seq <= prefix {
                store.external_lww_verify_seen(&target, &writer, seq, &hash).map_err(store_error)?;
                continue;
            }
            if seq != prefix.checked_add(1).ok_or_else(segment::corrupt)? {
                continue;
            }
            let bytes = read_bytes(self.provider.as_ref(), &self.repository, &receipt.locator, cancel).await?;
            if segment::digest(&bytes) != hash {
                return Err(segment::corrupt());
            }
            let payload = segment::open(&bytes, &self.library, &writer, seq, &self.root_key)?;
            self.require_readable_format(&payload.changes)?;
            store.external_lww_verify_seen(&target, &writer, seq, &hash).map_err(store_error)?;
            if let Some(change) = payload.changes.iter().find(|change| change.stamp.physical_ms.0 > upper.0) {
                future_error = Some(super::clock::future_change("receive", change.stamp.physical_ms.0, upper.0));
                held.insert(writer);
                continue;
            }
            store
                .external_lww_verify_versions(&target, &writer, seq, &payload.changes)
                .map_err(store_error)?;
            self.admit_objects(store, &payload, &segment_object_id(&writer, seq, &hash)?, cancel).await?;
            let references = segment_references(bytes.len() as u64, &payload)?;
            drop(bytes);
            store
                .external_lww_record_seen(&target, &writer, seq, &hash, Some(&references))
                .map_err(store_error)?;
            let before = offered.len();
            let mut pages = PageWriter::new(
                &target,
                format!("external-receive-{}-{}-{}-{}", self.library, authority.0, writer, seq),
                authority,
                Progress { kind: "external".into(), cursor: DecimalU64(seq), writer_id: Some(writer.clone()) },
                DecimalU64(prefix),
                upper,
            );
            for change in payload.changes {
                pages.push(store, offered, change)?;
            }
            pages.finish(store, offered)?;
            prefixes.insert(writer, seq);
            received += 1;
            staged += usize::from(offered.len() > before);
            if staged >= segment_limit && !offered.is_empty() {
                return Ok((prefixes, true));
            }
        }
        if received == 0 {
            if let Some(error) = future_error { return Err(error); }
        }
        Ok((prefixes, false))
    }
    /// Receives the published state as one catalog when the segments cannot
    /// reach what the checkpoints cover. The first writer's pages carry the
    /// catalog and the rest only advance their writer's progress.
    async fn receive_published(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        current: &BTreeMap<String, DecimalU64>,
        upper: DecimalU64,
        offered: &mut Vec<String>,
        cancel: &Cancellation,
    ) -> Result<()> {
        let directory=super::leftovers::managed_scratch(store.repository_root(),"lww-published-")?;
        let mut state=self.published_state(store,directory.path(),cancel).await?;
        self.stage_published_objects(store,&mut state,directory.path(),cancel).await?;
        let target = self.target_scope();
        for (writer, seq, hash) in state.covered() {
            store.external_lww_record_seen(&target, &writer, seq, &hash, None).map_err(store_error)?;
        }
        let identity=state.catalog.identity()?;
        let mut carried = false;
        for (writer,prefix) in &state.catalog.coverage {
            let before=current.get(writer).copied().unwrap_or(DecimalU64(0));
            if before>=*prefix {continue;}
            let mut pages = PageWriter::new(
                &target,
                format!("external-snapshot-{}-{}-{}-{}",authority.0,identity,writer,prefix.0),
                authority,
                Progress{kind:"external".into(),cursor:*prefix,writer_id:Some(writer.clone())},
                before,
                upper,
            );
            if !carried {
                let store = &*store;
                state.catalog.visit_changes(&mut |change| pages.push(store, offered, change))?;
                carried = true;
            }
            pages.finish(store, offered)?;
        }
        Ok(())
    }
    /// Brings the bodies the archives and restores of `request` read on this device from the
    /// storage that holds them, so applying the page never waits on a body held elsewhere.
    /// Pages are stored before earlier ones are applied, so this runs as each is handed out.
    pub(crate) async fn prepare_archive_bodies(
        store: &mut PersistentStore,
        request: &StageReceive,
        cancel: &Cancellation,
    ) -> Result<()> {
        let missing = store.lww_incoming_archive_bodies(&request.changes).map_err(store_error)?;
        if missing.is_empty() {
            return Ok(());
        }
        let root = store.repository_root().to_owned();
        let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // The flag also stops a transfer the hydration has in flight.
        let watcher = {
            let (cancel, cancelled) = (cancel.clone(), cancelled.clone());
            tokio::spawn(async move {
                cancel.cancelled().await;
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            })
        };
        let worker = cancel.clone();
        let fetched = spawn_blocking(move || {
            let check = || {
                worker.check().map_err(|_| crate::server_sync::SyncError::new("cancelled", 409))
            };
            crate::server_sync::residency::HydrationSession::new(&root, Some(cancelled))
                .and_then(|mut session| session.hydrate_many(&missing, &check))
        })
        .await;
        watcher.abort();
        match fetched.map_err(transient)? {
            Ok(unavailable) if unavailable.is_empty() => Ok(()),
            Ok(_) => Err(ProviderError {
                cause: ErrorCause(Some("required-asset-unavailable".into())),
                ..ProviderError::new(ErrorKind::PreviousStorageUnavailable)
            }),
            Err(error) => Err(Self::held_server_body_error(error, cancel)),
        }
    }
    #[cfg(test)]
    pub(crate) async fn receive_and_apply(
        &self,
        store: &mut PersistentStore,
        authority: DecimalU64,
        generating: &[MessageLocator],
        cancel: &Cancellation,
    ) -> Result<usize> {
        let key_operation = cycle_keys::Operation::start();
        let requests = self.receive_requests(store, authority, cancel).await?;
        let mut count = 0;
        for request in requests {
            Self::prepare_archive_bodies(store, &request, cancel).await?;
            store.lww_stage_receive(&request).map_err(store_error)?;
            let applied = store
                .lww_apply_receive(&ApplyReceive {
                    header: request.header.clone(),
                    generating: generating.to_vec(),
                })
                .map_err(store_error)?;
            store
                .lww_finish_receive(&request.header)
                .map_err(store_error)?;
            cycle_keys::observe(|work| {
                work.receive_applies += 1;
                work.affected_keys.extend(applied.affected_keys.into_iter().filter(|key| !library_format::is_key(key)));
                work.held_keys.extend(applied.held_keys);
                work.deferred_keys.extend(applied.deferred_keys);
            });
            count += 1;
        }
        key_operation.finish();
        Ok(count)
    }
}

#[cfg(test)]
#[path = "lww_upload_concurrency_tests.rs"]
mod upload_concurrency_tests;

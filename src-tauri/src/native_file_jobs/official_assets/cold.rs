//! Account cold items for an official snapshot restore. While staging reads the
//! snapshot, a reader thread reads the cold items the snapshot names ahead of
//! staging, under the slots and retry rules of the asset reads, and inflates
//! each into the job directory, where a bounded spool keeps it until staging
//! takes it. An item staging asks for that is not read yet is read next.
use super::download::{latin1_header, AccountReader};
use super::scheduler::{Attempt, Exchange, Outcome, Pause, Scheduler, Settled, StopReason};
use crate::native_file_jobs::official_snapshot::{await_network_operation, OfficialSnapshotCredential, NETWORK_IDLE_TIMEOUT};
use crate::native_file_jobs::restore::cold_keys::{self, ColdKey};
use crate::native_file_jobs::restore::{is_zlib_header, report_cold_payload_bytes, ColdPayloadProgress, ColdPayloadUnavailable};
use crate::native_file_jobs::{JobControl, NativeJobError};
use flate2::read::{DeflateDecoder, MultiGzDecoder, ZlibDecoder};
use futures::future::{FutureExt, LocalBoxFuture};
use futures::stream::{FuturesUnordered, StreamExt};
use reqwest::header::{ACCEPT_ENCODING, CONTENT_ENCODING, RETRY_AFTER};
use risunest_network_log::reqwest::RequestBuilderExt;
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, Read as _, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

const TICK: Duration = Duration::from_millis(150);
/// The restore's nominal account read slots.
const SLOTS: usize = 2;

/// What the reader may hold ahead of staging: items read or being read, and
/// the inflated bytes of those read. Zero items reads each item only when
/// staging asks for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpoolLimits {
    pub(crate) items: usize,
    pub(crate) bytes: u64,
}

impl Default for SpoolLimits {
    fn default() -> Self {
        Self { items: super::BACKLOG_BODIES, bytes: super::BACKLOG_BYTES }
    }
}

/// An item's place in staging's order: the snapshot's keys in turn, and the
/// chats of a character item right after it.
type Position = (u64, u64);

pub(crate) struct ColdSource<'a> {
    reader: AccountReader,
    job: &'a JobControl,
    limit: u64,
    spool: SpoolLimits,
    state: Mutex<State>,
    /// Wakes staging when an item is ready or reading stops.
    settled: Condvar,
    /// Wakes the reader when staging asks for or takes an item, a key arrives, or staging ends.
    wake: tokio::sync::Notify,
    /// Items handed to staging and their bytes, which the restore counts as progress.
    progress: Arc<ColdPayloadProgress>,
    /// The snapshot offset before which the scan has named every key; the end once it stops.
    scanned: AtomicU64,
}

#[derive(Default)]
struct State {
    /// Items read and not yet taken.
    ready: HashMap<String, Ready>,
    ready_bytes: u64,
    /// The key staging waits for.
    demand: Option<String>,
    /// The place of the last item staging took; the reader skips keys placed before it.
    taken: Option<Position>,
    /// Why reading stopped; staging fails at the first item it still needs.
    stopped: Option<NativeJobError>,
    closed: bool,
    /// The last item handed out; staging has parsed it by the next lookup.
    previous: Option<Payload>,
    unavailable: HashMap<String, ColdPayloadUnavailable>,
    fatal: Option<NativeJobError>,
}

struct Ready {
    item: Item,
    /// Every byte received for the item, counted when staging takes it.
    bytes: u64,
    position: Option<Position>,
}

impl Ready {
    fn size(&self) -> u64 {
        match &self.item {
            Item::Present(payload) => payload.size,
            _ => 0,
        }
    }
}

enum Item {
    Present(Payload),
    Missing,
    Unavailable(ColdPayloadUnavailable),
}

/// An inflated item in the job directory, removed when dropped.
struct Payload {
    path: PathBuf,
    size: u64,
}

impl Drop for Payload {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

enum Fetched {
    /// The item, and for a character item the chat keys it names.
    Present(Payload, Vec<String>),
    Unavailable(ColdPayloadUnavailable),
}

enum Read {
    Body(Fetched),
    Ended(Exchange),
    Cancelled,
}

impl<'a> ColdSource<'a> {
    pub(crate) fn new(
        base_url: &str,
        credential: &OfficialSnapshotCredential,
        directory: &Path,
        job: &'a JobControl,
        limit: u64,
        spool: SpoolLimits,
    ) -> Result<Self, NativeJobError> {
        Ok(Self {
            reader: AccountReader::new(base_url, credential, directory)?,
            job,
            limit,
            spool,
            state: Mutex::default(),
            settled: Condvar::new(),
            wake: tokio::sync::Notify::new(),
            progress: Arc::default(),
            scanned: AtomicU64::new(0),
        })
    }

    /// What has been read so far, for the restore's progress.
    pub(crate) fn progress(&self) -> Arc<ColdPayloadProgress> {
        Arc::clone(&self.progress)
    }

    /// Runs `staging` while the snapshot's cold items are read ahead of it; every
    /// lookup happens inside it. The reader stops and its spool is removed before this returns.
    pub(crate) fn with_read_ahead<T>(&self, snapshot: &Path, staging: impl FnOnce() -> T) -> T {
        let stop = AtomicBool::new(false);
        let (keys, received) = mpsc::sync_channel(self.spool.items.clamp(1, 64));
        std::thread::scope(|scope| {
            let stop = &stop;
            scope.spawn(move || {
                let _scanned = Scanned(self);
                if self.spool.items == 0 {
                    return;
                }
                let Ok(file) = File::open(snapshot) else { return };
                cold_keys::scan(
                    Stoppable { inner: BufReader::new(file), stop },
                    self.limit,
                    &mut |key| {
                        let sent = keys.send(key).is_ok();
                        self.wake.notify_one();
                        sent
                    },
                    &mut |offset| {
                        self.scanned.store(offset, Ordering::Release);
                        self.wake.notify_one();
                    },
                );
            });
            scope.spawn(move || {
                let _exit = ReaderExit(self);
                tauri::async_runtime::block_on(Prefetch::new(self, received).run());
            });
            let _close = Close(self, stop);
            staging()
        })
    }

    /// The inflated item, or `None` when the account holds no readable item for
    /// the key. An error ends the restore, and every later lookup repeats it.
    pub(crate) fn lookup(&self, key: &str) -> Result<Option<PathBuf>, NativeJobError> {
        let mut state = self.lock();
        if let Some(error) = &state.fatal {
            return Err(error.clone());
        }
        // A reader that still holds the file keeps reading it after removal.
        state.previous = None;
        if state.unavailable.contains_key(key) {
            return Ok(None);
        }
        let (mut state, result) = self.take(state, key);
        state.demand = None;
        if let Err(error) = &result {
            state.fatal = Some(error.clone());
        }
        result
    }

    /// Why a key that `lookup` did not find is unreadable; `None` means missing.
    pub(crate) fn unavailable(&self, key: &str) -> Option<ColdPayloadUnavailable> {
        self.lock().unavailable.get(key).copied()
    }

    /// The error that ended the restore, if a lookup ended it.
    pub(crate) fn fatal(&self) -> Option<NativeJobError> {
        self.lock().fatal.clone()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Waits until the reader has the item or stops.
    fn take<'s>(&'s self, mut state: MutexGuard<'s, State>, key: &str) -> (MutexGuard<'s, State>, Result<Option<PathBuf>, NativeJobError>) {
        loop {
            if self.job.is_cancel_requested() {
                return (state, Err(cancelled()));
            }
            if let Some(ready) = state.ready.remove(key) {
                let result = self.hand_out(&mut state, key, ready);
                return (state, result);
            }
            if let Some(error) = &state.stopped {
                let error = error.clone();
                return (state, Err(error));
            }
            if state.demand.as_deref() != Some(key) {
                state.demand = Some(key.to_owned());
                self.wake.notify_one();
            }
            state = self.settled.wait_timeout(state, TICK).unwrap_or_else(|poisoned| poisoned.into_inner()).0;
        }
    }

    fn hand_out(&self, state: &mut State, key: &str, ready: Ready) -> Result<Option<PathBuf>, NativeJobError> {
        state.ready_bytes -= ready.size();
        state.taken = state.taken.max(ready.position);
        self.wake.notify_one();
        if ready.bytes != 0 {
            self.progress.bytes.fetch_add(ready.bytes, Ordering::AcqRel);
            report_cold_payload_bytes(self.job, ready.bytes)?;
        }
        match ready.item {
            Item::Present(payload) => {
                self.progress.items.fetch_add(1, Ordering::AcqRel);
                let path = payload.path.clone();
                state.previous = Some(payload);
                Ok(Some(path))
            }
            Item::Missing => Ok(None),
            Item::Unavailable(reason) => {
                state.unavailable.insert(key.to_owned(), reason);
                Ok(None)
            }
        }
    }

    fn stop(&self, error: NativeJobError) {
        self.lock().stopped.get_or_insert(error);
        self.settled.notify_all();
    }

    /// Reads one item into `path` and counts the bytes received.
    async fn read(&self, key: &str, path: PathBuf, character: bool) -> (Read, u64) {
        let mut received = 0;
        let read = self.request(key, path, character, &mut received).await;
        (read, received)
    }

    async fn request(&self, key: &str, path: PathBuf, character: bool, received: &mut u64) -> Read {
        let Some(key_header) = latin1_header(key) else {
            return Read::Ended(Exchange::Permanent);
        };
        let request = self
            .reader
            .client
            .get(format!("{}/hub/account/coldstorage", self.reader.base_url))
            .headers(self.reader.credential.clone())
            .header("x-risu-key", key_header)
            .header(ACCEPT_ENCODING, "identity")
            .send_logged("account-cold-read");
        let response = match self.network(request).await {
            Ok(response) => response,
            Err(read) => return read,
        };
        let status = response.status().as_u16();
        match status {
            200 => {}
            401 | 403 => return Read::Ended(Exchange::Rejected),
            404 => return Read::Ended(Exchange::Missing),
            408 | 429 | 500..=u16::MAX => {
                return Read::Ended(Exchange::Retryable {
                    status: Some(status),
                    retry_after: response.headers().get(RETRY_AFTER).and_then(|value| value.to_str().ok()).map(str::to_owned),
                })
            }
            _ => return Read::Ended(Exchange::Permanent),
        }
        if response.headers().get(CONTENT_ENCODING).is_some_and(|value| !value.as_bytes().eq_ignore_ascii_case(b"identity")) {
            return Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::Damaged));
        }
        self.inflate(response, path, character, received).await
    }

    async fn network<F, T>(&self, operation: F) -> Result<T, Read>
    where
        F: std::future::Future<Output = Result<T, reqwest::Error>>,
    {
        await_network_operation(operation, self.job, NETWORK_IDLE_TIMEOUT, "Official account cold item read was cancelled")
            .await
            .map_err(|error| {
                if error.code == "cancelled" {
                    Read::Cancelled
                } else {
                    Read::Ended(Exchange::Retryable { status: None, retry_after: None })
                }
            })
    }

    /// Spools the response, then inflates gzip, zlib or raw deflate the way the
    /// renderer's account reader does, within the decoded block limit. Its files
    /// are removed unless it returns the item, also when the read is dropped.
    async fn inflate(&self, mut response: reqwest::Response, path: PathBuf, character: bool, received: &mut u64) -> Read {
        let compressed = Payload { path: path.with_extension("part"), size: 0 };
        let Ok(mut file) = OpenOptions::new().write(true).create_new(true).open(&compressed.path) else {
            return Read::Ended(Exchange::Local);
        };
        loop {
            let chunk = match self.network(response.chunk()).await {
                Ok(Some(chunk)) => chunk,
                Ok(None) => break,
                Err(read) => return read,
            };
            *received += chunk.len() as u64;
            if *received > self.limit {
                return Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::TooLarge));
            }
            if file.write_all(&chunk).is_err() {
                return Read::Ended(Exchange::Local);
            }
        }
        drop(file);
        let Ok(mut source) = File::open(&compressed.path) else {
            return Read::Ended(Exchange::Local);
        };
        let mut head = Vec::with_capacity(2);
        if (&mut source).take(2).read_to_end(&mut head).is_err() {
            return Read::Ended(Exchange::Local);
        }
        if head.len() < 2 {
            return Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::Damaged));
        }
        let gzip = head == [0x1f, 0x8b];
        let zlib = is_zlib_header(&head);
        let source = std::io::Cursor::new(head).chain(BufReader::new(source));
        let mut decoder: Box<dyn std::io::Read> = if gzip {
            Box::new(MultiGzDecoder::new(source))
        } else if zlib {
            Box::new(ZlibDecoder::new(source))
        } else {
            Box::new(DeflateDecoder::new(source))
        };
        let mut payload = Payload { path, size: 0 };
        let Ok(mut output) = OpenOptions::new().write(true).create_new(true).open(&payload.path) else {
            return Read::Ended(Exchange::Local);
        };
        let mut buffer = vec![0u8; 64 * 1024];
        loop {
            if self.job.is_cancel_requested() {
                return Read::Cancelled;
            }
            let read = match decoder.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(_) => return Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::Damaged)),
            };
            payload.size += read as u64;
            if payload.size > self.limit {
                return Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::TooLarge));
            }
            if output.write_all(&buffer[..read]).is_err() {
                return Read::Ended(Exchange::Local);
            }
        }
        drop(output);
        drop(decoder);
        // An item is one JSON value of any kind, as the renderer's reader accepts.
        let Ok(file) = File::open(&payload.path) else {
            return Read::Ended(Exchange::Local);
        };
        let parsed = if character {
            cold_keys::character_payload_keys(BufReader::new(file))
        } else {
            let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
            <serde::de::IgnoredAny as serde::Deserialize>::deserialize(&mut deserializer).and_then(|_| deserializer.end()).map(|()| Vec::new())
        };
        match parsed {
            Ok(chats) => Read::Body(Fetched::Present(payload, chats)),
            Err(error) if error.is_io() => Read::Ended(Exchange::Local),
            Err(_) => Read::Body(Fetched::Unavailable(ColdPayloadUnavailable::Damaged)),
        }
    }
}

/// The reader thread's side: which keys to read next and what each read became.
struct Prefetch<'s, 'a> {
    source: &'s ColdSource<'a>,
    keys: mpsc::Receiver<ColdKey>,
    scheduler: Scheduler,
    /// The place of the next key from the snapshot.
    order: u64,
    /// Keys not started yet, in staging's order.
    pending: BTreeMap<Position, ColdKey>,
    /// Each started key by its scheduler index, while it is read or waits for a retry.
    reading: HashMap<usize, Reading>,
    in_flight: HashMap<u64, Attempt>,
    /// The scheduler holds a fresh start not given a key yet.
    offered: bool,
    /// A key staging waits for that the snapshot scan does not name.
    unnamed: Option<String>,
    sequence: u64,
}

struct Reading {
    key: String,
    character: bool,
    position: Option<Position>,
    bytes: u64,
}

type PendingRead<'s> = LocalBoxFuture<'s, (u64, Read, u64)>;

impl<'s, 'a> Prefetch<'s, 'a> {
    fn new(source: &'s ColdSource<'a>, keys: mpsc::Receiver<ColdKey>) -> Self {
        // Cold reads keep a pause of their own, and a rejected credential ends
        // the job so the renderer can sign in again and restart the restore.
        let scheduler = Scheduler::new(SLOTS, Arc::new(Mutex::new(Pause::default())), usize::MAX, false, Box::new(super::jitter));
        Self {
            source,
            keys,
            scheduler,
            order: 0,
            pending: BTreeMap::new(),
            reading: HashMap::new(),
            in_flight: HashMap::new(),
            offered: false,
            unnamed: None,
            sequence: 0,
        }
    }

    async fn run(mut self) {
        let source = self.source;
        let mut reads = FuturesUnordered::<PendingRead<'s>>::new();
        loop {
            let cancel = source.job.is_cancel_requested();
            let closed = source.lock().closed;
            if (closed || cancel || self.scheduler.reason().is_some()) && !self.in_flight.is_empty() {
                // Dropping a read aborts its request and removes its files.
                reads = FuturesUnordered::new();
                for (_, attempt) in self.in_flight.drain() {
                    self.scheduler.abandon(attempt);
                }
            }
            if closed {
                return;
            }
            if cancel {
                source.stop(cancelled());
                return;
            }
            if let Some(reason) = self.scheduler.reason() {
                source.stop(stopped(reason));
                let _ = tokio::time::timeout(TICK, source.wake.notified()).await;
                continue;
            }
            self.receive();
            let open = self.start(&mut reads);
            let wait = self
                .scheduler
                .wake_at(open)
                .map_or(TICK, |at| TICK.min(Duration::from_millis(at.saturating_sub(super::now_ms()))));
            let finished = tokio::select! {
                Some(finished) = reads.next(), if !reads.is_empty() => Some(finished),
                _ = source.wake.notified() => None,
                _ = tokio::time::sleep(wait) => None,
            };
            if let Some((sequence, read, bytes)) = finished {
                self.finished(sequence, read, bytes);
            }
        }
    }

    /// Whether the reader already holds, reads or will read `key`. Staging
    /// reads a key named twice first at the earlier place, and that place is kept.
    fn place(&mut self, state: &mut State, key: &str, position: Position) -> bool {
        let earlier = |current: &mut Option<Position>| {
            if let Some(current) = current {
                *current = (*current).min(position);
            }
        };
        if let Some(ready) = state.ready.get_mut(key) {
            earlier(&mut ready.position);
            return true;
        }
        if let Some(reading) = self.reading.values_mut().find(|reading| reading.key == key) {
            earlier(&mut reading.position);
            return true;
        }
        let Some(current) = self.pending.iter().find(|(_, pending)| pending.key == key).map(|(current, _)| *current) else {
            return false;
        };
        if position < current {
            if let Some(pending) = self.pending.remove(&current) {
                self.pending.insert(position, pending);
            }
        }
        true
    }

    fn knows(&self, state: &State, key: &str) -> bool {
        state.ready.contains_key(key)
            || self.reading.values().any(|reading| reading.key == key)
            || self.pending.values().any(|pending| pending.key == key)
    }

    /// Skips keys staging went past and takes keys from the snapshot while the
    /// spool has places for them, or every key named so far while staging
    /// waits for a key the reader does not know.
    fn receive(&mut self) {
        let source = self.source;
        // The scan sends every key before the offset it reports.
        let scanned = source.scanned.load(Ordering::Acquire);
        let mut state = source.lock();
        if let Some(taken) = state.taken {
            // Staging reads such a key itself if it still needs it.
            let demand = state.demand.clone();
            self.pending.retain(|position, pending| *position > taken || demand.as_deref() == Some(pending.key.as_str()));
        }
        let unknown = state.demand.clone().filter(|key| !self.knows(&state, key));
        while unknown.is_some() || self.pending.len() < source.spool.items {
            let Ok(key) = self.keys.try_recv() else { break };
            let position = (self.order, 0);
            self.order += 1;
            if !self.place(&mut state, &key.key, position) {
                self.pending.insert(position, key);
            }
        }
        // Staging has read every block that names a key it asks for, so a key the
        // scan has not named by staging's place is one the scan does not name.
        self.unnamed = unknown.filter(|key| !self.knows(&state, key) && scanned >= source.progress.source.load(Ordering::Acquire));
    }

    /// Starts every read the slots allow: the item staging waits for first,
    /// then the next pending key while the spool has room. Returns whether reads may start.
    fn start(&mut self, reads: &mut FuturesUnordered<PendingRead<'s>>) -> bool {
        let source = self.source;
        let now = super::now_ms();
        loop {
            let (room, demand) = {
                let state = source.lock();
                let room = state.ready.len() + self.scheduler.active() < source.spool.items && state.ready_bytes < source.spool.bytes;
                (room, state.demand.clone().filter(|key| !state.ready.contains_key(key)))
            };
            // The item staging waits for starts whatever the spool holds.
            let open = room || demand.is_some();
            let unstarted = demand.filter(|key| {
                !self.reading.values().any(|reading| &reading.key == key)
                    && (self.unnamed.as_ref() == Some(key) || self.pending.values().any(|pending| &pending.key == key))
            });
            if !self.offered && (unstarted.is_some() || (room && !self.pending.is_empty())) {
                self.scheduler.receive(1);
                self.offered = true;
            }
            let Some(attempt) = self.scheduler.next(now, open) else { return open };
            if !self.reading.contains_key(&attempt.index) {
                // A fresh start takes whichever key comes first when it starts.
                self.offered = false;
                let reading = match unstarted {
                    Some(key) => {
                        let position = self.pending.iter().find(|(_, pending)| pending.key == key).map(|(position, _)| *position);
                        let character = position.and_then(|position| self.pending.remove(&position)).is_some_and(|pending| pending.character);
                        Reading { key, character, position, bytes: 0 }
                    }
                    None => match if room { self.pending.pop_first() } else { None } {
                        Some((position, pending)) => Reading { key: pending.key, character: pending.character, position: Some(position), bytes: 0 },
                        None => {
                            self.scheduler.abandon(attempt);
                            continue;
                        }
                    },
                };
                self.reading.insert(attempt.index, reading);
            }
            let reading = &self.reading[&attempt.index];
            self.sequence += 1;
            let (sequence, key, character) = (self.sequence, reading.key.clone(), reading.character);
            let path = source.reader.directory.join(format!("cold-{sequence}.json"));
            self.in_flight.insert(sequence, attempt);
            reads.push(
                async move {
                    let (read, bytes) = source.read(&key, path, character).await;
                    (sequence, read, bytes)
                }
                .boxed_local(),
            );
        }
    }

    fn finished(&mut self, sequence: u64, read: Read, bytes: u64) {
        let Some(attempt) = self.in_flight.remove(&sequence) else { return };
        let index = attempt.index;
        if let Some(reading) = self.reading.get_mut(&index) {
            reading.bytes += bytes;
        }
        let (exchange, fetched) = match read {
            Read::Cancelled => {
                self.scheduler.abandon(attempt);
                return;
            }
            Read::Body(fetched) => (Exchange::Body, Some(fetched)),
            Read::Ended(exchange) => (exchange, None),
        };
        let (item, chats) = match (self.scheduler.finish(attempt, exchange, super::now_ms()), fetched) {
            (Outcome::Install, Some(Fetched::Present(payload, chats))) => (Item::Present(payload), chats),
            (Outcome::Install, Some(Fetched::Unavailable(reason))) => (Item::Unavailable(reason), Vec::new()),
            (Outcome::Settled(Settled::Missing), _) => (Item::Missing, Vec::new()),
            (Outcome::Settled(_), _) if self.scheduler.reason().is_none() => {
                (Item::Unavailable(ColdPayloadUnavailable::Damaged), Vec::new())
            }
            // A stop leaves the item unread, and staging fails when it asks for it.
            _ => return,
        };
        self.settle(index, item, chats);
    }

    fn settle(&mut self, index: usize, item: Item, chats: Vec<String>) {
        let Some(reading) = self.reading.remove(&index) else { return };
        let source = self.source;
        let mut state = source.lock();
        // Staging reads a character's chats right after the character.
        if let (true, Some((order, _))) = (reading.character, reading.position) {
            for (offset, key) in (1..).zip(chats) {
                if key != reading.key && !self.place(&mut state, &key, (order, offset)) {
                    self.pending.insert((order, offset), ColdKey { key, character: false });
                }
            }
        }
        let ready = Ready { item, bytes: reading.bytes, position: reading.position };
        state.ready_bytes += ready.size();
        if let Some(replaced) = state.ready.insert(reading.key, ready) {
            state.ready_bytes -= replaced.size();
        }
        drop(state);
        source.settled.notify_all();
    }
}

/// Ends the reader once staging is done, also when it unwinds.
struct Close<'s, 'a>(&'s ColdSource<'a>, &'s AtomicBool);

impl Drop for Close<'_, '_> {
    fn drop(&mut self) {
        self.1.store(true, Ordering::Release);
        self.0.lock().closed = true;
        self.0.wake.notify_one();
    }
}

/// Empties the spool when the reader ends, and fails staging's wait if it ends first.
struct ReaderExit<'s, 'a>(&'s ColdSource<'a>);

impl Drop for ReaderExit<'_, '_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        if state.closed {
            state.previous = None;
        } else {
            state.stopped.get_or_insert_with(|| NativeJobError::new("store-error", "Official account cold item reader stopped"));
        }
        state.ready.clear();
        state.ready_bytes = 0;
        drop(state);
        self.0.settled.notify_all();
    }
}

/// Lets staging read every key itself once the scan stops.
struct Scanned<'s, 'a>(&'s ColdSource<'a>);

impl Drop for Scanned<'_, '_> {
    fn drop(&mut self) {
        self.0.scanned.store(u64::MAX, Ordering::Release);
        self.0.wake.notify_one();
    }
}

/// Ends the snapshot scan once staging is done.
struct Stoppable<'s, R> {
    inner: R,
    stop: &'s AtomicBool,
}

impl<R: std::io::Read> std::io::Read for Stoppable<'_, R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.stop.load(Ordering::Acquire) {
            return Err(std::io::Error::other("snapshot scan stopped"));
        }
        self.inner.read(buffer)
    }
}

fn cancelled() -> NativeJobError {
    NativeJobError::new("cancelled", "Official account cold item read was cancelled")
}

fn stopped(reason: StopReason) -> NativeJobError {
    match reason {
        StopReason::Authentication | StopReason::ReauthenticationNeeded | StopReason::AuthWarning => {
            NativeJobError::new("reauthentication-needed", "Official account authorization failed")
        }
        StopReason::Local | StopReason::LibraryChanged => {
            NativeJobError::new("store-error", "Official account cold item could not be stored")
        }
        StopReason::Service => NativeJobError::new("network-error", "Official account cold storage is not responding"),
    }
}

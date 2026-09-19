//! Shared in-memory provider and injectable test doubles for synthetic
//! core/adapter tests only. Nothing here is compiled into the product.
use super::{
    auth::{SecretBytes, SecretVault},
    capabilities::*,
    contract::*,
    http::{Clock, HttpTransport, MyboxRequestBudget, NativeHttpTransport, RequestState},
    providers::Dependencies,
    quota::AccountKey,
    quota_profiles::MyboxCharge,
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
};
/// The one mutable head, refused by every delete.
const HEAD_OBJECT: &str = "head";

pub(super) fn capabilities(cas: bool) -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        atomic_create_head: cas,
        conditional_head_update: cas,
        stable_head_replace: true,
        head_read_after_write: true,
        head_retry_control: true,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        ..Default::default()
    }
}

/// The same repository without synchronous removal. Publication and finite
/// work protection stay available.
pub(super) fn capabilities_without_cleanup(cas: bool) -> Capabilities {
    Capabilities {
        delete_objects: false,
        ..capabilities(cas)
    }
}

fn take_scheduled(
    scheduled: &mut Vec<(usize, String, ObjectRole, Vec<u8>)>,
    reached: usize,
) -> Vec<(String, ObjectRole, Vec<u8>)> {
    let mut taken = Vec::new();
    scheduled.retain(|(at, object, role, bytes)| {
        if *at == reached {
            taken.push((object.clone(), *role, bytes.clone()));
            return false;
        }
        true
    });
    taken
}

/// What a delete of one named target does instead of removing it. Ambiguous
/// outcomes are reachable because a cleanup has to survive them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeleteFault {
    /// The request never reached the service.
    Transient,
    /// No answer arrives at all; only cancellation ends the wait.
    Unanswered,
    /// The remote object is gone, but the answer was lost on the way back.
    AppliedThenLost,
    /// The service answered that its request budget is exhausted.
    RateLimited,
    Accepted,
    NotFound,
    Unauthorized,
}

#[derive(Default)]
pub(super) struct FakeState {
    pub(super) objects: BTreeMap<String, (Vec<u8>, u64)>,
    pub(super) next_version: u64,
    pub(super) lose_response: bool,
    roles: BTreeMap<String, ObjectRole>,
    deletes: BTreeMap<String, DeleteFault>,
    delete_attempts: Vec<String>,
    answered_deletes: usize,
    listings: usize,
    scheduled: Vec<(usize, String, ObjectRole, Vec<u8>)>,
    after_listing: Vec<(usize, String, ObjectRole, Vec<u8>)>,
    reads: BTreeMap<String, ProviderError>,
    read_attempts: Vec<String>,
    pub(super) ignore_unchanged: bool,
    pub(super) body_bytes: BTreeMap<String, usize>,
    cancel_after_reads: BTreeMap<String, (usize, Cancellation)>,
    upload_attempts: Vec<String>,
    upload_locators: BTreeMap<String, String>,
    reconcile_attempts: Vec<String>,
    scripted_pages: Vec<(Collection, Result<ObjectPage>)>,
    inventory_upload_failure: Option<ErrorKind>,
}
pub(crate) struct FakeProvider {
    pub(super) state: Mutex<FakeState>,
    cas: bool,
}
impl FakeProvider {
    pub(crate) fn new(cas: bool) -> Self {
        Self {
            state: Mutex::new(FakeState::default()),
            cas,
        }
    }
    /// Places an object of a role directly, so a listing of any collection can
    /// be arranged without going through an upload.
    pub(crate) fn seed(&self, object: &str, role: ObjectRole, bytes: Vec<u8>) {
        let mut state = self.state.lock().unwrap();
        state.next_version += 1;
        let version = state.next_version;
        state.objects.insert(object.to_owned(), (bytes, version));
        state.roles.insert(object.to_owned(), role);
    }
    /// Places an object once a given number of removals have been answered,
    /// which is how another device arrives in the middle of a removal.
    pub(crate) fn seed_after_delete(
        &self,
        answered: usize,
        object: &str,
        role: ObjectRole,
        bytes: Vec<u8>,
    ) {
        self.state
            .lock()
            .unwrap()
            .scheduled
            .push((answered, object.to_owned(), role, bytes));
    }
    /// Places an object once a given number of enumerations have answered,
    /// which is how another device arrives between two readings.
    pub(crate) fn seed_after_list(
        &self,
        listings: usize,
        object: &str,
        role: ObjectRole,
        bytes: Vec<u8>,
    ) {
        self.state
            .lock()
            .unwrap()
            .after_listing
            .push((listings, object.to_owned(), role, bytes));
    }
    pub(crate) fn fail_delete(&self, object: &str, fault: DeleteFault) {
        self.state
            .lock()
            .unwrap()
            .deletes
            .insert(object.to_owned(), fault);
    }
    pub(crate) fn holds(&self, object: &str) -> bool {
        self.state.lock().unwrap().objects.contains_key(object)
    }
    pub(crate) fn delete_attempts(&self, object: &str) -> usize {
        self.state
            .lock()
            .unwrap()
            .delete_attempts
            .iter()
            .filter(|attempt| attempt.as_str() == object)
            .count()
    }
    pub(crate) fn fail_read(&self, object: &str, kind: ErrorKind) {
        self.state.lock().unwrap().reads.insert(object.into(), ProviderError::new(kind));
    }
    pub(crate) fn read_attempts(&self, object: &str) -> usize {
        self.state.lock().unwrap().read_attempts.iter().filter(|id| id.as_str() == object).count()
    }
    pub(crate) fn cancel_after_read(
        &self,
        object: &str,
        attempts: usize,
        cancellation: &Cancellation,
    ) {
        self.state.lock().unwrap().cancel_after_reads.insert(
            object.into(),
            (attempts, cancellation.clone()),
        );
    }
    pub(crate) fn upload_attempts(&self, object: &str) -> usize {
        self.state.lock().unwrap().upload_attempts.iter().filter(|id| id.as_str() == object).count()
    }
    pub(crate) fn fail_inventory_upload(&self, kind: ErrorKind) {
        self.state.lock().unwrap().inventory_upload_failure = Some(kind);
    }
    pub(crate) fn set_upload_locator(&self, object_id: &str, locator: &str) {
        self.state.lock().unwrap().upload_locators.insert(object_id.into(), locator.into());
    }
    pub(crate) fn uploaded_ids(&self) -> Vec<String> {
        self.state.lock().unwrap().upload_attempts.clone()
    }
    pub(crate) fn listing_count(&self) -> usize {
        self.state.lock().unwrap().listings
    }
    pub(crate) fn deletion_order(&self) -> Vec<String> {
        self.state.lock().unwrap().delete_attempts.clone()
    }
    pub(crate) fn reconcile_attempts(&self, object: &str) -> usize {
        self.state.lock().unwrap().reconcile_attempts.iter().filter(|id| id.as_str() == object).count()
    }
    pub(crate) fn script_page(&self, collection: Collection, page: Result<ObjectPage>) {
        self.state.lock().unwrap().scripted_pages.push((collection, page));
    }
    pub(crate) fn forget(&self, object: &str) {
        let mut state = self.state.lock().unwrap();
        state.objects.remove(object);
        state.roles.remove(object);
    }
    /// Counts one answered removal and places whatever was scheduled for that
    /// point.
    fn answered(&self) {
        let pending = {
            let mut state = self.state.lock().unwrap();
            state.answered_deletes += 1;
            let reached = state.answered_deletes;
            take_scheduled(&mut state.scheduled, reached)
        };
        for (object, role, bytes) in pending {
            self.seed(&object, role, bytes);
        }
    }
    fn listed(&self) {
        let pending = {
            let mut state = self.state.lock().unwrap();
            state.listings += 1;
            let reached = state.listings;
            take_scheduled(&mut state.after_listing, reached)
        };
        for (object, role, bytes) in pending {
            self.seed(&object, role, bytes);
        }
    }
    pub(super) fn write(
        &self,
        locator: &RemoteLocator,
        expected: Option<&ExpectedHead>,
        bytes: &[u8],
    ) -> Result<HeadReceipt> {
        if expected.is_some() && !self.cas {
            return Err(ProviderError::new(ErrorKind::Unsupported));
        }
        let mut state = self.state.lock().unwrap();
        let previous = state.objects.get(&locator.object);
        if let Some(expected) = expected {
            let matches = match (expected, previous) {
                (ExpectedHead::Absent, None) => true,
                (ExpectedHead::Exact(token), Some((_, v))) => token.0 == v.to_string(),
                _ => false,
            };
            if !matches {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        state.next_version += 1;
        let version = state.next_version;
        state
            .objects
            .insert(locator.object.clone(), (bytes.into(), version));
        if std::mem::take(&mut state.lose_response) {
            return Err(ProviderError::new(ErrorKind::Transient));
        }
        Ok(HeadReceipt {
            version: Some(VersionToken(version.to_string())),
            complete: true,
        })
    }
}
impl Provider for FakeProvider {
    fn open_repository<'a>(
        &'a self,
        _: &'a ConnectionConfig,
        _: &'a SecretRef,
        _: OpenMode,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            c.check()?;
            Ok((repository(), capabilities(self.cas)))
        })
    }
    fn read_object<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            use tokio::io::AsyncWriteExt;
            c.check()?;
            l.validate_for(r)?;
            let cancel_after_read = {
                let mut state = self.state.lock().unwrap();
                state.read_attempts.push(l.object.clone());
                let attempts = state.read_attempts.iter()
                    .filter(|object| *object == &l.object)
                    .count();
                let cancel = state.cancel_after_reads.get(&l.object)
                    .filter(|(after, _)| *after == attempts)
                    .map(|(_, cancel)| cancel.clone());
                if let Some(error) = state.reads.remove(&l.object) {
                    return Err(error);
                }
                cancel
            };
            if let Some(cancel) = cancel_after_read {
                cancel.cancel();
            }
            let (bytes, version) = self
                .state
                .lock()
                .unwrap()
                .objects
                .get(&l.object)
                .cloned()
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            let token = VersionToken(version.to_string());
            if unchanged == Some(&token) && !self.state.lock().unwrap().ignore_unchanged {
                return Ok(ReadReceipt::NotModified(token));
            }
            *self.state.lock().unwrap().body_bytes.entry(l.object.clone()).or_default() += bytes.len();
            let hash = risunest_sync_wire::hash(&bytes);
            let mut writer = sink.open(0, bytes.len() as u64, c).await?;
            writer
                .write_all(&bytes)
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            writer
                .shutdown()
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            drop(writer);
            sink.finish(bytes.len() as u64, &hash).await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: l.clone(),
                byte_length: bytes.len() as u64,
                version: Some(token),
                checksum: None,
                complete: true,
            }))
        })
    }
    fn begin_upload<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move {
            c.check()?;
            intent.validate(r)?;
            Ok(None)
        })
    }
    fn create_object<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        _: Option<&'a ResumeState>,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            use tokio::io::AsyncReadExt;
            c.check()?;
            intent.validate(r)?;
            self.state.lock().unwrap().upload_attempts.push(intent.object_id.clone());
            if intent.role == ObjectRole::InventoryPage {
                if let Some(kind) = self.state.lock().unwrap().inventory_upload_failure.take() {
                    return Err(ProviderError::new(kind));
                }
            }
            if intent.byte_length > 1024 * 1024 || source.byte_length() != intent.byte_length {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let mut bytes = Vec::new();
            source
                .open(0, intent.byte_length, c)
                .await?
                .take(intent.byte_length + 1)
                .read_to_end(&mut bytes)
                .await
                .map_err(|_| ProviderError::new(ErrorKind::Transient))?;
            if bytes.len() as u64 != intent.byte_length
                || risunest_sync_wire::hash(&bytes) != intent.sha256
            {
                return Err(ProviderError::new(ErrorKind::Corrupt));
            }
            let receipt = {
                let mut state = self.state.lock().unwrap();
                let remote_id = state.upload_locators.get(&intent.object_id)
                    .cloned().unwrap_or_else(|| intent.object_id.clone());
                if let Some((old, _)) = state.objects.get(&remote_id) {
                    if old != &bytes || state.roles.get(&remote_id) != Some(&intent.role) {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                } else {
                    state.next_version += 1;
                    let v = state.next_version;
                    state.objects.insert(remote_id.clone(), (bytes, v));
                    state.roles.insert(remote_id.clone(), intent.role);
                }
                let (_, v) = state.objects.get(&remote_id).unwrap();
                ObjectReceipt {
                    locator: RemoteLocator {
                        connection_identity: r.connection_identity.clone(), collection: None, object: remote_id,
                    },
                    byte_length: intent.byte_length,
                    version: Some(VersionToken(v.to_string())),
                    checksum: None,
                    complete: true,
                }
            };
            if std::mem::take(&mut self.state.lock().unwrap().lose_response) {
                return Err(ProviderError::new(ErrorKind::Transient));
            }
            Ok(receipt)
        })
    }
    fn compare_exchange_head<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        e: &'a ExpectedHead,
        h: &'a HeadBytes,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            c.check()?;
            l.validate_for(r)?;
            self.write(l, Some(e), h.as_bytes())
        })
    }
    fn replace_head<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        h: &'a HeadBytes,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            c.check()?;
            l.validate_for(r)?;
            self.write(l, None, h.as_bytes())
        })
    }
    fn delete_object<'a>(
        &'a self,
        r: &'a RepositoryHandle,
        l: &'a RemoteLocator,
        c: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            c.check()?;
            l.validate_for(r)?;
            let refused = l.object == HEAD_OBJECT
                || self.state.lock().unwrap().roles.get(&l.object) == Some(&ObjectRole::Descriptor);
            if refused {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let injected = {
                let mut state = self.state.lock().unwrap();
                state.delete_attempts.push(l.object.clone());
                state.deletes.remove(&l.object)
            };
            match injected {
                Some(DeleteFault::Transient) => Err(ProviderError::new(ErrorKind::Transient)),
                Some(DeleteFault::Unanswered) => {
                    c.cancelled().await;
                    Err(ProviderError::new(ErrorKind::Cancelled))
                }
                Some(DeleteFault::AppliedThenLost) => {
                    self.forget(&l.object);
                    Err(ProviderError::new(ErrorKind::Transient))
                }
                Some(DeleteFault::RateLimited) => Err(ProviderError {
                    kind: ErrorKind::RateLimited,
                    http_status: Some(429),
                    retry_at_ms: None,
                }),
                Some(DeleteFault::Accepted) => Err(ProviderError {
                    kind: ErrorKind::Unsupported, http_status: Some(202), retry_at_ms: None,
                }),
                Some(DeleteFault::NotFound) => {
                    self.forget(&l.object);
                    Err(ProviderError { kind: ErrorKind::NotFound, http_status: Some(404), retry_at_ms: None })
                }
                Some(DeleteFault::Unauthorized) => Err(ProviderError {
                    kind: ErrorKind::Unauthorized, http_status: Some(401), retry_at_ms: None,
                }),
                // A target that is already gone answers the same as one removed now.
                None => {
                    self.forget(&l.object);
                    self.answered();
                    Ok(())
                }
            }
        })
    }
    fn list_objects<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        collection: Collection,
        cursor: Option<&'a str>,
        limit: u16,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectPage> {
        Box::pin(async move {
            cancel.check()?;
            if limit == 0 || limit > 1000 {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let scripted = {
                let mut state = self.state.lock().unwrap();
                state.scripted_pages.iter().position(|(kind, _)| *kind == collection)
                    .map(|index| state.scripted_pages.remove(index).1)
            };
            if let Some(page) = scripted {
                self.listed();
                return page;
            }
            // A published state and a backup bundle share the snapshot listing,
            // which is what every adapter answers.
            let roles: &[ObjectRole] = match collection {
                Collection::Snapshots => &[ObjectRole::SyncState, ObjectRole::BackupBundle],
                Collection::BackupPoints => &[ObjectRole::BackupPoint],
                Collection::InventoryPages => &[ObjectRole::InventoryPage],
                Collection::Descriptors => &[ObjectRole::Descriptor],
                Collection::Leases => &[ObjectRole::Lease],
            };
            let state = self.state.lock().unwrap();
            let mut matches = state.objects.iter().filter(|(id, _)| {
                state.roles.get(*id).is_some_and(|role| roles.contains(role))
                    && cursor.is_none_or(|cursor| id.as_str() > cursor)
            });
            let objects: Vec<_> = matches
                .by_ref()
                .take(limit as usize)
                .map(|(id, (bytes, version))| ObjectReceipt {
                    locator: RemoteLocator {
                        connection_identity: repository.connection_identity.clone(),
                        collection: None,
                        object: id.clone(),
                    },
                    byte_length: bytes.len() as u64,
                    version: Some(VersionToken(version.to_string())),
                    checksum: None,
                    complete: true,
                })
                .collect();
            let next_cursor = if matches.next().is_some() {
                objects.last().map(|object| object.locator.object.clone())
            } else {
                None
            };
            drop(state);
            self.listed();
            Ok(ObjectPage {
                objects,
                next_cursor,
            })
        })
    }
    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        _: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async move {
            cancel.check()?;
            intent.validate(repository)?;
            let mut state = self.state.lock().unwrap();
            state.reconcile_attempts.push(intent.object_id.clone());
            let remote_id = state.upload_locators.get(&intent.object_id).unwrap_or(&intent.object_id);
            let Some((bytes, version)) = state.objects.get(remote_id) else {
                return Ok(UploadResolution::RestartRequired);
            };
            if bytes.len() as u64 != intent.byte_length
                || risunest_sync_wire::hash(bytes) != intent.sha256
                || state.roles.get(remote_id) != Some(&intent.role)
            {
                return Ok(UploadResolution::Conflict);
            }
            Ok(UploadResolution::Complete(ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: repository.connection_identity.clone(),
                    collection: None,
                    object: remote_id.clone(),
                },
                byte_length: intent.byte_length,
                version: Some(VersionToken(version.to_string())),
                checksum: None,
                complete: true,
            }))
        })
    }
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        Ok(RemoteLocator {
            connection_identity: repository.connection_identity.clone(),
            collection: None,
            object: HEAD_OBJECT.into(),
        })
    }
}
pub(crate) fn repository() -> RepositoryHandle {
    RepositoryHandle {
        repository_id: "synthetic-repository".into(),
        connection_identity: "synthetic-account/root".into(),
        account: AccountKey::new(
            "fake",
            &url::Url::parse("https://synthetic.invalid").expect("valid fake endpoint"),
            "synthetic-account",
        )
        .expect("valid fake account"),
        context: Box::new(()),
    }
}
pub(crate) fn locator() -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository().connection_identity,
        collection: None,
        object: HEAD_OBJECT.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(name: &str) -> RemoteLocator {
        RemoteLocator {
            connection_identity: repository().connection_identity,
            collection: None,
            object: name.into(),
        }
    }

    #[test]
    fn deleting_is_idempotent_and_refuses_the_head_and_a_descriptor() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let provider = FakeProvider::new(false);
            let handle = repository();
            let cancel = Cancellation::default();
            provider.seed("pack-a", ObjectRole::Pack, vec![1, 2, 3]);
            provider.seed("descriptor-a", ObjectRole::Descriptor, vec![4]);
            provider.seed(HEAD_OBJECT, ObjectRole::SyncState, vec![5]);

            provider
                .delete_object(&handle, &object("pack-a"), &cancel)
                .await
                .unwrap();
            assert!(!provider.holds("pack-a"));
            // A second attempt at the same target still succeeds.
            provider
                .delete_object(&handle, &object("pack-a"), &cancel)
                .await
                .unwrap();

            for refused in [HEAD_OBJECT, "descriptor-a"] {
                assert_eq!(
                    provider
                        .delete_object(&handle, &object(refused), &cancel)
                        .await
                        .unwrap_err()
                        .kind,
                    ErrorKind::Unsupported
                );
                assert!(provider.holds(refused));
            }
        });
    }

    #[test]
    fn leases_enumerate_on_their_own_and_page_where_the_caller_asks() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let provider = FakeProvider::new(false);
            let handle = repository();
            let cancel = Cancellation::default();
            let tags = ["11111111111111111111111111111111", "22222222222222222222222222222222"];
            let work = lease_object_id(LeaseKind::Work, tags[0]).unwrap();
            let deleting = lease_object_id(LeaseKind::Deleting, tags[1]).unwrap();
            for name in [&work, &deleting] {
                provider.seed(name, ObjectRole::Lease, vec![1]);
            }
            provider.seed("state-a", ObjectRole::SyncState, vec![2]);

            let first = provider
                .list_objects(&handle, Collection::Leases, None, 1, &cancel)
                .await
                .unwrap();
            assert_eq!(first.objects.len(), 1);
            let second = provider
                .list_objects(&handle, Collection::Leases, first.next_cursor.as_deref(), 10, &cancel)
                .await
                .unwrap();
            let mut seen: Vec<String> = first
                .objects
                .iter()
                .chain(second.objects.iter())
                .map(|object| object.locator.object.clone())
                .collect();
            seen.sort();
            assert_eq!(seen, vec![deleting.clone(), work.clone()]);
            assert_eq!(second.next_cursor, None);

            // A lease never shows up in another collection's listing.
            let snapshots = provider
                .list_objects(&handle, Collection::Snapshots, None, 10, &cancel)
                .await
                .unwrap();
            assert_eq!(snapshots.objects.len(), 1);
            assert_eq!(snapshots.objects[0].locator.object, "state-a");

            // Leases are removable; that is the whole point of the collection.
            provider
                .delete_object(
                    &handle,
                    &object(&work),
                    &cancel,
                )
                .await
                .unwrap();
            assert!(!provider.holds(&work));
        });
    }

    #[test]
    fn injected_delete_faults_cover_answers_silence_and_a_lost_answer() {
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let provider = FakeProvider::new(false);
            let handle = repository();
            let cancel = Cancellation::default();
            for name in ["pack-a", "pack-b", "pack-c"] {
                provider.seed(name, ObjectRole::Pack, vec![7]);
            }

            provider.fail_delete("pack-a", DeleteFault::Transient);
            assert_eq!(
                provider
                    .delete_object(&handle, &object("pack-a"), &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Transient
            );
            assert!(provider.holds("pack-a"));

            provider.fail_delete("pack-b", DeleteFault::AppliedThenLost);
            assert_eq!(
                provider
                    .delete_object(&handle, &object("pack-b"), &cancel)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Transient
            );
            assert!(!provider.holds("pack-b"));

            provider.fail_delete("pack-c", DeleteFault::Unanswered);
            let pending = Cancellation::default();
            let signal = pending.clone();
            let stopper = tokio::spawn(async move {
                tokio::task::yield_now().await;
                signal.cancel();
            });
            assert_eq!(
                provider
                    .delete_object(&handle, &object("pack-c"), &pending)
                    .await
                    .unwrap_err()
                    .kind,
                ErrorKind::Cancelled
            );
            stopper.await.unwrap();
            // Cancelling the local request is no evidence the remote end ran.
            assert!(provider.holds("pack-c"));

            provider.seed("pack-d", ObjectRole::Pack, vec![7]);
            provider.fail_delete("pack-d", DeleteFault::RateLimited);
            let answered = provider
                .delete_object(&handle, &object("pack-d"), &cancel)
                .await
                .unwrap_err();
            assert_eq!(answered.kind, ErrorKind::RateLimited);
            assert_eq!(answered.http_status, Some(429));
            assert_eq!(provider.delete_attempts("pack-d"), 1);

            // Only the injected attempt is affected; the retry behaves normally.
            provider
                .delete_object(&handle, &object("pack-a"), &cancel)
                .await
                .unwrap();
            assert!(!provider.holds("pack-a"));
        });
    }
}

/// Test-only secret store. Missing references surface as `ReauthRequired`,
/// which is what a product vault reports for a revoked or lost credential.
#[derive(Default)]
pub(crate) struct MemoryVault {
    secrets: Mutex<BTreeMap<String, Vec<u8>>>,
    next: AtomicU64,
}
impl MemoryVault {
    pub(crate) fn with(reference: &str, bytes: &[u8]) -> Self {
        let vault = Self::default();
        vault
            .secrets
            .lock()
            .unwrap()
            .insert(reference.into(), bytes.into());
        vault
    }
    pub(crate) fn contents(&self, reference: &str) -> Option<Vec<u8>> {
        self.secrets.lock().unwrap().get(reference).cloned()
    }
}
impl SecretVault for MemoryVault {
    fn read<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, SecretBytes> {
        Box::pin(async move {
            self.secrets
                .lock()
                .unwrap()
                .get(&reference.0)
                .map(|bytes| SecretBytes(zeroize::Zeroizing::new(bytes.clone())))
                .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))
        })
    }
    fn store<'a>(&'a self, bytes: &'a SecretBytes) -> ProviderFuture<'a, SecretRef> {
        Box::pin(async move {
            let reference = format!("secret-{}", self.next.fetch_add(1, Ordering::SeqCst));
            self.secrets
                .lock()
                .unwrap()
                .insert(reference.clone(), bytes.0.to_vec());
            Ok(SecretRef(reference))
        })
    }
    fn replace<'a>(
        &'a self,
        reference: &'a SecretRef,
        bytes: &'a SecretBytes,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            let mut secrets = self.secrets.lock().unwrap();
            let slot = secrets
                .get_mut(&reference.0)
                .ok_or_else(|| ProviderError::new(ErrorKind::ReauthRequired))?;
            *slot = bytes.0.to_vec();
            Ok(())
        })
    }
    fn remove<'a>(&'a self, reference: &'a SecretRef) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            self.secrets.lock().unwrap().remove(&reference.0);
            Ok(())
        })
    }
}

/// Wall time, elapsed time and lifecycle can advance independently without sleeps.
pub(crate) struct FakeLeaseClock(Mutex<super::leases::ClockReading>);
impl FakeLeaseClock {
    pub(crate) fn new(wall_ms: u64) -> Self {
        Self(Mutex::new(super::leases::ClockReading {
            wall_ms, monotonic_ms: 0, epoch: 0, foreground: true, trusted: true,
        }))
    }
    pub(crate) fn advance(&self, milliseconds: u64) {
        let mut now = self.0.lock().unwrap();
        now.wall_ms = now.wall_ms.checked_add(milliseconds).unwrap();
        now.monotonic_ms = now.monotonic_ms.checked_add(milliseconds).unwrap();
    }
    pub(crate) fn set_trusted(&self, trusted: bool) {
        self.0.lock().unwrap().trusted = trusted;
    }
    pub(crate) fn suspend(&self) {
        let mut now = self.0.lock().unwrap();
        now.epoch += 1;
        now.foreground = false;
        now.trusted = false;
    }
    pub(crate) fn foreground(&self) {
        self.0.lock().unwrap().foreground = true;
    }
}
impl super::leases::LeaseClock for FakeLeaseClock {
    fn reading(&self) -> super::leases::ClockReading {
        *self.0.lock().unwrap()
    }
}

pub(crate) struct FixedClock(AtomicU64);
impl FixedClock {
    pub(crate) fn at(now_ms: u64) -> Self {
        Self(AtomicU64::new(now_ms))
    }
    pub(crate) fn set(&self, now_ms: u64) {
        self.0.store(now_ms, Ordering::SeqCst);
    }
}
impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Records actual MYBOX reservations. Other providers have no synthetic
/// request-cost ledger.
#[derive(Default)]
pub(crate) struct RecordingBudget {
    pub(crate) reservations: Mutex<Vec<(AccountKey, MyboxCharge, u64)>>,
    pub(crate) deny: AtomicBool,
}
impl MyboxRequestBudget for RecordingBudget {
    fn reserve_mybox<'a>(
        &'a self,
        account: &'a AccountKey,
        charge: &'a MyboxCharge,
        now_ms: u64,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            self.reservations
                .lock()
                .unwrap()
                .push((account.clone(), charge.clone(), now_ms));
            if self.deny.load(Ordering::SeqCst) {
                Err(ProviderError::new(ErrorKind::DailyQuotaExhausted))
            } else {
                Ok(())
            }
        })
    }
}

pub(crate) struct TestDependencies {
    pub(crate) dependencies: Dependencies,
    pub(crate) budget: Arc<RecordingBudget>,
    pub(crate) clock: Arc<FixedClock>,
    pub(crate) vault: Arc<MemoryVault>,
}
/// Loopback HTTP against `wire_fixture::WireServer`, a fixed clock and a
/// recording budget. Adapter factories receive `dependencies`.
pub(crate) fn loopback_dependencies(vault: MemoryVault, now_ms: u64) -> TestDependencies {
    with_transport(
        Arc::new(NativeHttpTransport::for_loopback_tests()),
        vault,
        now_ms,
    )
}
pub(crate) fn with_transport(
    http: Arc<dyn HttpTransport>,
    vault: MemoryVault,
    now_ms: u64,
) -> TestDependencies {
    let budget = Arc::new(RecordingBudget::default());
    let clock = Arc::new(FixedClock::at(now_ms));
    let vault = Arc::new(vault);
    TestDependencies {
        dependencies: Dependencies {
            http,
            mybox_budget: budget.clone(),
            requests: Arc::new(RequestState::default()),
            clock: clock.clone(),
            vault: vault.clone(),
        },
        budget,
        clock,
        vault,
    }
}

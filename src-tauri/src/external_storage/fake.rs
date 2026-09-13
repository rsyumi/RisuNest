//! Shared in-memory provider for synthetic core/adapter tests only.
use super::{capabilities::*, contract::*};
use std::{collections::BTreeMap, sync::Mutex};
pub(super) fn capabilities(cas: bool) -> Capabilities {
    Capabilities {
        immutable_create: Evidence::Synthetic,
        direct_complete_read: Evidence::Synthetic,
        atomic_create_head: if cas {
            Evidence::Synthetic
        } else {
            Evidence::Unverified
        },
        conditional_head_update: if cas {
            Evidence::Synthetic
        } else {
            Evidence::Unverified
        },
        stable_head_replace: Evidence::Synthetic,
        head_read_after_write: Evidence::Synthetic,
        head_retry_control: Evidence::Synthetic,
        snapshot_discovery: Evidence::Synthetic,
        ..Default::default()
    }
}

#[derive(Default)]
pub(super) struct FakeState {
    pub(super) objects: BTreeMap<String, (Vec<u8>, u64)>,
    pub(super) next_version: u64,
    pub(super) lose_response: bool,
    roles: BTreeMap<String, ObjectRole>,
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
            let (bytes, version) = self
                .state
                .lock()
                .unwrap()
                .objects
                .get(&l.object)
                .cloned()
                .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
            let token = VersionToken(version.to_string());
            if unchanged == Some(&token) {
                return Ok(ReadReceipt::NotModified(token));
            }
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
            let locator = RemoteLocator {
                connection_identity: r.connection_identity.clone(),
                collection: None,
                object: intent.object_id.clone(),
            };
            let receipt = {
                let mut state = self.state.lock().unwrap();
                if let Some((old, _)) = state.objects.get(&intent.object_id) {
                    if old != &bytes || state.roles.get(&intent.object_id) != Some(&intent.role) {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                } else {
                    state.next_version += 1;
                    let v = state.next_version;
                    state.objects.insert(intent.object_id.clone(), (bytes, v));
                    state.roles.insert(intent.object_id.clone(), intent.role);
                }
                let (_, v) = state.objects.get(&intent.object_id).unwrap();
                ObjectReceipt {
                    locator,
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
            let role = match collection {
                Collection::Snapshots => ObjectRole::Snapshot,
                Collection::BackupPoints => ObjectRole::BackupPoint,
                Collection::Descriptors => ObjectRole::Descriptor,
            };
            let state = self.state.lock().unwrap();
            let mut matches = state.objects.iter().filter(|(id, _)| {
                state.roles.get(*id) == Some(&role)
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
        _: &'a ResumeState,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async move {
            cancel.check()?;
            intent.validate(repository)?;
            let state = self.state.lock().unwrap();
            let Some((bytes, version)) = state.objects.get(&intent.object_id) else {
                return Ok(UploadResolution::RestartRequired);
            };
            if bytes.len() as u64 != intent.byte_length
                || risunest_sync_wire::hash(bytes) != intent.sha256
                || state.roles.get(&intent.object_id) != Some(&intent.role)
            {
                return Ok(UploadResolution::Conflict);
            }
            Ok(UploadResolution::Complete(ObjectReceipt {
                locator: RemoteLocator {
                    connection_identity: repository.connection_identity.clone(),
                    collection: None,
                    object: intent.object_id.clone(),
                },
                byte_length: intent.byte_length,
                version: Some(VersionToken(version.to_string())),
                checksum: None,
                complete: true,
            }))
        })
    }
    fn request_cost(&self, _: ProviderOperation) -> Vec<RequestCost> {
        Vec::new()
    }
}
pub(crate) fn repository() -> RepositoryHandle {
    RepositoryHandle {
        repository_id: "synthetic-repository".into(),
        connection_identity: "synthetic-account/root".into(),
        context: Box::new(()),
    }
}
pub(crate) fn locator() -> RemoteLocator {
    RemoteLocator {
        connection_identity: repository().connection_identity,
        collection: None,
        object: "head".into(),
    }
}

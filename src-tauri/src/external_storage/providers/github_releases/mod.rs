//! GitHub Releases backup adapter. Append only: it creates unique release
//! assets and reads them back, and offers no head publication at all, so
//! `compare_exchange_head` and `replace_head` answer `Unsupported` and the head
//! capabilities stay unverified. Asset metadata updates rename an asset rather
//! than replacing its bytes, and deleting and recreating an asset changes its
//! id, so neither is presented as a stable head.
//!
//! Connection contract:
//!
//! - `provider` is `github_releases` and `oauth_profile` must be absent.
//! - `endpoint` is the REST host, normally `https://api.github.com`.
//! - `location["uploadEndpoint"]` is the asset upload host, `https://uploads.github.com`
//!   by default. Both accept `http` only for `127.0.0.1`, which is the loopback
//!   test fixture; the product transport refuses any other plain HTTP.
//! - `location["owner"]`, `location["repo"]` select the repository, which must
//!   be private. A public repository is refused with `Unsupported` because the
//!   adapter would publish backup objects at world readable URLs.
//! - `location["tagPrefix"]` is the root inside that repository. Releases are
//!   tagged `<tagPrefix>-<batch>-<seq>`, where `<batch>` is `d` for descriptors
//!   and `j<16 hex>` derived from the job id for every other role, and `<seq>`
//!   rolls over when a release reaches the adapter's asset bound.
//! - `account_id` is the GitHub login whose primary REST allowance the
//!   connection consumes; it is the quota sharing key of every request.
//! - `profile` is unused and any value is accepted.
//! - The secret is UTF-8 JSON `{"token":"<pat>"}` with no other field. A fine
//!   grained personal access token needs `Contents: read and write` on that one
//!   repository, plus the mandatory `Metadata: read`.
//!
//! Releases are created as drafts. A draft is visible only to accounts with
//! push access and is never published, which keeps assets appendable. Creating
//! a release may materialise the Git tag it names, so the repository must be a
//! dedicated backup repository: the adapter only ever adds uniquely named tags
//! and never moves or removes an existing ref.
use super::{common, Dependencies};
use crate::external_storage::{
    capabilities::{Capabilities, Evidence},
    contract::*,
    http::{HttpRequest, HttpResponse},
};
use api::{AssetView, Batch, Context, ReleaseView, RepositoryView};
use reqwest::Method;
use std::{collections::BTreeMap, sync::Arc};
use zeroize::Zeroizing;

mod api;
#[cfg(test)]
mod tests;

/// A batch may open this many successive releases before the adapter stops.
const MAX_BATCH_ROLLOVERS: u32 = 4096;
/// Reconciliation walks batch releases from the first sequence upwards.
const RECONCILE_SEQ_SCAN: u32 = 8;

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(GithubReleases { dependencies }))
}

struct GithubReleases {
    dependencies: Dependencies,
}

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}

fn capabilities() -> Capabilities {
    Capabilities {
        immutable_create: Evidence::Synthetic,
        direct_complete_read: Evidence::Synthetic,
        // No conditional or plain head write exists on this service.
        atomic_create_head: Evidence::Unverified,
        conditional_head_update: Evidence::Unverified,
        stable_head_replace: Evidence::Unverified,
        head_read_after_write: Evidence::Unverified,
        head_retry_control: Evidence::Unverified,
        snapshot_discovery: Evidence::Synthetic,
        // One release listing request accompanies each page of assets read.
        discovery_extra_requests: 1,
        conditional_get: false,
        range: false,
        resumable_upload: false,
        max_stored_bytes: Some(api::MAX_ASSET_BYTES),
        sdk_overhead_bytes: 0,
        upload_alignment: 1,
        documented_at: Some("2026-09-14".into()),
        evidence_urls: vec![
            "https://docs.github.com/en/rest/releases/releases?apiVersion=2022-11-28".into(),
            "https://docs.github.com/en/rest/releases/assets?apiVersion=2022-11-28".into(),
            "https://docs.github.com/en/rest/repos/repos?apiVersion=2022-11-28".into(),
            "https://docs.github.com/en/rest/using-the-rest-api/rate-limits-for-the-rest-api?apiVersion=2022-11-28".into(),
            "https://docs.github.com/en/rest/authentication/permissions-required-for-fine-grained-personal-access-tokens?apiVersion=2022-11-28".into(),
            "https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases".into(),
            "https://github.blog/changelog/2025-06-03-releases-now-expose-digests-for-release-assets/".into(),
        ],
    }
}

impl GithubReleases {
    fn now(&self) -> u64 {
        self.dependencies.clock.now_ms()
    }

    async fn send(&self, request: HttpRequest, cancel: &Cancellation) -> Result<HttpResponse> {
        crate::external_storage::http::send(
            self.dependencies.http.as_ref(),
            self.dependencies.budget.as_ref(),
            self.dependencies.clock.as_ref(),
            request,
            cancel,
        )
        .await
    }

    fn context<'a>(&self, repository: &'a RepositoryHandle) -> Result<&'a Context> {
        repository
            .context
            .downcast_ref::<Context>()
            .filter(|context| context.identity == repository.connection_identity)
            .ok_or_else(corrupt)
    }

    fn request(
        &self,
        context: &Context,
        method: Method,
        url: url::Url,
        operation: ProviderOperation,
    ) -> HttpRequest {
        let mut headers = BTreeMap::new();
        headers.insert("accept".to_owned(), api::JSON_ACCEPT.to_owned());
        headers.insert(
            "x-github-api-version".to_owned(),
            api::API_VERSION.to_owned(),
        );
        headers.insert("user-agent".to_owned(), api::USER_AGENT.to_owned());
        headers.insert(
            "authorization".to_owned(),
            context.authorization().to_string(),
        );
        HttpRequest {
            method,
            url,
            headers,
            body: None,
            content_length: None,
            operation,
            costs: api::costs(operation, &context.account),
        }
    }

    async fn decode<T: serde::de::DeserializeOwned>(
        &self,
        mut response: HttpResponse,
        cancel: &Cancellation,
    ) -> Result<T> {
        common::require_status(&response, &[200, 201], self.now())?;
        let bytes =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        // A response body may quote a token or a signed URL; only the shape is read.
        serde_json::from_slice(&bytes).map_err(|_| corrupt())
    }

    async fn token(&self, secret: &SecretRef) -> Result<Zeroizing<String>> {
        let bytes = self.dependencies.vault.read(secret).await?;
        let payload: api::TokenPayload = serde_json::from_slice(bytes.0.as_slice())
            .map_err(|_| ProviderError::new(ErrorKind::ReauthRequired))?;
        let token = Zeroizing::new(payload.token);
        if token.is_empty() || token.len() > 512 || !token.chars().all(|c| c.is_ascii_graphic()) {
            return Err(ProviderError::new(ErrorKind::ReauthRequired));
        }
        Ok(token)
    }

    async fn releases_page(
        &self,
        context: &Context,
        page: u32,
        cancel: &Cancellation,
    ) -> Result<Vec<ReleaseView>> {
        let url = context.release_page_url(page)?;
        let request = self.request(context, Method::GET, url, ProviderOperation::List);
        let response = self.send(request, cancel).await?;
        self.decode(response, cancel).await
    }

    async fn assets_page(
        &self,
        context: &Context,
        release: u64,
        page: u32,
        cancel: &Cancellation,
    ) -> Result<Vec<AssetView>> {
        let url = context.asset_page_url(release, page)?;
        let request = self.request(context, Method::GET, url, ProviderOperation::List);
        let response = self.send(request, cancel).await?;
        self.decode(response, cancel).await
    }

    /// Release listing is page numbered, so one tag costs a bounded scan. A scan
    /// that reaches the bound reports absence, which keeps every caller on the
    /// side that never reuses or overwrites an unseen release.
    async fn find_release(
        &self,
        context: &Context,
        tag: &str,
        cancel: &Cancellation,
    ) -> Result<Option<u64>> {
        for page in 1..=api::MAX_RELEASE_SCAN_PAGES {
            let releases = self.releases_page(context, page, cancel).await?;
            if let Some(found) = releases.iter().find(|release| release.tag_name == tag) {
                return Ok(Some(found.id));
            }
            if releases.len() < api::RELEASE_PAGE_SIZE {
                break;
            }
        }
        Ok(None)
    }

    async fn find_asset(
        &self,
        context: &Context,
        release: u64,
        name: &str,
        cancel: &Cancellation,
    ) -> Result<Option<AssetView>> {
        for page in 1..=api::MAX_ASSET_SCAN_PAGES {
            let assets = self.assets_page(context, release, page, cancel).await?;
            let exhausted = assets.len() < api::ASSET_PAGE_SIZE;
            if let Some(found) = assets.into_iter().find(|asset| asset.name == name) {
                return Ok(Some(found));
            }
            if exhausted {
                break;
            }
        }
        Ok(None)
    }

    /// True when the root already holds releases of this adapter. A scan that
    /// reaches its bound counts as occupied: a crowded location is refused for
    /// `Create` instead of being silently adopted.
    async fn root_occupied(&self, context: &Context, cancel: &Cancellation) -> Result<bool> {
        for page in 1..=api::MAX_RELEASE_SCAN_PAGES {
            let releases = self.releases_page(context, page, cancel).await?;
            if releases
                .iter()
                .any(|release| context.owns_tag(&release.tag_name))
            {
                return Ok(true);
            }
            if releases.len() < api::RELEASE_PAGE_SIZE {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Creates the release of one batch sequence, or adopts the existing one
    /// when the tag is already taken. Existing assets are counted, never removed.
    async fn open_batch(
        &self,
        context: &Context,
        batch: &str,
        seq: u32,
        cancel: &Cancellation,
    ) -> Result<Batch> {
        let tag = context.tag(batch, seq);
        let body = serde_json::to_vec(&serde_json::json!({
            "tag_name": tag,
            "name": tag,
            "draft": true,
            "prerelease": false,
        }))
        .map_err(|_| corrupt())?;
        let url = context.repository_url(&["releases"])?;
        let mut request = self.request(context, Method::POST, url, ProviderOperation::Create);
        request
            .headers
            .insert("content-type".to_owned(), "application/json".to_owned());
        request.content_length = Some(body.len() as u64);
        request.body = Some(Box::pin(std::io::Cursor::new(body)) as _);
        let response = self.send(request, cancel).await?;
        match response.status {
            201 => {
                let release: ReleaseView = self.decode(response, cancel).await?;
                Ok(Batch {
                    seq,
                    release: release.id,
                    assets: 0,
                })
            }
            422 => {
                let conflict = api::classify(422, &response.headers, self.now());
                let Some(release) = self.find_release(context, &tag, cancel).await? else {
                    return Err(conflict);
                };
                let assets = self.assets_page(context, release, 1, cancel).await?;
                Ok(Batch {
                    seq,
                    release,
                    assets: assets.len(),
                })
            }
            status => Err(api::classify(status, &response.headers, self.now())),
        }
    }

    /// Resolves the release that should receive the next asset of a batch,
    /// rolling to the following sequence once the current release is full.
    async fn batch_release(
        &self,
        context: &Context,
        batch: &str,
        cancel: &Cancellation,
    ) -> Result<(u64, String)> {
        let mut state = context.batches().get(batch).copied();
        for _ in 0..MAX_BATCH_ROLLOVERS {
            let resolved = match state {
                Some(current) if current.assets < api::MAX_ASSETS_PER_RELEASE => {
                    return Ok((current.release, context.tag(batch, current.seq)))
                }
                Some(current) => {
                    if batch == api::DESCRIPTOR_BATCH {
                        // Descriptors stay in one deterministic release so an
                        // existing root is provable without a scan.
                        return Err(ProviderError::new(ErrorKind::StorageFull));
                    }
                    self.open_batch(context, batch, current.seq + 1, cancel)
                        .await?
                }
                None => self.open_batch(context, batch, 0, cancel).await?,
            };
            context.batches().insert(batch.to_owned(), resolved);
            state = Some(resolved);
        }
        Err(ProviderError::new(ErrorKind::StorageFull))
    }

    /// Definitive evidence that the stored asset is this object: the service
    /// reports the asset finished, its length matches and, when it exposes a
    /// digest, that digest is the expected SHA-256 of the bytes.
    fn verify(asset: &AssetView, intent: &ObjectIntent) -> Result<()> {
        if asset.name != api::asset_name(intent.role, &intent.object_id) {
            return Err(corrupt());
        }
        if !asset.matches(intent) {
            return Err(common::error(ErrorKind::PreconditionFailed, 422));
        }
        if !asset.uploaded() {
            // An unfinished asset holds the name; it is never deleted or replaced.
            return Err(common::error(ErrorKind::Transient, 422));
        }
        Ok(())
    }

    fn receipt(
        context: &Context,
        tag: &str,
        release: u64,
        asset: &AssetView,
        intent: &ObjectIntent,
    ) -> ObjectReceipt {
        ObjectReceipt {
            locator: context.locator(tag, release, asset.id),
            byte_length: intent.byte_length,
            version: None,
            checksum: asset.checksum(),
            complete: true,
        }
    }
}

impl Provider for GithubReleases {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let context = Context::new(config, self.token(secret).await?)?;
            let url = context.repository_url(&[])?;
            let request = self.request(&context, Method::GET, url, ProviderOperation::Metadata);
            let response = self.send(request, cancel).await?;
            if response.status != 200 {
                return Err(api::classify(
                    response.status,
                    &response.headers,
                    self.now(),
                ));
            }
            let repository: RepositoryView = self.decode(response, cancel).await?;
            if !repository.private {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            if repository
                .permissions
                .is_some_and(|permissions| !permissions.push)
            {
                return Err(ProviderError::new(ErrorKind::Unauthorized));
            }
            match mode {
                OpenMode::Create => {
                    if self.root_occupied(&context, cancel).await? {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                }
                OpenMode::Existing => {
                    let tag = context.descriptor_tag();
                    let release = self
                        .find_release(&context, &tag, cancel)
                        .await?
                        .ok_or_else(|| ProviderError::new(ErrorKind::NotFound))?;
                    let assets = self.assets_page(&context, release, 1, cancel).await?;
                    let prefix = format!("{}-", api::role_prefix(ObjectRole::Descriptor));
                    if !assets.iter().any(|asset| asset.name.starts_with(&prefix)) {
                        return Err(ProviderError::new(ErrorKind::NotFound));
                    }
                    context.batches().insert(
                        api::DESCRIPTOR_BATCH.to_owned(),
                        Batch {
                            seq: 0,
                            release,
                            assets: assets.len(),
                        },
                    );
                }
            }
            let identity = context.identity.clone();
            Ok((
                RepositoryHandle {
                    repository_id: identity.clone(),
                    connection_identity: identity,
                    context: Box::new(context),
                },
                capabilities(),
            ))
        })
    }

    fn read_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        _unchanged: Option<&'a VersionToken>,
        sink: &'a mut dyn TransferSink,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ReadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            // Assets are immutable under their name, so the service exposes no
            // version token here and `unchanged` cannot be answered with 304.
            let (_release, asset) = api::parse_locator(&locator.object)?;
            let url = context.asset_url(asset)?;
            let mut request = self.request(
                context,
                Method::GET,
                url.clone(),
                ProviderOperation::DownloadUrl,
            );
            request
                .headers
                .insert("accept".to_owned(), api::BINARY_ACCEPT.to_owned());
            let mut response = self.send(request, cancel).await?;
            if response.status != 200 {
                let location = match response.status {
                    301 | 302 | 303 | 307 | 308 => response.headers.get("location").cloned(),
                    _ => None,
                };
                let Some(location) = location else {
                    return Err(api::classify(
                        response.status,
                        &response.headers,
                        self.now(),
                    ));
                };
                let target = url.join(&location).map_err(|_| corrupt())?;
                if !target.username().is_empty()
                    || target.password().is_some()
                    || target.fragment().is_some()
                {
                    return Err(corrupt());
                }
                // One documented hop to the asset host, without the token.
                let hop = HttpRequest {
                    method: Method::GET,
                    url: target,
                    headers: BTreeMap::from([
                        ("accept".to_owned(), api::BINARY_ACCEPT.to_owned()),
                        ("user-agent".to_owned(), api::USER_AGENT.to_owned()),
                    ]),
                    body: None,
                    content_length: None,
                    operation: ProviderOperation::Get,
                    costs: api::costs(ProviderOperation::Get, &context.account),
                };
                response = self.send(hop, cancel).await?;
                if response.status != 200 {
                    return Err(api::classify(
                        response.status,
                        &response.headers,
                        self.now(),
                    ));
                }
            }
            let length = common::content_length(&response.headers)?;
            let (byte_length, sha256) = common::stream_to_sink(
                &mut response.body,
                sink,
                length,
                length.unwrap_or(api::MAX_ASSET_BYTES),
                cancel,
            )
            .await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length,
                version: None,
                checksum: Some(Checksum {
                    algorithm: "sha256".into(),
                    value: sha256,
                    provider_verified: false,
                }),
                complete: true,
            }))
        })
    }

    /// The service has no resumable asset upload, so an interrupted object is
    /// retransmitted whole and no session state is ever journalled.
    fn begin_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, Option<ResumeState>> {
        Box::pin(async move {
            cancel.check()?;
            self.context(repository)?;
            intent.validate(repository)?;
            Ok(None)
        })
    }

    fn create_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        source: &'a dyn TransferSource,
        resume: Option<&'a ResumeState>,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ObjectReceipt> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            intent.validate(repository)?;
            if resume.is_some() {
                // This adapter issues no session, so no state can belong to it.
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            if !api::is_safe_name(&intent.object_id, 180) {
                return Err(corrupt());
            }
            if source.byte_length() != intent.byte_length {
                return Err(corrupt());
            }
            if intent.byte_length > api::MAX_ASSET_BYTES {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let batch = api::batch_key(intent.role, &intent.job_id);
            let name = api::asset_name(intent.role, &intent.object_id);
            let (release, tag) = self.batch_release(context, &batch, cancel).await?;
            let url = context.upload_url(release, &name)?;
            let mut request = self.request(context, Method::POST, url, ProviderOperation::Create);
            request
                .headers
                .insert("content-type".to_owned(), api::BINARY_ACCEPT.to_owned());
            request.content_length = Some(intent.byte_length);
            request.body = Some(source.open(0, intent.byte_length, cancel).await?);
            let response = self.send(request, cancel).await?;
            match response.status {
                201 => {
                    let asset: AssetView = self.decode(response, cancel).await?;
                    Self::verify(&asset, intent)?;
                    {
                        let mut batches = context.batches();
                        if let Some(state) = batches.get_mut(&batch) {
                            if state.release == release {
                                state.assets += 1;
                            }
                        }
                    }
                    Ok(Self::receipt(context, &tag, release, &asset, intent))
                }
                422 => {
                    // The name is taken. A retry of the same object converges on
                    // the stored asset; different bytes are refused, never replaced.
                    let conflict = api::classify(422, &response.headers, self.now());
                    let Some(asset) = self.find_asset(context, release, &name, cancel).await?
                    else {
                        return Err(conflict);
                    };
                    Self::verify(&asset, intent)?;
                    Ok(Self::receipt(context, &tag, release, &asset, intent))
                }
                status => Err(api::classify(status, &response.headers, self.now())),
            }
        })
    }

    /// No conditional write primitive exists for a release asset.
    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        _locator: &'a RemoteLocator,
        _expected: &'a ExpectedHead,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.context(repository)?;
            Err(ProviderError::new(ErrorKind::Unsupported))
        })
    }

    /// Replacing an asset's bytes is not possible: metadata updates rename it
    /// and delete then recreate yields a new id, so no stable head is offered.
    fn replace_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        _locator: &'a RemoteLocator,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.context(repository)?;
            Err(ProviderError::new(ErrorKind::Unsupported))
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
            let context = self.context(repository)?;
            if limit == 0 || limit > 1000 {
                return Err(ProviderError::new(ErrorKind::Unsupported));
            }
            let (mut release_page, mut release_index, mut asset_page, mut asset_index) =
                parse_cursor(cursor)?;
            let prefix = format!("{}-", api::collection_prefix(collection));
            let mut objects = Vec::new();
            let mut next_cursor = None;
            let mut releases: Option<Vec<ReleaseView>> = None;
            for step in 0..api::MAX_LIST_STEPS {
                if releases.is_none() {
                    releases = Some(self.releases_page(context, release_page, cancel).await?);
                }
                let page_length = releases.as_ref().map_or(0, Vec::len);
                if release_index >= page_length {
                    if page_length < api::RELEASE_PAGE_SIZE {
                        break;
                    }
                    release_page += 1;
                    release_index = 0;
                    asset_page = 1;
                    asset_index = 0;
                    releases = None;
                    continue;
                }
                let (id, tag) = releases
                    .as_ref()
                    .map(|page| (page[release_index].id, page[release_index].tag_name.clone()))
                    .ok_or_else(corrupt)?;
                if !context.holds_collection(&tag, collection) {
                    release_index += 1;
                    asset_page = 1;
                    asset_index = 0;
                    continue;
                }
                let assets = self.assets_page(context, id, asset_page, cancel).await?;
                let exhausted = assets.len() < api::ASSET_PAGE_SIZE;
                let matching: Vec<&AssetView> = assets
                    .iter()
                    .filter(|asset| asset.name.starts_with(&prefix))
                    .collect();
                let mut stopped = false;
                for (index, asset) in matching.iter().enumerate().skip(asset_index) {
                    if objects.len() >= limit as usize {
                        next_cursor = Some(format!(
                            "{release_page}:{release_index}:{asset_page}:{index}"
                        ));
                        stopped = true;
                        break;
                    }
                    objects.push(ObjectReceipt {
                        locator: context.locator(&tag, id, asset.id),
                        byte_length: asset.size,
                        version: None,
                        checksum: asset.checksum(),
                        complete: asset.uploaded(),
                    });
                }
                if stopped {
                    break;
                }
                asset_index = 0;
                if exhausted {
                    release_index += 1;
                    asset_page = 1;
                } else {
                    asset_page += 1;
                }
                if step + 1 == api::MAX_LIST_STEPS {
                    next_cursor = Some(format!("{release_page}:{release_index}:{asset_page}:0"));
                }
            }
            Ok(ObjectPage {
                objects,
                next_cursor,
            })
        })
    }

    /// The adapter issues no upload session, so `resume` carries no offset of
    /// its own. Reconciliation asks the service what it actually stored for
    /// this object and reports that, which is the recovery path after a lost
    /// upload response.
    fn reconcile_upload<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        intent: &'a ObjectIntent,
        _resume: &'a ResumeState,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, UploadResolution> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            intent.validate(repository)?;
            if !api::is_safe_name(&intent.object_id, 180) {
                return Err(corrupt());
            }
            let batch = api::batch_key(intent.role, &intent.job_id);
            let name = api::asset_name(intent.role, &intent.object_id);
            for seq in 0..RECONCILE_SEQ_SCAN {
                let tag = context.tag(&batch, seq);
                let Some(release) = self.find_release(context, &tag, cancel).await? else {
                    break;
                };
                let Some(asset) = self.find_asset(context, release, &name, cancel).await? else {
                    continue;
                };
                return Ok(if asset.uploaded() && asset.matches(intent) {
                    UploadResolution::Complete(Self::receipt(
                        context, &tag, release, &asset, intent,
                    ))
                } else {
                    UploadResolution::Conflict
                });
            }
            Ok(UploadResolution::RestartRequired)
        })
    }

    /// No head exists on this service, so no locator can name one.
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        self.context(repository)?;
        Err(ProviderError::new(ErrorKind::Unsupported))
    }

    fn request_cost(
        &self,
        repository: &RepositoryHandle,
        operation: ProviderOperation,
    ) -> Result<Vec<RequestCost>> {
        Ok(api::costs(operation, &self.context(repository)?.account))
    }
}

/// `<release page>:<release index>:<asset page>:<asset index>`, all positions
/// of the page numbered listings the service documents.
fn parse_cursor(cursor: Option<&str>) -> Result<(u32, usize, u32, usize)> {
    let Some(cursor) = cursor else {
        return Ok((1, 0, 1, 0));
    };
    let parts: Vec<&str> = cursor.split(':').collect();
    if parts.len() != 4 {
        return Err(corrupt());
    }
    let release_page: u32 = parts[0].parse().map_err(|_| corrupt())?;
    let release_index: usize = parts[1].parse().map_err(|_| corrupt())?;
    let asset_page: u32 = parts[2].parse().map_err(|_| corrupt())?;
    let asset_index: usize = parts[3].parse().map_err(|_| corrupt())?;
    if release_page == 0
        || asset_page == 0
        || release_index >= api::RELEASE_PAGE_SIZE
        || asset_index >= api::ASSET_PAGE_SIZE
    {
        return Err(corrupt());
    }
    Ok((release_page, release_index, asset_page, asset_index))
}

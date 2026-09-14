//! GitLab Generic Packages adapter for GitLab.com and self-managed instances.
//!
//! Append-only backup target. Generic packages have no conditional write and no
//! documented overwrite-and-read path for one file name, so both head methods
//! answer `Unsupported` and every head capability stays `Unverified`. No Git
//! protocol is used.
//!
//! `ConnectionConfig`:
//!
//! * `provider` — `gitlab_packages`.
//! * `endpoint` — instance base URL, for example `https://gitlab.com` or
//!   `https://gitlab.example.com/gitlab`. The API is addressed under `/api/v4`.
//! * `profile` — absent (inferred from the host), `gitlabCom` or `selfManaged`.
//!   The GitLab.com profile pins the documented 5 GB generic file limit.
//! * `account_id` — the token principal the instance rate limit is shared by.
//! * `location["projectId"]` — numeric project id or the plain namespace path
//!   (`group/subgroup/project`); it is percent-encoded per request, so a value
//!   that is already URL-encoded is rejected.
//! * `location["packageName"]` — dedicated package base name for this
//!   repository, so a package cleanup policy can be scoped to it.
//! * `location["maxFileBytes"]` — optional decimal per-file limit of a
//!   self-managed instance. Combined with the profile limit by taking the lower.
//! * `oauth_profile` — must be absent; this service authenticates with a token.
//!
//! Secret payload (`vault.read`): UTF-8 JSON
//! `{"token":"<token>","kind":"deployToken|personalAccessToken|projectAccessToken"}`.
//! A deploy token is sent as `DEPLOY-TOKEN`, the other kinds as `PRIVATE-TOKEN`.
//!
//! Remote layout under the project, one package name per role and one package
//! version per object:
//!
//! ```text
//! <packageName>.repository / v0        / repository            (root marker)
//! <packageName>.descriptor / v0-<id>   / descriptor-<id>
//! <packageName>.pack       / v0-<id>   / pack-<id>
//! <packageName>.catalog    / v0-<id>   / catalog-<id>
//! <packageName>.snapshot   / v0-<id>   / snapshot-<id>
//! <packageName>.point      / v0-<id>   / point-<id>
//! ```
//!
//! `<id>` is the object identifier as base64url without padding, which keeps
//! both the version and the file name inside the charsets GitLab documents.
//! `RemoteLocator.object` is `<package>/<version>/<file>`, a permanent path
//! another device resolves directly against the same project.
//!
//! Creating an object always asks the remote state first, because an instance
//! that allows duplicate generic packages answers a repeated upload by adding a
//! second file under the same name instead of rejecting it. A token that can
//! list resolves that from package metadata, otherwise the stored bytes are
//! read back; an absent object costs one `404` before the upload.
mod api;
mod config;
#[cfg(test)]
mod tests;

use super::{common, Dependencies};
use crate::external_storage::{
    capabilities::{Capabilities, Evidence},
    contract::*,
    http::{self, HttpResponse},
};
use api::{Outgoing, PackageFileJson, PackageJson};
use config::{Credential, Placement, Settings};
use sha2::Digest;
use std::{pin::Pin, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt};

const DOCUMENTED_AT: &str = "2026-09-14";
/// Used only when a download answers without a Content-Length.
const READ_CEILING: u64 = 5 * 1024 * 1024 * 1024;
/// Object storage backed instances answer a download with one signed redirect.
const MAX_REDIRECTS: u8 = 2;

fn corrupt() -> ProviderError {
    ProviderError::new(ErrorKind::Corrupt)
}
fn unsupported() -> ProviderError {
    ProviderError::new(ErrorKind::Unsupported)
}
fn io_error(cancel: &Cancellation) -> ProviderError {
    cancel
        .check()
        .err()
        .unwrap_or_else(|| ProviderError::new(ErrorKind::Transient))
}

fn evidence_urls() -> Vec<String> {
    [
        "https://docs.gitlab.com/user/packages/generic_packages/",
        "https://docs.gitlab.com/api/packages/",
        "https://docs.gitlab.com/api/rest/",
        "https://docs.gitlab.com/user/gitlab_com/",
        "https://docs.gitlab.com/user/project/deploy_tokens/",
        "https://docs.gitlab.com/user/storage_usage_quotas/",
        "https://docs.gitlab.com/administration/object_storage/",
        "https://docs.gitlab.com/administration/settings/user_and_ip_rate_limits/",
    ]
    .iter()
    .map(|url| (*url).to_owned())
    .collect()
}

struct Repository {
    settings: Settings,
    secret: SecretRef,
    /// Result of the listing probe performed while opening the repository.
    can_list: bool,
}

enum Verification {
    Absent,
    Present { provider_verified: bool },
    Different,
}

pub(crate) struct GitlabPackages {
    deps: Dependencies,
}

pub(crate) fn create(dependencies: Dependencies) -> Result<Arc<dyn Provider>> {
    Ok(Arc::new(GitlabPackages { deps: dependencies }))
}

async fn digest_body(
    body: &mut Pin<Box<dyn AsyncRead + Send>>,
    max: u64,
    cancel: &Cancellation,
) -> Result<(u64, String)> {
    let mut digest = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut received = 0u64;
    loop {
        let read = body.read(&mut buffer).await.map_err(|_| io_error(cancel))?;
        if read == 0 {
            break;
        }
        received += read as u64;
        if received > max {
            return Err(corrupt());
        }
        digest.update(&buffer[..read]);
    }
    Ok((received, hex::encode(digest.finalize())))
}

impl GitlabPackages {
    fn now(&self) -> u64 {
        self.deps.clock.now_ms()
    }
    async fn send(
        &self,
        settings: &Settings,
        outgoing: Outgoing<'_>,
        cancel: &Cancellation,
    ) -> Result<HttpResponse> {
        http::send(
            self.deps.http.as_ref(),
            self.deps.budget.as_ref(),
            self.deps.clock.as_ref(),
            api::request(settings, outgoing),
            cancel,
        )
        .await
    }
    fn context<'a>(&self, repository: &'a RepositoryHandle) -> Result<&'a Repository> {
        let context = repository
            .context
            .downcast_ref::<Repository>()
            .ok_or_else(corrupt)?;
        if context.settings.connection_identity != repository.connection_identity
            || context.settings.connection_identity != repository.repository_id
        {
            return Err(corrupt());
        }
        Ok(context)
    }
    async fn credential(&self, secret: &SecretRef) -> Result<Credential> {
        let bytes = self.deps.vault.read(secret).await?;
        config::credential(&bytes.0)
    }
    async fn json<T: serde::de::DeserializeOwned>(
        &self,
        response: &mut HttpResponse,
        cancel: &Cancellation,
    ) -> Result<T> {
        let bytes =
            common::read_bounded(&mut response.body, common::MAX_CONTROL_BODY, cancel).await?;
        // The response text is never turned into an error message.
        serde_json::from_slice(&bytes).map_err(|_| corrupt())
    }

    /// Downloads an object, following the documented object storage redirect
    /// without carrying the instance credential to another origin. Each hop
    /// reserves its own budget. `None` means the file does not exist.
    async fn fetch(
        &self,
        settings: &Settings,
        credential: &Credential,
        placement: &Placement,
        cancel: &Cancellation,
    ) -> Result<Option<HttpResponse>> {
        let mut url = api::object_url(settings, placement, &[])?;
        let mut hops = 0u8;
        loop {
            let same_origin = url.origin() == settings.endpoint.origin();
            let response = self
                .send(
                    settings,
                    Outgoing {
                        method: reqwest::Method::GET,
                        url: url.clone(),
                        operation: ProviderOperation::Get,
                        credential: same_origin.then_some(credential),
                        body: None,
                        content_length: None,
                    },
                    cancel,
                )
                .await?;
            match response.status {
                200 => return Ok(Some(response)),
                404 => return Ok(None),
                301 | 302 | 303 | 307 | 308 if hops < MAX_REDIRECTS => {
                    let location = response
                        .headers
                        .get("location")
                        .cloned()
                        .ok_or_else(corrupt)?;
                    url = url.join(&location).map_err(|_| corrupt())?;
                    hops += 1;
                }
                status => return Err(api::classify(status, &response.headers, self.now())),
            }
        }
    }

    async fn locate_package(
        &self,
        settings: &Settings,
        credential: &Credential,
        package: &str,
        version: &str,
        cancel: &Cancellation,
    ) -> Result<Option<u64>> {
        let url = api::packages_url(
            settings,
            &[
                ("package_type", "generic"),
                ("package_name", package),
                ("package_version", version),
                ("per_page", "20"),
            ],
        )?;
        let mut response = self
            .send(
                settings,
                Outgoing {
                    method: reqwest::Method::GET,
                    url,
                    operation: ProviderOperation::List,
                    credential: Some(credential),
                    body: None,
                    content_length: None,
                },
                cancel,
            )
            .await?;
        match response.status {
            200 => {}
            404 => return Ok(None),
            status => return Err(api::classify(status, &response.headers, self.now())),
        }
        let packages: Vec<PackageJson> = self.json(&mut response, cancel).await?;
        Ok(packages
            .into_iter()
            .find(|entry| entry.name == package && entry.version == version)
            .map(|entry| entry.id))
    }

    async fn package_files(
        &self,
        settings: &Settings,
        credential: &Credential,
        package_id: u64,
        cancel: &Cancellation,
    ) -> Result<Vec<PackageFileJson>> {
        let url = api::package_files_url(settings, package_id, &[("per_page", "100")])?;
        let mut response = self
            .send(
                settings,
                Outgoing {
                    method: reqwest::Method::GET,
                    url,
                    operation: ProviderOperation::List,
                    credential: Some(credential),
                    body: None,
                    content_length: None,
                },
                cancel,
            )
            .await?;
        match response.status {
            200 => {}
            404 => return Ok(Vec::new()),
            status => return Err(api::classify(status, &response.headers, self.now())),
        }
        self.json(&mut response, cancel).await
    }

    /// Remote truth for one intended object. Two files under one name mean a
    /// duplicate accumulated under an instance that allows duplicates, which is
    /// conflict evidence rather than a converged upload.
    async fn verify(
        &self,
        context: &Repository,
        credential: &Credential,
        intent: &ObjectIntent,
        placement: &Placement,
        cancel: &Cancellation,
    ) -> Result<Verification> {
        let settings = &context.settings;
        if context.can_list {
            let Some(package_id) = self
                .locate_package(
                    settings,
                    credential,
                    &placement.package,
                    &placement.version,
                    cancel,
                )
                .await?
            else {
                return Ok(Verification::Absent);
            };
            let files = self
                .package_files(settings, credential, package_id, cancel)
                .await?;
            let mut matching = files.iter().filter(|file| file.file_name == placement.file);
            let Some(file) = matching.next() else {
                return Ok(Verification::Absent);
            };
            if matching.next().is_some() {
                return Ok(Verification::Different);
            }
            if file.size != intent.byte_length {
                return Ok(Verification::Different);
            }
            match file.file_sha256.as_deref() {
                Some(value) if value.eq_ignore_ascii_case(&intent.sha256) => {
                    return Ok(Verification::Present {
                        provider_verified: true,
                    })
                }
                Some(_) => return Ok(Verification::Different),
                // An instance that stores no digest still has to prove the bytes.
                None => {}
            }
        }
        let Some(mut response) = self.fetch(settings, credential, placement, cancel).await? else {
            return Ok(Verification::Absent);
        };
        if common::content_length(&response.headers)?
            .is_some_and(|length| length != intent.byte_length)
        {
            return Ok(Verification::Different);
        }
        let (received, hash) = digest_body(
            &mut response.body,
            intent.byte_length.saturating_add(1),
            cancel,
        )
        .await?;
        if received == intent.byte_length && hash.eq_ignore_ascii_case(&intent.sha256) {
            Ok(Verification::Present {
                provider_verified: false,
            })
        } else {
            Ok(Verification::Different)
        }
    }

    async fn write_marker(
        &self,
        settings: &Settings,
        credential: &Credential,
        cancel: &Cancellation,
    ) -> Result<()> {
        let marker = settings.marker();
        let body = settings.marker_body();
        let length = body.len() as u64;
        let expected = hex::encode(sha2::Sha256::digest(&body));
        let url = api::object_url(settings, &marker, &[("select", "package_file")])?;
        let mut response = self
            .send(
                settings,
                Outgoing {
                    method: reqwest::Method::PUT,
                    url,
                    operation: ProviderOperation::Create,
                    credential: Some(credential),
                    body: Some(Box::pin(std::io::Cursor::new(body))),
                    content_length: Some(length),
                },
                cancel,
            )
            .await?;
        match response.status {
            200 => {
                let file: PackageFileJson = self.json(&mut response, cancel).await?;
                let agrees = file.file_name == marker.file
                    && file.size == length
                    && file
                        .file_sha256
                        .as_deref()
                        .is_none_or(|value| value.eq_ignore_ascii_case(&expected));
                if agrees {
                    Ok(())
                } else {
                    Err(corrupt())
                }
            }
            201 => Ok(()),
            // The instance forbids duplicates and the marker already exists.
            400 => Err(ProviderError {
                kind: ErrorKind::PreconditionFailed,
                http_status: Some(400),
                retry_at_ms: None,
            }),
            status => Err(api::classify(status, &response.headers, self.now())),
        }
    }

    async fn probe_listing(
        &self,
        settings: &Settings,
        credential: &Credential,
        cancel: &Cancellation,
    ) -> Result<bool> {
        let url = api::packages_url(
            settings,
            &[
                ("package_type", "generic"),
                ("package_name", &settings.package_base),
                ("per_page", "1"),
            ],
        )?;
        let response = self
            .send(
                settings,
                Outgoing {
                    method: reqwest::Method::GET,
                    url,
                    operation: ProviderOperation::List,
                    credential: Some(credential),
                    body: None,
                    content_length: None,
                },
                cancel,
            )
            .await?;
        match response.status {
            200 => Ok(true),
            401 | 403 => Ok(false),
            status => Err(api::classify(status, &response.headers, self.now())),
        }
    }
}

fn receipt(
    settings: &Settings,
    intent: &ObjectIntent,
    placement: &Placement,
    provider_verified: bool,
) -> ObjectReceipt {
    ObjectReceipt {
        locator: settings.locator(placement),
        byte_length: intent.byte_length,
        version: None,
        checksum: Some(Checksum {
            algorithm: "sha256".into(),
            value: intent.sha256.clone(),
            provider_verified,
        }),
        complete: true,
    }
}

fn capabilities(settings: &Settings, can_list: bool) -> Capabilities {
    Capabilities {
        immutable_create: Evidence::Synthetic,
        direct_complete_read: Evidence::Synthetic,
        atomic_create_head: Evidence::Unverified,
        conditional_head_update: Evidence::Unverified,
        stable_head_replace: Evidence::Unverified,
        head_read_after_write: Evidence::Unverified,
        head_retry_control: Evidence::Unverified,
        snapshot_discovery: if can_list {
            Evidence::Synthetic
        } else {
            Evidence::Unverified
        },
        discovery_extra_requests: u32::from(can_list),
        conditional_get: false,
        range: false,
        resumable_upload: false,
        max_stored_bytes: settings.max_stored_bytes,
        sdk_overhead_bytes: 0,
        upload_alignment: 1,
        documented_at: Some(DOCUMENTED_AT.into()),
        evidence_urls: evidence_urls(),
    }
}

impl Provider for GitlabPackages {
    fn open_repository<'a>(
        &'a self,
        config: &'a ConnectionConfig,
        secret: &'a SecretRef,
        mode: OpenMode,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, (RepositoryHandle, Capabilities)> {
        Box::pin(async move {
            cancel.check()?;
            let settings = config::settings(config)?;
            let credential = self.credential(secret).await?;
            let marker = settings.marker();
            let url = api::object_url(&settings, &marker, &[])?;
            let mut response = self
                .send(
                    &settings,
                    Outgoing {
                        method: reqwest::Method::GET,
                        url,
                        operation: ProviderOperation::Metadata,
                        credential: Some(&credential),
                        body: None,
                        content_length: None,
                    },
                    cancel,
                )
                .await?;
            match (response.status, mode) {
                (200, OpenMode::Create) => {
                    return Err(ProviderError {
                        kind: ErrorKind::PreconditionFailed,
                        http_status: Some(200),
                        retry_at_ms: None,
                    })
                }
                (200, OpenMode::Existing) => {
                    let body = common::read_bounded(&mut response.body, 64 * 1024, cancel).await?;
                    if body != settings.marker_body() {
                        return Err(corrupt());
                    }
                }
                (404, OpenMode::Existing) => {
                    return Err(ProviderError {
                        kind: ErrorKind::NotFound,
                        http_status: Some(404),
                        retry_at_ms: None,
                    })
                }
                (404, OpenMode::Create) => {
                    self.write_marker(&settings, &credential, cancel).await?;
                }
                (status, _) => {
                    return Err(api::classify(status, &response.headers, self.now()));
                }
            }
            let can_list = self.probe_listing(&settings, &credential, cancel).await?;
            let capabilities = capabilities(&settings, can_list);
            let handle = RepositoryHandle {
                repository_id: settings.connection_identity.clone(),
                connection_identity: settings.connection_identity.clone(),
                context: Box::new(Repository {
                    settings,
                    secret: secret.clone(),
                    can_list,
                }),
            };
            Ok((handle, capabilities))
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
            let (_, placement) = context.settings.parse_object(&locator.object)?;
            let credential = self.credential(&context.secret).await?;
            let Some(mut response) = self
                .fetch(&context.settings, &credential, &placement, cancel)
                .await?
            else {
                return Err(ProviderError {
                    kind: ErrorKind::NotFound,
                    http_status: Some(404),
                    retry_at_ms: None,
                });
            };
            let declared = common::content_length(&response.headers)?;
            let ceiling = declared
                .or(context.settings.max_stored_bytes)
                .unwrap_or(READ_CEILING);
            let (received, hash) =
                common::stream_to_sink(&mut response.body, sink, declared, ceiling, cancel).await?;
            Ok(ReadReceipt::Body(ObjectReceipt {
                locator: locator.clone(),
                byte_length: received,
                version: None,
                checksum: Some(Checksum {
                    algorithm: "sha256".into(),
                    value: hash,
                    provider_verified: false,
                }),
                complete: true,
            }))
        })
    }

    /// Generic packages document no resumable session, so an interrupted object
    /// is sent again in full.
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
                return Err(unsupported());
            }
            if source.byte_length() != intent.byte_length {
                return Err(corrupt());
            }
            if context
                .settings
                .max_stored_bytes
                .is_some_and(|max| intent.byte_length > max)
            {
                return Err(ProviderError::new(ErrorKind::FileTooLarge));
            }
            let placement = context.settings.place(intent.role, &intent.object_id)?;
            let credential = self.credential(&context.secret).await?;
            // An instance that allows duplicates would append a second file
            // under the same name, so the remote state decides before any byte
            // is sent again.
            match self
                .verify(context, &credential, intent, &placement, cancel)
                .await?
            {
                Verification::Present { provider_verified } => {
                    return Ok(receipt(
                        &context.settings,
                        intent,
                        &placement,
                        provider_verified,
                    ))
                }
                Verification::Different => {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed))
                }
                Verification::Absent => {}
            }
            let url =
                api::object_url(&context.settings, &placement, &[("select", "package_file")])?;
            let body = source.open(0, intent.byte_length, cancel).await?;
            let mut response = self
                .send(
                    &context.settings,
                    Outgoing {
                        method: reqwest::Method::PUT,
                        url,
                        operation: ProviderOperation::Create,
                        credential: Some(&credential),
                        body: Some(body),
                        content_length: Some(intent.byte_length),
                    },
                    cancel,
                )
                .await?;
            match response.status {
                200 => {
                    let file: PackageFileJson = self.json(&mut response, cancel).await?;
                    if file.file_name != placement.file || file.size != intent.byte_length {
                        return Err(corrupt());
                    }
                    let provider_verified = match file.file_sha256.as_deref() {
                        Some(value) if value.eq_ignore_ascii_case(&intent.sha256) => true,
                        Some(_) => return Err(corrupt()),
                        None => false,
                    };
                    Ok(receipt(
                        &context.settings,
                        intent,
                        &placement,
                        provider_verified,
                    ))
                }
                201 => Ok(receipt(&context.settings, intent, &placement, false)),
                // Duplicates are forbidden, the package name is taken, or the
                // request was rejected. Only the remote object decides which.
                400 => match self
                    .verify(context, &credential, intent, &placement, cancel)
                    .await?
                {
                    Verification::Present { provider_verified } => Ok(receipt(
                        &context.settings,
                        intent,
                        &placement,
                        provider_verified,
                    )),
                    Verification::Different => Err(ProviderError {
                        kind: ErrorKind::PreconditionFailed,
                        http_status: Some(400),
                        retry_at_ms: None,
                    }),
                    Verification::Absent => Err(api::classify(400, &response.headers, self.now())),
                },
                status => Err(api::classify(status, &response.headers, self.now())),
            }
        })
    }

    fn compare_exchange_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        _expected: &'a ExpectedHead,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.context(repository)?;
            locator.validate_for(repository)?;
            Err(unsupported())
        })
    }

    /// Publishing the same file name again either fails or appends a second
    /// file, and no documentation states which one a later read returns.
    fn replace_head<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        _head: &'a HeadBytes,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, HeadReceipt> {
        Box::pin(async move {
            cancel.check()?;
            self.context(repository)?;
            locator.validate_for(repository)?;
            Err(unsupported())
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
                return Err(unsupported());
            }
            let page = api::page_cursor(cursor)?;
            if !context.can_list {
                return Err(unsupported());
            }
            let role = config::collection_role(collection);
            let package = context.settings.package(role);
            let per_page = limit.min(100).to_string();
            let mut query = vec![
                ("package_type", "generic"),
                ("package_name", package.as_str()),
                ("order_by", "version"),
                ("sort", "asc"),
                ("per_page", per_page.as_str()),
            ];
            if let Some(page) = page.as_deref() {
                query.push(("page", page));
            }
            let url = api::packages_url(&context.settings, &query)?;
            let credential = self.credential(&context.secret).await?;
            let mut response = self
                .send(
                    &context.settings,
                    Outgoing {
                        method: reqwest::Method::GET,
                        url,
                        operation: ProviderOperation::List,
                        credential: Some(&credential),
                        body: None,
                        content_length: None,
                    },
                    cancel,
                )
                .await?;
            if response.status != 200 {
                return Err(api::classify(
                    response.status,
                    &response.headers,
                    self.now(),
                ));
            }
            let next_cursor = api::next_page(&response.headers);
            let packages: Vec<PackageJson> = self.json(&mut response, cancel).await?;
            let mut objects = Vec::new();
            for entry in packages.into_iter().filter(|entry| entry.name == package) {
                let Ok(placement) = context.settings.place_version(role, &entry.version) else {
                    continue;
                };
                let files = self
                    .package_files(&context.settings, &credential, entry.id, cancel)
                    .await?;
                let mut matching = files.iter().filter(|file| file.file_name == placement.file);
                let Some(file) = matching.next() else {
                    continue;
                };
                if matching.next().is_some() {
                    continue;
                }
                objects.push(ObjectReceipt {
                    locator: context.settings.locator(&placement),
                    byte_length: file.size,
                    version: None,
                    checksum: file.file_sha256.as_ref().map(|value| Checksum {
                        algorithm: "sha256".into(),
                        value: value.clone(),
                        provider_verified: false,
                    }),
                    complete: true,
                });
            }
            Ok(ObjectPage {
                objects,
                next_cursor,
            })
        })
    }

    /// No session exists, so the resolution comes from the stored object alone.
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
            let placement = context.settings.place(intent.role, &intent.object_id)?;
            let credential = self.credential(&context.secret).await?;
            Ok(
                match self
                    .verify(context, &credential, intent, &placement, cancel)
                    .await?
                {
                    Verification::Present { provider_verified } => UploadResolution::Complete(
                        receipt(&context.settings, intent, &placement, provider_verified),
                    ),
                    Verification::Different => UploadResolution::Conflict,
                    Verification::Absent => UploadResolution::RestartRequired,
                },
            )
        })
    }

    /// Generic packages offer no stable head, so no locator can name one.
    fn head_locator(&self, repository: &RepositoryHandle) -> Result<RemoteLocator> {
        self.context(repository)?;
        Err(unsupported())
    }

    fn request_cost(
        &self,
        repository: &RepositoryHandle,
        operation: ProviderOperation,
    ) -> Result<Vec<RequestCost>> {
        Ok(api::costs(&self.context(repository)?.settings, operation))
    }
}

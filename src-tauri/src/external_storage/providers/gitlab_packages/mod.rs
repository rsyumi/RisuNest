//! GitLab Generic Packages adapter for GitLab.com and self-managed instances.
//!
//! Append-only backup target. Generic packages have no conditional write and no
//! documented overwrite-and-read path for one file name, so both head methods
//! answer `Unsupported` and all head capabilities are false. No Git protocol
//! is used.
//!
//! `ConnectionConfig`:
//!
//! * `provider` — `gitlab_packages`.
//! * `endpoint` — instance base URL, for example `https://gitlab.com` or
//!   `https://gitlab.example.com/gitlab`. The API is addressed under `/api/v4`.
//! * `profile` — absent (inferred from the host), `gitlabCom` or `selfManaged`.
//!   The GitLab.com profile pins the documented 5 GB generic file limit.
//! * `account_id` — an opaque token fingerprint used for request sharing.
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
//! `{"token":"<personal-or-project-access-token>"}`. The token is sent as
//! `PRIVATE-TOKEN` and must have the `api` scope plus Maintainer or Owner access
//! to the project.
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
//! second file under the same name instead of rejecting it. Package metadata
//! resolves that before every upload.
mod api;
mod config;
#[cfg(test)]
mod tests;

use super::{common, Dependencies};
use crate::external_storage::{
    capabilities::Capabilities,
    contract::*,
    http::HttpResponse,
};
use api::{Outgoing, PackageFileJson, PackageJson};
use config::{Credential, Placement, Settings};
use sha2::Digest;
use std::{collections::BTreeSet, pin::Pin, sync::Arc};
use tokio::io::{AsyncRead, AsyncReadExt};

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
fn complete_next_page(
    headers: &std::collections::BTreeMap<String, String>,
) -> Result<Option<String>> {
    let next = api::next_page(headers);
    if headers
        .get("x-next-page")
        .is_some_and(|value| !value.trim().is_empty() && next.is_none())
    {
        return Err(corrupt());
    }
    Ok(next)
}
fn io_error(cancel: &Cancellation) -> ProviderError {
    cancel
        .check()
        .err()
        .unwrap_or_else(|| ProviderError::new(ErrorKind::Transient))
}

struct Repository {
    settings: Settings,
    secret: SecretRef,
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
        let request = api::request(settings, outgoing);
        let account = request.account.clone();
        let response = self.deps.send(request, cancel).await?;
        if response.status == 429 {
            let now = self.now();
            let error = api::classify(response.status, &response.headers, now);
            self.deps.requests.observe_classified_error(
                &account,
                &error,
                &response.headers,
                now,
            )?;
        }
        Ok(response)
    }
    fn context<'a>(&self, repository: &'a RepositoryHandle) -> Result<&'a Repository> {
        let context = repository
            .context
            .downcast_ref::<Repository>()
            .ok_or_else(corrupt)?;
        if context.settings.connection_identity != repository.connection_identity
            || context.settings.connection_identity != repository.repository_id
            || context.settings.account != repository.account
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

    async fn complete_package_files(
        &self,
        settings: &Settings,
        credential: &Credential,
        package_id: u64,
        cancel: &Cancellation,
    ) -> Result<Vec<PackageFileJson>> {
        let mut files = Vec::new();
        let mut page = None;
        let mut seen = BTreeSet::new();
        loop {
            let mut query = vec![("per_page", "100")];
            if let Some(page) = page.as_deref() {
                query.push(("page", page));
            }
            let url = api::package_files_url(settings, package_id, &query)?;
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
            if response.status != 200 {
                return Err(api::classify(response.status, &response.headers, self.now()));
            }
            let next = complete_next_page(&response.headers)?;
            files.extend(self.json::<Vec<PackageFileJson>>(&mut response, cancel).await?);
            match next {
                None => return Ok(files),
                Some(next) if seen.len() < 10_000 && seen.insert(next.clone()) => {
                    page = Some(next)
                }
                Some(_) => return Err(corrupt()),
            }
        }
    }

    async fn require_resumable_create_layout(
        &self,
        settings: &Settings,
        credential: &Credential,
        cancel: &Cancellation,
    ) -> Result<()> {
        let marker = settings.marker();
        let owned_prefix = format!("{}.", settings.package_base);
        let mut marker_count = 0usize;
        let mut descriptor_count = 0usize;
        let mut page = None;
        let mut seen = BTreeSet::new();
        loop {
            let mut query = vec![
                ("package_type", "generic"),
                ("per_page", "100"),
            ];
            if let Some(page) = page.as_deref() {
                query.push(("page", page));
            }
            let url = api::packages_url(settings, &query)?;
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
            if response.status != 200 {
                return Err(api::classify(response.status, &response.headers, self.now()));
            }
            let next = complete_next_page(&response.headers)?;
            let packages: Vec<PackageJson> = self.json(&mut response, cancel).await?;
            for package in packages {
                if !package.name.starts_with(&owned_prefix) {
                    continue;
                }
                if package.id == 0 {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                if package.name == marker.package {
                    marker_count += 1;
                    let files = self
                        .complete_package_files(settings, credential, package.id, cancel)
                        .await?;
                    if marker_count > 1
                        || package.version != marker.version
                        || files.len() != 1
                        || files[0].id == 0
                        || files[0].file_name != marker.file
                        || files[0].size != settings.marker_body().len() as u64
                        || files[0].file_sha256.as_deref().is_some_and(|digest| {
                            !digest.eq_ignore_ascii_case(&hex::encode(sha2::Sha256::digest(
                                settings.marker_body(),
                            )))
                        })
                    {
                        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                    }
                    continue;
                }
                let suffix = package
                    .name
                    .strip_prefix(&owned_prefix)
                    .ok_or_else(corrupt)?;
                let Some(role) = config::role_from_name(suffix) else {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                };
                if role != ObjectRole::Descriptor {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                descriptor_count += 1;
                if descriptor_count > 1 {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
                let placement = settings
                    .place_version(role, &package.version)
                    .map_err(|_| ProviderError::new(ErrorKind::PreconditionFailed))?;
                let files = self
                    .complete_package_files(settings, credential, package.id, cancel)
                    .await?;
                if files.len() != 1
                    || files[0].id == 0
                    || files[0].file_name != placement.file
                    || files[0].size == 0
                    || files[0]
                        .file_sha256
                        .as_deref()
                        .is_some_and(|digest| {
                            digest.len() != 64
                                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                        })
                {
                    return Err(ProviderError::new(ErrorKind::PreconditionFailed));
                }
            }
            match next {
                None => break,
                Some(next) if seen.len() < 10_000 && seen.insert(next.clone()) => {
                    page = Some(next)
                }
                Some(_) => return Err(corrupt()),
            }
        }
        if marker_count != 1 {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }

    async fn require_empty_create_layout(
        &self,
        settings: &Settings,
        credential: &Credential,
        cancel: &Cancellation,
    ) -> Result<()> {
        let owned_prefix = format!("{}.", settings.package_base);
        let mut page = None;
        let mut seen = BTreeSet::new();
        loop {
            let mut query = vec![("package_type", "generic"), ("per_page", "100")];
            if let Some(page) = page.as_deref() {
                query.push(("page", page));
            }
            let url = api::packages_url(settings, &query)?;
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
            if response.status != 200 {
                return Err(api::classify(response.status, &response.headers, self.now()));
            }
            let next = complete_next_page(&response.headers)?;
            let packages: Vec<PackageJson> = self.json(&mut response, cancel).await?;
            if packages
                .iter()
                .any(|package| package.name.starts_with(&owned_prefix))
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
            match next {
                None => return Ok(()),
                Some(next) if seen.len() < 10_000 && seen.insert(next.clone()) => {
                    page = Some(next)
                }
                Some(_) => return Err(corrupt()),
            }
        }
    }

    /// One offset page of the package a role lives in, appended to `objects`.
    /// The answer is the following page inside that same package.
    #[allow(clippy::too_many_arguments)]
    async fn list_package_page(
        &self,
        context: &Repository,
        credential: &Credential,
        role: ObjectRole,
        page: Option<&str>,
        limit: u16,
        objects: &mut Vec<ObjectReceipt>,
        cancel: &Cancellation,
    ) -> Result<Option<String>> {
        let settings = &context.settings;
        let package = settings.package(role);
        let per_page = limit.min(100).to_string();
        let mut query = vec![
            ("package_type", "generic"),
            ("package_name", package.as_str()),
            ("order_by", "version"),
            ("sort", "asc"),
            ("per_page", per_page.as_str()),
        ];
        if let Some(page) = page {
            query.push(("page", page));
        }
        let url = api::packages_url(settings, &query)?;
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
        if response.status != 200 {
            return Err(api::classify(
                response.status,
                &response.headers,
                self.now(),
            ));
        }
        let next_page = api::next_page(&response.headers);
        let packages: Vec<PackageJson> = self.json(&mut response, cancel).await?;
        for entry in packages.into_iter().filter(|entry| entry.name == package) {
            let Ok(placement) = settings.place_version(role, &entry.version) else {
                continue;
            };
            let files = self
                .package_files(settings, credential, entry.id, cancel)
                .await?;
            let mut matching = files.iter().filter(|file| file.file_name == placement.file);
            let Some(file) = matching.next() else {
                continue;
            };
            if matching.next().is_some() {
                continue;
            }
            objects.push(ObjectReceipt {
                locator: settings.locator(&placement),
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
        Ok(next_page)
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
                if !agrees {
                    return Err(corrupt());
                }
            }
            201 => {}
            // The instance forbids duplicates and the marker already exists.
            400 => {
                return Err(ProviderError {
                    kind: ErrorKind::PreconditionFailed,
                    http_status: Some(400),
                    retry_at_ms: None,
                })
            }
            status => return Err(api::classify(status, &response.headers, self.now())),
        }
        let url = api::object_url(settings, &marker, &[])?;
        let mut verification = self
            .send(
                settings,
                Outgoing {
                    method: reqwest::Method::GET,
                    url,
                    operation: ProviderOperation::Metadata,
                    credential: Some(credential),
                    body: None,
                    content_length: None,
                },
                cancel,
            )
            .await?;
        match verification.status {
            200 => {
                let stored =
                    common::read_bounded(&mut verification.body, 64 * 1024, cancel).await?;
                if stored == settings.marker_body() {
                    Ok(())
                } else {
                    Err(ProviderError::new(ErrorKind::PreconditionFailed))
                }
            }
            404 => Err(ProviderError::new(ErrorKind::Transient)),
            status => Err(api::classify(status, &verification.headers, self.now())),
        }
    }

    async fn require_listing(
        &self,
        settings: &Settings,
        credential: &Credential,
        cancel: &Cancellation,
    ) -> Result<()> {
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
            200 => Ok(()),
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

fn capabilities(settings: &Settings) -> Capabilities {
    Capabilities {
        immutable_create: true,
        direct_complete_read: true,
        atomic_create_head: false,
        conditional_head_update: false,
        stable_head_replace: false,
        head_read_after_write: false,
        head_retry_control: false,
        snapshot_discovery: true,
        lease_operations: true,
        delete_objects: true,
        conditional_get: false,
        range: false,
        resumable_upload: false,
        max_stored_bytes: settings.max_stored_bytes,
        sdk_overhead_bytes: 0,
        upload_alignment: 1,
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
            if mode == OpenMode::Create {
                self.require_empty_create_layout(&settings, &credential, cancel)
                    .await?;
            }
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
                (200, OpenMode::Existing | OpenMode::ResumeCreate) => {
                    let body = common::read_bounded(&mut response.body, 64 * 1024, cancel).await?;
                    if body != settings.marker_body() {
                        return Err(if mode == OpenMode::ResumeCreate {
                            ProviderError::new(ErrorKind::PreconditionFailed)
                        } else {
                            corrupt()
                        });
                    }
                }
                (404, OpenMode::Existing) => {
                    return Err(ProviderError {
                        kind: ErrorKind::NotFound,
                        http_status: Some(404),
                        retry_at_ms: None,
                    })
                }
                (404, OpenMode::Create | OpenMode::ResumeCreate) => {
                    self.write_marker(&settings, &credential, cancel).await?;
                }
                (status, _) => {
                    return Err(api::classify(status, &response.headers, self.now()));
                }
            }
            match mode {
                OpenMode::Create => {
                    self.require_resumable_create_layout(&settings, &credential, cancel)
                        .await?;
                }
                OpenMode::ResumeCreate => {
                    self.require_resumable_create_layout(&settings, &credential, cancel)
                        .await?;
                }
                OpenMode::Existing => self.require_listing(&settings, &credential, cancel).await?,
            }
            let capabilities = capabilities(&settings);
            let handle = RepositoryHandle {
                repository_id: settings.connection_identity.clone(),
                connection_identity: settings.connection_identity.clone(),
                account: settings.account.clone(),
                context: Box::new(Repository {
                    settings,
                    secret: secret.clone(),
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

    /// Removal addresses a numeric package file id, so the package and its file
    /// listing are resolved first.
    fn delete_object<'a>(
        &'a self,
        repository: &'a RepositoryHandle,
        locator: &'a RemoteLocator,
        cancel: &'a Cancellation,
    ) -> ProviderFuture<'a, ()> {
        Box::pin(async move {
            cancel.check()?;
            let context = self.context(repository)?;
            locator.validate_for(repository)?;
            let settings = &context.settings;
            let (role, placement) = settings.parse_object(&locator.object)?;
            if role == ObjectRole::Descriptor {
                return Err(unsupported());
            }
            let credential = self.credential(&context.secret).await?;
            let Some(package_id) = self
                .locate_package(settings, &credential, &placement.package, &placement.version, cancel)
                .await?
            else {
                return Ok(());
            };
            let files = self
                .package_files(settings, &credential, package_id, cancel)
                .await?;
            let Some(file) = files.iter().find(|file| file.file_name == placement.file) else {
                return Ok(());
            };
            let response = self
                .send(
                    settings,
                    Outgoing {
                        method: reqwest::Method::DELETE,
                        url: api::package_file_url(settings, package_id, file.id)?,
                        operation: ProviderOperation::Delete,
                        credential: Some(&credential),
                        body: None,
                        content_length: None,
                    },
                    cancel,
                )
                .await?;
            match response.status {
                200 | 204 | 404 => Ok(()),
                status => Err(api::classify(status, &response.headers, self.now())),
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
            let context = self.context(repository)?;
            if limit == 0 || limit > 1000 {
                return Err(unsupported());
            }
            let roles = config::collection_roles(collection);
            let (mut index, mut page) = api::list_cursor(cursor, roles.len())?;
            let credential = self.credential(&context.secret).await?;
            let mut objects = Vec::new();
            let mut next_cursor = None;
            loop {
                let next_page = self
                    .list_package_page(
                        context,
                        &credential,
                        roles[index],
                        page.as_deref(),
                        limit,
                        &mut objects,
                        cancel,
                    )
                    .await?;
                if let Some(next) = next_page {
                    next_cursor = Some(format!("{index}:{next}"));
                    break;
                }
                index += 1;
                page = None;
                if index >= roles.len() {
                    break;
                }
                // An exhausted package must not end the page empty handed: the
                // remaining packages of this collection are read in the same call.
                if !objects.is_empty() {
                    next_cursor = Some(format!("{index}:1"));
                    break;
                }
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
        _resume: Option<&'a ResumeState>,
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

}

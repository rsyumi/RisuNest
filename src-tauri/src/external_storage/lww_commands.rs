use super::{
    connection_commands,
    contract::*,
    lww_engine::{ExternalLwwEngine, PublicationResult},
    lww_segment,
    providers::Dependencies,
    runtime,
};
use crate::persistent_store::{
    lww::{Header, MessageLocator, NewDevicePreparation, StageReceive},
    sync_selection::SyncTarget,
};
use crate::native_log::logged;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex, OnceLock},
    time::Instant,
};
use tauri::{AppHandle,Manager};

struct Session {
    progress: super::progress::Observer,
    engine: Option<ExternalLwwEngine>,
    identity: Option<Vec<u8>>,
    dependencies: Option<Dependencies>,
    fresh_after: Instant,
    clock_refresh_attempted: bool,
    cancel: Cancellation,
    /// Ids of the receive pages the last listing produced, in order.
    receive_pages: VecDeque<String>,
    /// The last listing holds segments its pages stopped before.
    receive_more: bool,
    /// A resume kept the pages queued, so the receive that finishes them lists again for what arrived meanwhile.
    receive_relist: bool,
}
impl Session {
    fn new() -> Self {
        Self {
            progress: Default::default(),
            engine: None,
            identity: None,
            dependencies: None,
            fresh_after: Instant::now(),
            clock_refresh_attempted: false,
            cancel: Cancellation::default(),
            receive_pages: VecDeque::new(),
            receive_more: false,
            receive_relist: false,
        }
    }

    fn reset(&mut self) {
        self.engine = None;
        self.identity = None;
        self.dependencies = None;
        self.receive_pages.clear();
        self.receive_more = false;
        self.receive_relist = false;
        self.fresh_after = Instant::now();
    }

    fn use_cancellation(&mut self, cancel: &Cancellation) {
        if !self.cancel.same(cancel)
            || self.engine.as_ref().is_some_and(|engine| engine.admitted_upper().is_err())
        {
            if let Some(engine) = self.engine.as_mut() { engine.invalidate_clock(); }
            self.fresh_after = Instant::now();
        }
        self.cancel = cancel.clone();
    }

    /// Starts again after the app returns or the binding resumes: new work takes `cancel` and the clock is read
    /// again. The pages the last listing stored stay queued, so their segments are not read again, and the receive
    /// that finishes them lists again.
    fn resume(&mut self, cancel: Cancellation) {
        self.cancel = cancel;
        self.fresh_after = Instant::now();
        self.receive_relist = !self.receive_pages.is_empty();
        if let Some(engine) = self.engine.as_mut() {
            engine.invalidate_clock()
        }
    }

    fn finish_maintenance<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.is_err() { self.reset(); }
        result
    }
}

// Only effective opening inputs belong here; completion timestamps never reopen a session.
fn session_identity(
    stored: &super::connection_store::StoredConnection,
    capabilities: &super::capabilities::Capabilities,
) -> Result<Vec<u8>> {
    serde_json::to_vec(&(
        &stored.id, &stored.config, &stored.descriptor, &stored.descriptor_locator,
        &stored.provider_repository_id, &stored.credential_ref, &stored.root_key_ref,
        capabilities,
    )).map_err(runtime::local_error)
}

struct Context {
    session: tokio::sync::Mutex<Session>,
    cancel: Mutex<Cancellation>,
    maintenance:tokio::sync::Mutex<Session>,
    checkpoints:tokio::sync::Mutex<super::lww_compaction::CheckpointSummaries>,
    turn:Mutex<super::lww_compaction::CompactionTurn>,
}
static CONTEXTS: OnceLock<Mutex<BTreeMap<String, Arc<Context>>>> = OnceLock::new();
#[cfg(test)]
#[path = "lww_scheduling_command_tests.rs"]
mod scheduling_command_tests;
pub(crate) async fn invalidate_connection_cache(id: &str) -> Result<()> {
    let existing = CONTEXTS.get().map(|contexts| {
        contexts.lock().map_err(runtime::local_error).map(|contexts| contexts.get(id).cloned())
    }).transpose()?.flatten();
    if let Some(context) = existing {
        context.cancel.lock().map_err(runtime::local_error)?.cancel();
        context.checkpoints.lock().await.invalidate();
    }
    Ok(())
}
fn context(id: &str) -> Result<Arc<Context>> {
    if id.is_empty() {
        return Err(lww_segment::corrupt());
    }
    let mut contexts = CONTEXTS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(runtime::local_error)?;
    Ok(contexts
        .entry(id.into())
        .or_insert_with(|| {
            Arc::new(Context {
                session: tokio::sync::Mutex::new(Session::new()),
                cancel: Mutex::new(Cancellation::default()),
                maintenance: tokio::sync::Mutex::new(Session::new()),
                checkpoints:Default::default(),
                turn:Mutex::new(Default::default()),
            })
        })
        .clone())
}
/// Sync runs only while the repository is at this app's format level.
fn require_format(engine: &ExternalLwwEngine) -> Result<()> {
    let store = super::connection_store::ConnectionStore::open(&engine.connection_root)?;
    format_outcome(store.format_level(&engine.connection_id)?)
}
fn format_outcome(level: u64) -> Result<()> {
    match level.cmp(&crate::server_sync::library_format::LEVEL) {
        std::cmp::Ordering::Equal => Ok(()),
        std::cmp::Ordering::Greater => Err(ProviderError::new(ErrorKind::AppUpdateRequired)),
        std::cmp::Ordering::Less => Err(ProviderError::new(ErrorKind::LibraryFormatSwitchRequired)),
    }
}
/// Whether a repository on `provider` can be used for sync.
pub(crate) fn syncs_through(provider: &str) -> bool {
    matches!(provider, "webdav" | "s3" | "google_drive" | "onedrive")
}
async fn connect(app: &AppHandle, id: &str, session: &mut Session) -> Result<()> {
    let root = runtime::root(app)?;
    let stored = super::connection_store::ConnectionStore::open(&root)?.read(id)?;
    let cancel = session.cancel.clone();
    connect_with(&root, &stored, session,
        connection_commands::open_connected_with_cancel(app, id, &cancel)).await
}

async fn connect_with(
    root: &std::path::Path,
    stored: &super::connection_store::StoredConnection,
    session: &mut Session,
    opening: impl std::future::Future<Output = Result<connection_commands::ConnectedRepository>>,
) -> Result<()> {
    session.cancel.check()?;
    let identity = session_identity(stored, &stored.capabilities)?;
    if session.engine.is_some() && session.identity.as_ref() == Some(&identity) {
        return Ok(());
    }
    session.reset();
    let connected = opening.await?;
    // Capabilities are negotiated on open, while invalidation compares the persisted inputs.
    if session_identity(&connected.stored, &stored.capabilities)? != identity {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if !syncs_through(&connected.stored.config.provider)
        || connected.stored.descriptor.publication_strategy.is_none()
    {
        return Err(ProviderError::new(ErrorKind::Unsupported));
    }
    super::connection::validate_sync_location(&connected.stored.config)?;
    session.cancel.check()?;
    session.dependencies = Some(connected.dependencies);
    session.engine = Some(ExternalLwwEngine {
        provider: session.progress.wrap(connected.provider),
        repository: connected.handle,
        library: connected.stored.descriptor.repository_id.clone(),
        root_key: connected.root_key,
        admission: None,
        #[cfg(test)]
        test_lease_clock: None,
        connection_id: stored.id.clone(),
        connection_root: root.to_path_buf(),
        capabilities: connected.stored.capabilities,
        descriptor: connected.stored.descriptor,
    });
    session.identity = Some(identity);
    Ok(())
}
async fn open(app: &AppHandle, id: &str, session: &mut Session) -> Result<()> {
    connect(app, id, session).await?;
    session.clock_refresh_attempted = false;
    admit_clock(session).await
}
async fn admit_clock(session: &mut Session) -> Result<()> {
    let engine = session.engine.as_mut().ok_or_else(lww_segment::corrupt)?;
    if engine.admitted_upper().is_ok() {
        return Ok(());
    }
    let requests = &session.dependencies.as_ref().ok_or_else(lww_segment::corrupt)?.requests;
    let cached = requests.require_clock_sample_after(&engine.repository.account, session.fresh_after)
        .and_then(|sample| { engine.admit(&sample)?; engine.admitted_upper().map(|_| ()) });
    match cached {
        Ok(()) => return Ok(()),
        Err(error) if error.kind != ErrorKind::ClockSkew => return Err(error),
        Err(_) => {}
    }
    refresh_clock(session).await
}
async fn refresh_clock(session: &mut Session) -> Result<()> {
    session.cancel.check()?;
    session.clock_refresh_attempted = true;
    let engine = session.engine.as_mut().ok_or_else(lww_segment::corrupt)?;
    let requests = &session.dependencies.as_ref().ok_or_else(lww_segment::corrupt)?.requests;
    engine.invalidate_clock();
    session.fresh_after = Instant::now();
    let refreshed = async {
        engine.provider.list_objects(&engine.repository, Collection::Segments, None, 1, &session.cancel).await?;
        let sample = requests.require_clock_sample_after(&engine.repository.account, session.fresh_after)?;
        engine.admit(&sample)?;
        engine.admitted_upper().map(|_| ())
    }.await;
    if refreshed.is_err() { engine.invalidate_clock(); }
    refreshed.map_err(super::clock::after_refresh)
}
async fn retry_clock_failure(session: &mut Session, error: ProviderError) -> Result<()> {
    if error.kind != ErrorKind::ClockSkew {
        return Err(error);
    }
    if session.clock_refresh_attempted {
        return Err(super::clock::after_refresh(error));
    }
    if session.engine.as_ref().ok_or_else(lww_segment::corrupt)?.admitted_upper().is_ok() {
        return Err(error);
    }
    refresh_clock(session).await
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExitTarget {
    revision: risunest_sync_wire::stamp::DecimalU64,
    library_epoch: String,
    selection_epoch: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Request {
    pub connection_id: String,
    #[serde(flatten)]
    pub header: Header,
    #[serde(default)]
    pub generating: Vec<MessageLocator>,
    #[serde(default)]
    exit_target: Option<ExitTarget>,
    #[serde(default)]
    turn_limit: Option<u32>,
    #[serde(default)]
    complete_available: bool,
}
impl Request {
    fn check_publish_limit(&self) -> Result<()> {
        if self.turn_limit == Some(0) || (self.exit_target.is_some() && self.turn_limit.is_some()) {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        Ok(())
    }
}
fn check(
    app: &AppHandle,
    request: &Request,
    selected: bool,
) -> Result<crate::persistent_store::PersistentStore> {
    let store = runtime::native_store(app)?;
    check_store(&store, request, selected)?;
    Ok(store)
}
fn check_store(
    store: &crate::persistent_store::PersistentStore,
    request: &Request,
    selected: bool,
) -> Result<()> {
    if store
        .lww_binding_authority()
        .map_err(runtime::local_error)?
        != request.header.binding_authority
    {
        return Err(ProviderError::new(ErrorKind::PreconditionFailed));
    }
    if selected
        && store
            .lww_binding_state()
            .map_err(runtime::local_error)?
            .target
            != SyncTarget::External(request.connection_id.clone())
    {
        return Err(ProviderError::new(ErrorKind::Cancelled));
    }
    Ok(())
}
/// The connection's cancellation for a new binding attempt. Turning sync off
/// leaves the old token cancelled, so the attempt takes a fresh one; a live
/// token is kept so a fence during the attempt still stops it.
fn binding_cancellation(context: &Context) -> Result<Cancellation> {
    let mut cancel = context.cancel.lock().map_err(runtime::local_error)?;
    if cancel.check().is_err() {
        *cancel = Cancellation::default();
    }
    Ok(cancel.clone())
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Inspection {
    inspection_id: String,
    target_id: String,
    library_id: String,
    empty: bool,
    previously_bound_library: bool,
    registration_changed: bool,
    server_restored: bool,
}
#[tauri::command]
pub(crate) async fn external_lww_inspect(app: AppHandle, request: Request, webview: tauri::Webview, progress: Option<tauri::ipc::JavaScriptChannelId>) -> Result<Inspection> {
    logged("external_lww_inspect", Box::pin(async move {
        let context = context(&request.connection_id)?;
        let mut session = context.session.lock().await;
        let _progress = session.progress.begin(progress.map(|channel| channel.channel_on(webview)));
        session.cancel = binding_cancellation(&context)?;
        open(&app, &request.connection_id, &mut session).await?;
        let store = check(&app, &request, false)?;
        let engine = session.engine.as_ref().ok_or_else(lww_segment::corrupt)?;
        let objects = engine.listing(&session.cancel).await?;
        let target_id = engine.repository.connection_identity.clone();
        let library_id = engine.library.clone();
        let inspection_id = store
            .register_lww_binding_inspection(
                request.header.binding_authority,
                &SyncTarget::External(request.connection_id),
                &target_id,
                &library_id,
            )
            .map_err(runtime::local_error)?;
        let previously_bound_library = store
            .lww_previously_bound(&library_id, &target_id)
            .map_err(runtime::local_error)?;
        Ok(Inspection {
            inspection_id,
            target_id,
            library_id,
            empty: objects.is_empty() && engine.snapshot_listing(&session.cancel).await?.is_empty(),
            previously_bound_library,
            registration_changed: false,
            server_restored: false,
        })
    }).await)
}
pub(crate) struct StageRequest {
    request: Request,
    inspection_id: String,
    target_id: String,
    library_id: String,
}
impl<'de> Deserialize<'de> for StageRequest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Input {
            connection_id: String,
            binding_authority: risunest_sync_wire::stamp::DecimalU64,
            request_id: String,
            #[serde(default)]
            generating: Vec<MessageLocator>,
            inspection_id: String,
            target_id: String,
            library_id: String,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            request: Request {
                connection_id: input.connection_id,
                header: Header {
                    binding_authority: input.binding_authority,
                    request_id: input.request_id,
                },
                generating: input.generating,
                exit_target: None,
                turn_limit: None,
                complete_available: false,
            },
            inspection_id: input.inspection_id,
            target_id: input.target_id,
            library_id: input.library_id,
        })
    }
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Staged {
    target_id: String,
    library_id: String,
    staging_id: String,
    receive_id: String,
}
#[tauri::command]
pub(crate) async fn external_lww_stage_binding(
    app: AppHandle,
    request: StageRequest,
    webview: tauri::Webview, progress: Option<tauri::ipc::JavaScriptChannelId>,
) -> Result<Staged> {
    logged("external_lww_stage_binding", Box::pin(async move {
        let context = context(&request.request.connection_id)?;
        let mut session = context.session.lock().await;
        let _progress = session.progress.begin(progress.map(|channel| channel.channel_on(webview)));
        open(&app, &request.request.connection_id, &mut session).await?;
        let mut store = check(&app, &request.request, false)?;
        let engine = session.engine.as_ref().ok_or_else(lww_segment::corrupt)?;
        if request.library_id != engine.library
            || request.target_id != engine.repository.connection_identity
        {
            return Err(lww_segment::corrupt());
        }
        let stage = loop {
            let engine = session.engine.as_ref().ok_or_else(lww_segment::corrupt)?;
            let result = session.progress.scope(engine.stage_binding(
                &mut store, &request.request.header, &request.inspection_id, &session.cancel,
            )).await;
            match result {
                Ok(stage) => break stage,
                Err(error) => retry_clock_failure(&mut session, error).await?,
            }
        };
        Ok(Staged {
            target_id: request.target_id,
            library_id: request.library_id,
            staging_id: stage.staging_id,
            receive_id: request.request.header.request_id,
        })
    }).await)
}
#[tauri::command]
pub(crate) async fn external_lww_publish(
    app: AppHandle,
    request: Request,
    webview: tauri::Webview, progress: Option<tauri::ipc::JavaScriptChannelId>,
) -> Result<PublicationResult> {
    logged("external_lww_publish", Box::pin(async move {
        request.check_publish_limit()?;
        let state = app.state::<crate::persistent_store::PersistentStoreState>();
        let _operation = state.admit_renderer_operation().map_err(runtime::local_error)?;
        let context = context(&request.connection_id)?;
        let mut session = context.session.lock().await;
        let _progress = session.progress.begin(progress.map(|channel| channel.channel_on(webview)));
        open(&app, &request.connection_id, &mut session).await?;
        require_format(session.engine.as_ref().ok_or_else(lww_segment::corrupt)?)?;
        let mut store = state.open_admitted_native_job_store(&_operation).map_err(runtime::local_error)?;
        check_store(&store, &request, true)?;
        if let Some(target) = &request.exit_target {
            let identity = store.external_identity().map_err(runtime::local_error)?;
            if identity.library_epoch != target.library_epoch
                || identity.selection_epoch != target.selection_epoch
                || u64::try_from(identity.revision).ok() != Some(target.revision.0)
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        if request.exit_target.is_none() {
            store
                .lww_finish_initial_publication(&request.header)
                .map_err(runtime::local_error)?;
        }
        let result = publish_round(
            &mut session,
            &mut store,
            request.header.binding_authority,
            &request.generating,
            request.turn_limit,
        )
        .await?;
        if result.segments.0 > 0 {
            context.turn.lock().map_err(runtime::local_error)?.published(Instant::now());
        }
        if let Some(target) = &request.exit_target {
            let identity = store.external_identity().map_err(runtime::local_error)?;
            if identity.library_epoch != target.library_epoch
                || identity.selection_epoch != target.selection_epoch
                || u64::try_from(identity.revision).ok() != Some(target.revision.0)
                || !store
                    .lww_read_outbox(request.header.binding_authority, 1)
                    .map_err(runtime::local_error)?
                    .entries
                    .is_empty()
            {
                return Err(ProviderError::new(ErrorKind::PreconditionFailed));
            }
        }
        Ok(result)
    }).await)
}
#[tauri::command]
pub(crate) async fn external_lww_receive(
    app: AppHandle,
    request: Request,
    webview: tauri::Webview, progress: Option<tauri::ipc::JavaScriptChannelId>,
) -> Result<Option<StageReceive>> {
    logged("external_lww_receive", Box::pin(async move {
        let context = context(&request.connection_id)?;
        let mut session = context.session.lock().await;
        let _progress = session.progress.begin(progress.map(|channel| channel.channel_on(webview)));
        open(&app, &request.connection_id, &mut session).await?;
        require_format(session.engine.as_ref().ok_or_else(lww_segment::corrupt)?)?;
        let mut store = check(&app, &request, true)?;
        let (page, _) = receive_page(
            &mut store,
            &mut session,
            &context.checkpoints,
            request.header.binding_authority,
            request.complete_available,
        )
        .await?;
        let own = store.lww_clock_state().map_err(runtime::local_error)?.writer_id;
        if page.as_ref().is_some_and(|page| page.progress.writer_id.as_deref() != Some(own.as_str())) {
            context.turn.lock().map_err(runtime::local_error)?.observed_foreign(Instant::now());
        }
        Ok(page)
    }).await)
}

/// Reads the repository descriptor again and succeeds once the repository is
/// at this app's format level, as after another device switched it.
#[tauri::command]
pub(crate) async fn external_lww_check_format(app: AppHandle, request: Request) -> Result<()> {
    logged("external_lww_check_format", Box::pin(async move {
        let context = context(&request.connection_id)?;
        let session = context.session.lock().await;
        check(&app, &request, true)?;
        connection_commands::open_connected_with_cancel(&app, &request.connection_id, &session.cancel).await?;
        let store = super::connection_store::ConnectionStore::open(&runtime::root(&app)?)?;
        format_outcome(store.format_level(&request.connection_id)?)
    }).await)
}
/// Raises the repository to this app's format level: the descriptor is
/// replaced and read back, then a segment that holds only the level unit is
/// published. A repository already at the level is left as it is, and the
/// segment is published again.
#[tauri::command]
pub(crate) async fn external_lww_switch_format(
    app: AppHandle,
    request: Request,
    webview: tauri::Webview, progress: Option<tauri::ipc::JavaScriptChannelId>,
) -> Result<()> {
    logged("external_lww_switch_format", Box::pin(async move {
        let state = app.state::<crate::persistent_store::PersistentStoreState>();
        let _operation = state.admit_renderer_operation().map_err(runtime::local_error)?;
        let context = context(&request.connection_id)?;
        let mut session = context.session.lock().await;
        let _progress = session.progress.begin(progress.map(|channel| channel.channel_on(webview)));
        open(&app, &request.connection_id, &mut session).await?;
        let mut store = state.open_admitted_native_job_store(&_operation).map_err(runtime::local_error)?;
        check_store(&store, &request, true)?;
        let root = runtime::root(&app)?;
        let level = crate::server_sync::library_format::LEVEL;
        let connected = connection_commands::open_connected_with_cancel(&app, &request.connection_id, &session.cancel).await?;
        let recorded = super::connection_store::ConnectionStore::open(&root)?.format_level(&request.connection_id)?;
        if recorded > level {
            return Err(ProviderError::new(ErrorKind::AppUpdateRequired));
        }
        let stored = &connected.stored;
        let switched = stored.descriptor.clone().with_format_level(level)
            .map_err(|_| ProviderError::new(ErrorKind::Unsupported))?;
        super::descriptor::replace(connected.provider.as_ref(), &connected.handle, &stored.descriptor_locator,
            &switched, &connected.root_key, &session.cancel).await?;
        // Only the level read back confirms the switch.
        let current = super::descriptor::read(&root, connected.provider.as_ref(), &connected.handle,
            &stored.descriptor_locator, &stored.descriptor, &connected.root_key, &session.cancel).await?;
        let store_levels = super::connection_store::ConnectionStore::open(&root)?;
        store_levels.record_format_level(&request.connection_id, current.level())?;
        if current.level() < level {
            return Err(ProviderError::new(ErrorKind::PreconditionFailed));
        }
        format_outcome(store_levels.format_level(&request.connection_id)?)?;
        let session = &mut *session;
        loop {
            let engine = session.engine.as_mut().ok_or_else(lww_segment::corrupt)?;
            match engine.publish_format_level(&mut store, request.header.binding_authority, &session.cancel).await {
                Ok(_) => return Ok(()),
                Err(error) => retry_clock_failure(session, error).await?,
            }
        }
    }).await)
}

/// A publication that sent segments or finishing every receive page of a
/// listing that held nothing more completes a sync round, so only those record
/// the connection's sync time.
fn record_sync_round(engine: &ExternalLwwEngine) {
    runtime::record_connection_completion_at(
        &engine.connection_root,
        &engine.connection_id,
        super::connection_store::CompletionKind::Sync,
    );
}

async fn publish_round(
    session: &mut Session,
    store: &mut crate::persistent_store::PersistentStore,
    authority: risunest_sync_wire::stamp::DecimalU64,
    generating: &[MessageLocator],
    turn_limit: Option<u32>,
) -> Result<PublicationResult> {
    let mut result = PublicationResult::default();
    loop {
        let engine = session.engine.as_mut().ok_or_else(lww_segment::corrupt)?;
        match engine.publish_accumulating(store, authority, generating, turn_limit, &session.cancel, &mut result).await {
            Ok(()) => break,
            Err(error) => retry_clock_failure(session, error).await?,
        }
    }
    if result.segments.0 > 0 {
        record_sync_round(session.engine.as_ref().ok_or_else(lww_segment::corrupt)?);
    }
    Ok(result)
}

/// One page per call: pages from the last listing come first, and a new
/// listing runs on the next round after every page is finished. It runs at
/// once while the last listing holds segments its batch stopped before, after
/// a resume kept pages queued, or with `complete_available`; then no page
/// means none is available. Returns the page and whether this call recorded
/// the sync time.
async fn receive_page(
    store: &mut crate::persistent_store::PersistentStore,
    session: &mut Session,
    checkpoints: &tokio::sync::Mutex<super::lww_compaction::CheckpointSummaries>,
    authority: risunest_sync_wire::stamp::DecimalU64,
    complete_available: bool,
) -> Result<(Option<StageReceive>, bool)> {
    let mut finished = 0;
    loop {
        let Session { engine, receive_pages: queue, receive_more: more, receive_relist: relist, cancel, .. } = session;
        let engine = engine.as_ref().ok_or_else(lww_segment::corrupt)?;
        let result = async {
            let mut page = next_receive_page(store, queue, authority, &mut finished)?;
            let recorded = page.is_none() && finished > 0 && !*more;
            if page.is_none() && (finished == 0 || complete_available || *more || *relist) {
                let batch = engine.receive_requests_cached(store, authority, checkpoints, cancel).await?;
                *relist = false;
                *more = batch.more;
                *queue = batch.pages.into();
                page = next_receive_page(store, queue, authority, &mut finished)?;
            }
            if recorded {
                record_sync_round(engine);
            }
            if let Some(page) = &page {
                ExternalLwwEngine::prepare_archive_bodies(store, page, cancel).await?;
            }
            Ok((page, recorded))
        }.await;
        match result {
            Ok(value) => return Ok(value),
            Err(error) => retry_clock_failure(session, error).await?,
        }
    }
}

/// The first unfinished page in `queue`, dropping finished ones and counting
/// them in `finished`. A page of another binding empties the queue.
fn next_receive_page(
    store: &crate::persistent_store::PersistentStore,
    queue: &mut VecDeque<String>,
    authority: risunest_sync_wire::stamp::DecimalU64,
    finished: &mut usize,
) -> Result<Option<StageReceive>> {
    while let Some(id) = queue.front() {
        match store.external_lww_unfinished_receive(id).map_err(runtime::local_error)? {
            Some(stored) if stored.header.binding_authority == authority => return Ok(Some(stored)),
            Some(_) => queue.clear(),
            None => {
                queue.pop_front();
                *finished += 1;
            }
        }
    }
    Ok(None)
}

#[tauri::command]
pub(crate) async fn external_lww_maintenance(app:AppHandle,request:Request)->Result<Option<serde_json::Value>> {
    logged("external_lww_maintenance", Box::pin(async move {
        let context=context(&request.connection_id)?;
        let Ok(mut session)=context.maintenance.try_lock() else {return Ok(None)};
        let cancel=context.cancel.lock().map_err(runtime::local_error)?.clone();
        let result=maintain(&app,&request,&context,&mut session,&cancel).await;
        session.finish_maintenance(result)
    }).await)
}
/// One maintenance tick on the engine kept from earlier ticks. A replaced
/// cancellation or an expired clock admission takes a fresh clock sample, and
/// a changed connection or any failure connects again.
async fn maintain(app:&AppHandle,request:&Request,context:&Context,session:&mut Session,cancel:&Cancellation)->Result<Option<serde_json::Value>> {
    session.use_cancellation(cancel);
    open(app,&request.connection_id,session).await?;
    require_format(session.engine.as_ref().ok_or_else(lww_segment::corrupt)?)?;
    loop {
        match maintain_round(app,request,context,session,cancel).await {
            Ok(result)=>return Ok(result),
            Err(error)=>retry_clock_failure(session,error).await?,
        }
    }
}
async fn maintain_round(app:&AppHandle,request:&Request,context:&Context,session:&Session,cancel:&Cancellation)->Result<Option<serde_json::Value>> {
    let root=runtime::root(app)?;
    let mut store=check(app,request,true)?;
    let writer=store.lww_clock_state().map_err(runtime::local_error)?.writer_id;
    let engine=session.engine.as_ref().ok_or_else(lww_segment::corrupt)?;
    let due=engine.maintenance_needed_cached(&context.checkpoints,cancel).await?;
    let compact=context.turn.lock().map_err(runtime::local_error)?.ready(due,Instant::now());
    let job=uuid::Uuid::new_v4().to_string();
    let protection=super::leases::LeaseContext{root:&root,connection_id:&request.connection_id,writer_id:&writer,descriptor:&engine.descriptor,root_key:&engine.root_key,provider:engine.provider.as_ref(),repository:&engine.repository,clock:super::leases::system_clock(),protection_supported:engine.capabilities.lease_operations,ledger:Some(app.state::<super::job_store::JobCommandState>().lease_ledger()?)};
    let snapshot_id=if compact {
        let owner=match super::leases::admit_shared_work(&protection,&job,cancel).await? {
            super::leases::Admission::Admitted(owner)=>owner,
            super::leases::Admission::Yield{..}=>return Ok(None),
            super::leases::Admission::UnsupportedProtection=>return Err(ProviderError::new(ErrorKind::Unsupported)),
        };
        let directory=root.join("external-storage").join("maintenance").join(&job);
        let result=owner.run(&protection,cancel,async {
            if let Some(reason)=owner.recheck(&protection,cancel).await? {return Err(super::leases::yield_error(reason));}
            check(app,request,true)?;
            let completed=engine.compact_published(&mut store,&directory,&job,&writer,&engine.capabilities,cancel,Some((&owner,&protection))).await?;
            check(app,request,true)?;
            Ok(completed)
        }).await?;
        Some(result.snapshot_id)
    } else {None};
    check(app,request,true)?;
    let last_run=super::gc_store::GcStore::open(&root)?.last_run_ms(&request.connection_id)?;
    if !super::cleanup::lww_cleanup_due(snapshot_id.is_some(),last_run,runtime::now_ms()) {
        return Ok(Some(serde_json::json!({"snapshotId":snapshot_id,"cleanup":null})));
    }
    let connected=connection_commands::open_connected_with_cancel(app,&request.connection_id,cancel).await?;
    if connected.handle.repository_id!=engine.repository.repository_id || connected.handle.connection_identity!=engine.repository.connection_identity || connected.stored.descriptor.repository_id!=engine.library {return Err(lww_segment::corrupt())}
    let cleanup=runtime::run_connected_cleanup(app,&connected,&request.connection_id,&job,Some(engine),cancel).await?;
    check(app,request,true)?;
    Ok(Some(serde_json::json!({"snapshotId":snapshot_id,"cleanup":cleanup})))
}
#[tauri::command]
pub(crate) async fn external_lww_fence(
    app: AppHandle,
    connection_id: String,
    new_device: bool,
) -> Result<()> {
    logged("external_lww_fence", Box::pin(async move {
        let context = context(&connection_id)?;
        context
            .cancel
            .lock()
            .map_err(runtime::local_error)?
            .cancel();
        context.checkpoints.lock().await.invalidate();
        let mut session = context.session.lock().await;
        session.cancel = Cancellation::default();
        let fenced = fence(&app, &connection_id, &mut session, new_device).await;
        session.cancel.cancel();
        session.receive_pages.clear();
        session.receive_more = false;
        if let Some(engine) = session.engine.as_mut() {
            engine.invalidate_clock()
        }
        session.fresh_after = Instant::now();
        fenced
    }).await)
}
/// Asks the repository about a sent segment only when one awaits an answer,
/// so a binding change away from a repository that cannot be reached goes
/// ahead without it.
async fn fence(app: &AppHandle, id: &str, session: &mut Session, new_device: bool) -> Result<()> {
    let mut store = runtime::native_store(app)?;
    if !store
        .external_lww_unconfirmed_dispatch()
        .map_err(runtime::local_error)?
    {
        return Ok(());
    }
    if let Err(error) = connect(app, id, session).await {
        return ExternalLwwEngine::fence_outcome(error, new_device);
    }
    session
        .engine
        .as_ref()
        .ok_or_else(lww_segment::corrupt)?
        .fence_binding_change(&mut store, new_device, &session.cancel)
        .await
}
#[tauri::command]
pub(crate) async fn external_lww_resume(connection_id: String) -> Result<()> {
    logged("external_lww_resume", async move {
        let context = context(&connection_id)?;
        context.checkpoints.lock().await.invalidate();
        let mut session = context.session.lock().await;
        let cancel = Cancellation::default();
        *context.cancel.lock().map_err(runtime::local_error)? = cancel.clone();
        session.resume(cancel);
        Ok(())
    }.await)
}
pub(crate) struct NewDeviceRequest {
    request: Request,
    staging_id: String,
}
impl<'de> Deserialize<'de> for NewDeviceRequest {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Input {
            connection_id: String,
            binding_authority: risunest_sync_wire::stamp::DecimalU64,
            request_id: String,
            #[serde(default)]
            generating: Vec<MessageLocator>,
            staging_id: String,
        }
        let input = Input::deserialize(deserializer)?;
        Ok(Self {
            request: Request {
                connection_id: input.connection_id,
                header: Header {
                    binding_authority: input.binding_authority,
                    request_id: input.request_id,
                },
                generating: input.generating,
                exit_target: None,
                turn_limit: None,
                complete_available: false,
            },
            staging_id: input.staging_id,
        })
    }
}
#[tauri::command]
pub(crate) async fn external_lww_prepare_new_device(
    app: AppHandle,
    request: NewDeviceRequest,
) -> Result<NewDevicePreparation> {
    logged("external_lww_prepare_new_device", Box::pin(async move {
        let context = context(&request.request.connection_id)?;
        let mut session = context.session.lock().await;
        // Acquiring this lock settles every task using the old cancellation token.
        session.cancel = Cancellation::default();
        open(&app, &request.request.connection_id, &mut session).await?;
        let mut store = check(&app, &request.request, false)?;
        session
            .engine
            .as_ref()
            .ok_or_else(lww_segment::corrupt)?
            .prepare_new_device(
                &mut store,
                &request.request.header,
                &request.staging_id,
                &session.cancel,
            )
            .await
    }).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn stored_connection() -> super::super::connection_store::StoredConnection {
        super::super::connection_store::StoredConnection {
            id: "synthetic-sync".into(),
            config: ConnectionConfig {
                provider: "webdav".into(), profile: None,
                endpoint: "https://synthetic.invalid".into(), account_id: "synthetic".into(),
                location: BTreeMap::from([("root".into(), "sync".into())]), oauth_profile: None,
            },
            descriptor: risunest_external_storage_format::format::Descriptor::new(
                "synthetic-library".into(), Some(PublicationStrategy::Sequential)).unwrap(),
            descriptor_locator: super::super::fake::locator(),
            provider_repository_id: "synthetic-repository".into(),
            credential_ref: "synthetic-credential".into(), root_key_ref: "synthetic-key".into(),
            recovery_key_ref: "synthetic-recovery".into(),
            capabilities: super::super::fake::capabilities(false),
            created_at_ms: 1, verified_at_ms: 1, last_sync_at_ms: None,
            last_backup_at_ms: None, retention_policy: None, transfer_concurrency: None, labels: Default::default(),
        }
    }

    async fn open_fixture(
        stored: &super::super::connection_store::StoredConnection,
        opens: &std::cell::Cell<usize>,
    ) -> Result<connection_commands::ConnectedRepository> {
        opens.set(opens.get() + 1);
        let mut connected = stored.clone();
        // Provider negotiation need not return the persisted capability snapshot.
        connected.capabilities.range = !connected.capabilities.range;
        Ok(connection_commands::ConnectedRepository {
            stored: connected,
            provider: Arc::new(super::super::fake::FakeProvider::new(false)),
            handle: super::super::fake::repository(),
            dependencies: super::super::fake::loopback_dependencies(
                super::super::fake::MemoryVault::default(), runtime::now_ms()).dependencies,
            root_key: zeroize::Zeroizing::new([7; 32]),
        })
    }

    #[test]
    fn unchanged_sessions_reuse_opening_despite_completion_timestamp_churn() {
        tauri::async_runtime::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let mut stored = stored_connection();
            let opens = std::cell::Cell::new(0);
            let mut session = Session::new();
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            session.receive_pages.push_back("pending-page".into());
            for n in 2..5 {
                stored.created_at_ms = n;
                stored.verified_at_ms = n;
                stored.last_sync_at_ms = Some(n);
                stored.last_backup_at_ms = Some(n);
                stored.recovery_key_ref = format!("recovery-{n}");
                connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
                session.finish_maintenance(Ok(())).unwrap();
            }
            assert_eq!(opens.get(), 1);
            assert_eq!(session.receive_pages.front().map(String::as_str), Some("pending-page"));
        });
    }

    #[test]
    fn effective_connection_changes_reopen_receive_and_maintenance_sessions() {
        use super::super::connection_store::StoredConnection;
        let changes: &[fn(&mut StoredConnection)] = &[
            |s| s.id.push_str("-new"),
            |s| s.config.provider = "s3".into(),
            |s| s.config.profile = Some("synthetic-profile".into()),
            |s| s.config.endpoint.push_str("/new"),
            |s| s.config.account_id.push_str("-new"),
            |s| { s.config.location.insert("root".into(), "new-root".into()); },
            |s| s.config.oauth_profile = Some(OAuthProfile { project_id: "synthetic-project".into(), platform_client_ids: BTreeMap::new() }),
            |s| s.descriptor = risunest_external_storage_format::format::Descriptor::new("new-library".into(), Some(PublicationStrategy::Sequential)).unwrap(),
            |s| s.descriptor_locator.object.push_str("-new"),
            |s| s.provider_repository_id.push_str("-new"),
            |s| s.credential_ref.push_str("-renewed"),
            |s| s.root_key_ref.push_str("-unlocked"),
            |s| s.capabilities.lease_operations = !s.capabilities.lease_operations,
        ];
        tauri::async_runtime::block_on(async {
            let root = tempfile::tempdir().unwrap();
            for change in changes {
                let mut stored = stored_connection();
                let opens = std::cell::Cell::new(0);
                let mut receive = Session::new();
                let mut maintenance = Session::new();
                for session in [&mut receive, &mut maintenance] {
                    connect_with(root.path(), &stored, session, open_fixture(&stored, &opens)).await.unwrap();
                    session.receive_pages.push_back("old-page".into());
                }
                change(&mut stored);
                for session in [&mut receive, &mut maintenance] {
                    connect_with(root.path(), &stored, session, open_fixture(&stored, &opens)).await.unwrap();
                    assert!(session.receive_pages.is_empty());
                    assert!(session.engine.as_ref().unwrap().admitted_upper().is_err());
                    connect_with(root.path(), &stored, session, open_fixture(&stored, &opens)).await.unwrap();
                }
                assert_eq!(opens.get(), 4);
            }
        });
    }

    #[test]
    fn failed_reopening_and_maintenance_retry_without_stale_session_state() {
        tauri::async_runtime::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let mut stored = stored_connection();
            let opens = std::cell::Cell::new(0);
            let mut session = Session::new();
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            stored.credential_ref.push_str("-renewed");
            let failure = connect_with(root.path(), &stored, &mut session, async {
                opens.set(opens.get() + 1);
                Err(ProviderError::new(ErrorKind::Unauthorized))
            }).await;
            assert_eq!(failure.unwrap_err().kind, ErrorKind::Unauthorized);
            assert!(session.engine.is_none() && session.dependencies.is_none() && session.identity.is_none());
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            session.receive_pages.push_back("failed-page".into());
            let failure = session.finish_maintenance::<()>(Err(ProviderError::new(ErrorKind::Transient)));
            assert_eq!(failure.unwrap_err().kind, ErrorKind::Transient);
            assert!(session.receive_pages.is_empty());
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            assert_eq!(opens.get(), 4);
            let mut racing = stored.clone();
            racing.config.endpoint.push_str("/changed-during-open");
            session.reset();
            let failure = connect_with(root.path(), &stored, &mut session, open_fixture(&racing, &opens)).await;
            assert_eq!(failure.unwrap_err().kind, ErrorKind::PreconditionFailed);
            assert!(session.engine.is_none());
            connect_with(root.path(), &racing, &mut session, open_fixture(&racing, &opens)).await.unwrap();
            assert_eq!(opens.get(), 6);
        });
    }

    #[test]
    fn replacement_cancellation_and_invalid_clock_refresh_without_reopening() {
        tauri::async_runtime::block_on(async {
            let root = tempfile::tempdir().unwrap();
            let stored = stored_connection();
            let opens = std::cell::Cell::new(0);
            let mut session = Session::new();
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            session.engine.as_mut().unwrap().admission = Some(super::super::lww_engine::Admission::synthetic(runtime::now_ms()));
            let cancel = session.cancel.clone();
            session.use_cancellation(&cancel);
            assert!(session.engine.as_ref().unwrap().admitted_upper().is_ok());
            cancel.cancel();
            session.use_cancellation(&Cancellation::default());
            assert!(session.engine.as_ref().unwrap().admitted_upper().is_err());
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            let now = runtime::now_ms();
            let mut skewed = super::super::lww_engine::Admission::synthetic(now);
            skewed.set_reading_for_test(now + 5001, 0);
            session.engine.as_mut().unwrap().admission = Some(skewed);
            let cancel = session.cancel.clone();
            session.use_cancellation(&cancel);
            assert!(session.engine.as_ref().unwrap().admission.is_none());
            connect_with(root.path(), &stored, &mut session, open_fixture(&stored, &opens)).await.unwrap();
            assert_eq!(opens.get(), 1);
        });
    }

    #[test]
    fn binding_after_a_fence_takes_a_fresh_cancellation_that_a_later_fence_stops() {
        let context = context("synthetic-binding-cancellation").unwrap();
        let first = binding_cancellation(&context).unwrap();
        assert!(first.check().is_ok());
        assert!(binding_cancellation(&context).unwrap().same(&first));
        // Turning sync off cancels the connection's token and leaves it cancelled.
        context.cancel.lock().unwrap().cancel();
        assert!(first.check().is_err());
        let again = binding_cancellation(&context).unwrap();
        assert!(again.check().is_ok());
        assert!(!again.same(&first));
        context.cancel.lock().unwrap().cancel();
        assert!(again.check().is_err());
    }

    fn webdav_collection(href: &str) -> String {
        format!("<D:response><D:href>{href}</D:href><D:propstat><D:prop>\
            <D:resourcetype><D:collection/></D:resourcetype></D:prop>\
            <D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>")
    }
    fn webdav_listing(date: &str, hrefs: &[String]) -> super::super::wire_fixture::Reply {
        let body = format!("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">{}</D:multistatus>",
            hrefs.iter().map(|href| webdav_collection(href)).collect::<String>());
        super::super::wire_fixture::Reply::Http {
            status: 207,
            headers: vec![("Content-Type".into(), "application/xml".into()), ("Date".into(), date.into())],
            body: body.into_bytes(),
        }
    }

    /// The external LWW opening admits its clock from the first fresh response.
    /// WebDAV answers that listing with PROPFIND, which no HTTP cache stores.
    #[test]
    fn webdav_listing_admits_the_server_clock_and_a_skewed_server_still_reports_clock_skew() {
        for (offset_ms, skewed) in [(0, false), (20 * 60 * 1000, true)] {
            tauri::async_runtime::block_on(async move {
                let now = runtime::now_ms();
                let date = httpdate::fmt_http_date(std::time::UNIX_EPOCH + std::time::Duration::from_millis(now + offset_ms));
                let server = super::super::wire_fixture::WireServer::start(vec![
                    webdav_listing(&date, &["/synthetic/sync/".into(), "/synthetic/sync/descriptors/".into()]),
                    webdav_listing(&date, &["/synthetic/sync/segments/".into()]),
                ]);
                let test = super::super::fake::loopback_dependencies(
                    super::super::fake::MemoryVault::with("synthetic-credential", b"app-password"), now);
                let provider = super::super::providers::webdav::create(test.dependencies.clone()).unwrap();
                let mut stored = stored_connection();
                stored.config.endpoint = server.url.as_str().into();
                let cancel = Cancellation::default();
                let mut session = Session::new();
                let (handle, _) = provider.open_repository(&stored.config,
                    &SecretRef(stored.credential_ref.clone()), OpenMode::Existing, &cancel).await.unwrap();
                let root = tempfile::tempdir().unwrap();
                let connected = connection_commands::ConnectedRepository {
                    stored: stored.clone(), provider, handle,
                    dependencies: test.dependencies.clone(), root_key: zeroize::Zeroizing::new([7; 32]),
                };
                connect_with(root.path(), &stored, &mut session, async { Ok(connected) }).await.unwrap();
                // A later response replaces the opening's own sample, so the
                // admission has to come from the listing it requests itself.
                session.fresh_after = Instant::now();
                let admitted = admit_clock(&mut session).await;
                if skewed {
                    assert_eq!(admitted.unwrap_err().kind, ErrorKind::ClockSkew);
                    assert!(session.engine.as_ref().unwrap().admission.is_none());
                } else {
                    admitted.unwrap();
                    assert!(session.engine.as_ref().unwrap().admission.is_some());
                }
                let requests = server.requests.lock().unwrap();
                assert_eq!(requests.len(), 2);
                assert!(requests[1].headers.starts_with("PROPFIND /synthetic/sync/segments/ "), "{}", requests[1].headers.lines().next().unwrap_or_default());
            });
        }
    }

    #[test]
    fn only_a_publication_that_sent_segments_or_a_finished_receive_page_records_the_sync_time() {
        use super::super::connection_store::ConnectionStore;
        use crate::persistent_store::{lww::{ApplyReceive, UnitMutation}, WorkingSetCommit};
        use risunest_sync_wire::{stamp::DecimalU64, unit::UnitKey};
        tauri::async_runtime::block_on(async {
            let mut f = super::super::lww_tests::CycleFixture::new();
            let receiver = super::super::previous_storage_tests::receiver_connection(&f).stored;
            let mut sender = receiver.clone();
            sender.id = "sender".into();
            ConnectionStore::open(f.directory_a.path()).unwrap().insert(&sender).unwrap();
            ConnectionStore::open(f.directory_b.path()).unwrap().insert(&receiver).unwrap();
            let last_sync = |root: &std::path::Path, id: &str| ConnectionStore::open(root).unwrap().read(id).unwrap().last_sync_at_ms;
            let mut sender = Session::new();
            sender.engine = Some(f.sender);
            let mut receiver = Session::new();
            receiver.engine = Some(f.receiver);
            assert_eq!(publish_round(&mut sender, &mut f.a, DecimalU64(0), &[], None).await.unwrap().segments.0, 0);
            assert_eq!(last_sync(f.directory_a.path(), "sender"), None);
            f.a.commit(&WorkingSetCommit {
                expected_revision: f.a.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: UnitKey::new(&["root", "language"]).unwrap(), value: serde_json::json!("ko"),
                }]),
                ..Default::default()
            }).unwrap();
            assert_eq!(publish_round(&mut sender, &mut f.a, DecimalU64(0), &[], None).await.unwrap().segments.0, 1);
            assert!(last_sync(f.directory_a.path(), "sender").is_some());

            let checkpoints = Default::default();
            let (page, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), false).await.unwrap();
            let page = page.expect("the published segment is offered");
            assert!(!recorded);
            assert_eq!(last_sync(f.directory_b.path(), "receiver"), None);
            let (unfinished, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), false).await.unwrap();
            assert_eq!(unfinished.map(|page| page.header.request_id), Some(page.header.request_id.clone()));
            assert!(!recorded);
            f.b.lww_stage_receive(&page).unwrap();
            f.b.lww_apply_receive(&ApplyReceive { header: page.header.clone(), generating: Vec::new() }).unwrap();
            f.b.lww_finish_receive(&page.header).unwrap();
            f.a.commit(&WorkingSetCommit {
                expected_revision: f.a.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: UnitKey::new(&["root", "language"]).unwrap(), value: serde_json::json!("en"),
                }]),
                ..Default::default()
            }).unwrap();
            publish_round(&mut sender, &mut f.a, DecimalU64(0), &[], None).await.unwrap();
            let (next, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), false).await.unwrap();
            assert!(next.is_none());
            assert!(recorded);
            let recorded_at = last_sync(f.directory_b.path(), "receiver").expect("a finished page records the sync time");
            let (following, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), false).await.unwrap();
            assert!(following.is_some(), "the next round receives arrivals after the captured listing");
            assert!(!recorded);
            assert_eq!(last_sync(f.directory_b.path(), "receiver"), Some(recorded_at));

            let summary = serde_json::to_value(super::super::connection::summary(
                &ConnectionStore::open(f.directory_b.path()).unwrap().read("receiver").unwrap())).unwrap();
            assert_eq!(summary["lastSyncAtMs"], serde_json::json!(recorded_at.to_string()));

            let following = following.unwrap();
            f.b.lww_stage_receive(&following).unwrap();
            f.b.lww_apply_receive(&ApplyReceive { header: following.header.clone(), generating: Vec::new() }).unwrap();
            f.b.lww_finish_receive(&following.header).unwrap();
            f.a.commit(&WorkingSetCommit {
                expected_revision: f.a.revision().unwrap(),
                unit_mutations: Some(vec![UnitMutation::Set {
                    key: UnitKey::new(&["root", "language"]).unwrap(), value: serde_json::json!("ja"),
                }]),
                ..Default::default()
            }).unwrap();
            publish_round(&mut sender, &mut f.a, DecimalU64(0), &[], None).await.unwrap();
            let (latest, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), true).await.unwrap();
            let latest = latest.expect("a complete receive lists again once the listed pages are finished");
            assert!(recorded);
            f.b.lww_stage_receive(&latest).unwrap();
            f.b.lww_apply_receive(&ApplyReceive { header: latest.header.clone(), generating: Vec::new() }).unwrap();
            f.b.lww_finish_receive(&latest.header).unwrap();
            let (exhausted, recorded) = receive_page(&mut f.b, &mut receiver, &checkpoints, DecimalU64(0), true).await.unwrap();
            assert!(exhausted.is_none());
            assert!(recorded);
        });
    }

    #[test]
    fn a_receive_lists_the_next_batch_at_once_while_the_listing_holds_more() {
        use super::super::connection_store::ConnectionStore;
        use crate::persistent_store::{lww::{ApplyReceive, UnitMutation}, WorkingSetCommit};
        use risunest_sync_wire::{stamp::DecimalU64, unit::UnitKey};
        tauri::async_runtime::block_on(async {
            let mut f = super::super::lww_tests::CycleFixture::new();
            let receiver = super::super::previous_storage_tests::receiver_connection(&f).stored;
            ConnectionStore::open(f.directory_b.path()).unwrap().insert(&receiver).unwrap();
            let last_sync = |root: &std::path::Path| ConnectionStore::open(root).unwrap().read("receiver").unwrap().last_sync_at_ms;
            for index in 0..6 {
                f.a.commit(&WorkingSetCommit {
                    expected_revision: f.a.revision().unwrap(),
                    unit_mutations: Some(vec![UnitMutation::Set {
                        key: UnitKey::new(&["root", "language"]).unwrap(), value: serde_json::json!(format!("synthetic-{index}")),
                    }]),
                    ..Default::default()
                }).unwrap();
                f.publish_a().await;
            }
            let mut session = Session::new();
            session.engine = Some(f.receiver);
            let checkpoints = Default::default();
            // Six segments take two listings of at most four; the second follows the first without a round in between.
            for _ in 0..6 {
                let (page, recorded) = receive_page(&mut f.b, &mut session, &checkpoints, DecimalU64(0), false).await.unwrap();
                let page = page.expect("pages continue while the listing holds more segments");
                assert!(!recorded);
                assert_eq!(last_sync(f.directory_b.path()), None);
                f.b.lww_stage_receive(&page).unwrap();
                f.b.lww_apply_receive(&ApplyReceive { header: page.header.clone(), generating: Vec::new() }).unwrap();
                f.b.lww_finish_receive(&page.header).unwrap();
            }
            let (done, recorded) = receive_page(&mut f.b, &mut session, &checkpoints, DecimalU64(0), false).await.unwrap();
            assert!(done.is_none());
            assert!(recorded, "finishing the last listed segment records the sync time");
            assert!(last_sync(f.directory_b.path()).is_some());
            assert_eq!(f.b.read_root(None).unwrap().value["language"], "synthetic-5");
        });
    }

    #[test]
    fn external_commands_log_their_own_failures() {
        use crate::external_storage::contract::{ConnectionConfig, ErrorKind};
        let resumed = tauri::async_runtime::block_on(external_lww_resume(String::new()));
        assert_eq!(resumed.unwrap_err().kind, ErrorKind::Corrupt);
        let config = ConnectionConfig {
            provider: "synthetic-provider".into(),
            profile: None,
            endpoint: String::new(),
            account_id: String::new(),
            location: Default::default(),
            oauth_profile: None,
        };
        let validated = super::super::connection_commands::external_storage_validate_sync_root(config);
        assert_eq!(validated.unwrap_err().kind, ErrorKind::Unsupported);
        for (command, code, file) in [
            ("external_lww_resume", "corrupt", "lww_commands.rs:"),
            ("external_storage_validate_sync_root", "unsupported", "connection_commands.rs:"),
        ] {
            let entry = crate::native_log::global_state()
                .tail(None)
                .into_iter()
                .rev()
                .find(|entry| entry.message.starts_with(&format!("{command} failed: code={code} at=")))
                .unwrap_or_else(|| panic!("{command} logs its failure"));
            assert_eq!((entry.level.as_str(), entry.target.as_str()), ("error", "native-command"));
            assert!(entry.message.contains(file), "{}", entry.message);
        }
    }
    #[test]
    fn renderer_command_envelopes_deserialize_and_reject_unknown_fields() {
        let base = serde_json::json!({"connectionId":"sync","bindingAuthority":"4","requestId":"request","generating":[]});
        assert!(serde_json::from_value::<Request>(base.clone()).is_ok());
        let mut stage = base.clone();
        stage["inspectionId"] = serde_json::json!("inspection");
        stage["targetId"] = serde_json::json!("target");
        stage["libraryId"] = serde_json::json!("library");
        assert!(serde_json::from_value::<StageRequest>(stage.clone()).is_ok());
        let mut invalid_stage = stage.clone();
        invalid_stage["inspectionId"] = serde_json::json!(4);
        assert!(serde_json::from_value::<StageRequest>(invalid_stage).is_err());
        stage["rendererSettled"] = serde_json::json!(true);
        assert!(serde_json::from_value::<StageRequest>(stage).is_err());
        let mut prepare = base.clone();
        prepare["stagingId"] = serde_json::json!("stage");
        assert!(serde_json::from_value::<NewDeviceRequest>(prepare.clone()).is_ok());
        let mut invalid_prepare = prepare.clone();
        invalid_prepare["bindingAuthority"] = serde_json::json!(4);
        assert!(serde_json::from_value::<NewDeviceRequest>(invalid_prepare).is_err());
        prepare["rendererSettled"] = serde_json::json!(true);
        assert!(serde_json::from_value::<NewDeviceRequest>(prepare).is_err());
        let mut exit = base.clone();
        exit["exitTarget"] = serde_json::json!({"revision":"12","libraryEpoch":"epoch","selectionEpoch":"selection"});
        assert!(serde_json::from_value::<Request>(exit).is_ok());
        for (field, value) in [
            ("bindingAuthority", serde_json::json!(4)),
            ("generating", serde_json::json!("conversation")),
            ("requestId", serde_json::json!(4)),
        ] {
            let mut invalid = base.clone();
            invalid[field] = value;
            assert!(
                serde_json::from_value::<Request>(invalid).is_err(),
                "{field}"
            );
        }
        let mut invalid = base;
        invalid["rendererSettled"] = serde_json::json!(true);
        assert!(serde_json::from_value::<Request>(invalid).is_err());
    }
}

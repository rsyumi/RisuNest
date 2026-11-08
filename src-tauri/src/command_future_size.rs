//! Tauri hands an async command's future by value to the IPC resolver on the
//! thread that received the request, which is the 1 MiB main thread on Windows
//! and iOS. Release builds copy it through several frames there before tokio
//! moves it to the heap: those frames measured 23 to 62 times the size this
//! test build reports. A command whose body is larger than `MAX_FUTURE_BYTES`
//! awaits it through `Box::pin`.

use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    path::Path,
};

const MAX_FUTURE_BYTES: usize = 4 * 1024;

trait AsyncCommand<Args> {
    fn future_bytes(&self) -> usize;
}

macro_rules! async_command_arity {
    ($($arg:ident),*) => {
        impl<F, R, $($arg,)*> AsyncCommand<($($arg,)*)> for F
        where
            F: Fn($($arg),*) -> R,
            R: Future,
        {
            fn future_bytes(&self) -> usize {
                std::mem::size_of::<R>()
            }
        }
    };
}
async_command_arity!();
async_command_arity!(A);
async_command_arity!(A, B);
async_command_arity!(A, B, C);
async_command_arity!(A, B, C, D);
async_command_arity!(A, B, C, D, E);
async_command_arity!(A, B, C, D, E, G);
async_command_arity!(A, B, C, D, E, G, H);
async_command_arity!(A, B, C, D, E, G, H, I);
async_command_arity!(A, B, C, D, E, G, H, I, J);

fn future_bytes<Args>(command: impl AsyncCommand<Args>) -> usize {
    command.future_bytes()
}

macro_rules! async_commands {
    ($($(#[$meta:meta])* $($segment:ident)::+),* $(,)?) => {
        /// Every async command in the handler, including other platforms' ones.
        const LISTED: &[&str] = &[$(stringify!($($segment)::+)),*];

        fn measured() -> Vec<(&'static str, usize)> {
            vec![$($(#[$meta])* (stringify!($($segment)::+), future_bytes(crate::$($segment)::+)),)*]
        }
    };
}

async_commands![
    diagnostics::diagnostic_device_info,
    network_log::network_log_command,
    native_log::file_page::native_log_file_read,
    native_log::file_export::native_log_file_export,
    app_paths::app_paths_roots,
    app_lock::app_lock_check,
    app_lock::app_lock_authenticate,
    app_lock::app_lock_set_policy,
    app_lock::app_lock_ready,
    app_lock_record::app_lock_read_record,
    app_lock_record::app_lock_write_record,
    app_cleanup::app_cleanup_request,
    app_cleanup::app_cleanup_resume,
    app_cleanup::app_cleanup_cancel,
    external_storage::connection_commands::external_storage_unlock_connection,
    external_storage::connection_commands::external_storage_commit_connection,
    external_storage::connection_commands::external_storage_begin_authorization,
    external_storage::connection_commands::external_storage_complete_authorization,
    external_storage::connection_commands::external_storage_list_folders,
    external_storage::connection_commands::external_storage_select_folder,
    external_storage::connection_commands::external_storage_change_endpoint,
    external_storage::connection_commands::external_storage_remove_connection,
    external_storage::connection_commands::external_storage_begin_connection_settings_export,
    external_storage::connection_commands::external_storage_save_connection_settings_file,
    external_storage::lww_commands::external_lww_inspect,
    external_storage::lww_commands::external_lww_stage_binding,
    external_storage::lww_commands::external_lww_publish,
    external_storage::lww_commands::external_lww_check_format,
    external_storage::lww_commands::external_lww_switch_format,
    external_storage::lww_commands::external_lww_receive,
    external_storage::lww_commands::external_lww_maintenance,
    external_storage::lww_commands::external_lww_fence,
    external_storage::lww_commands::external_lww_resume,
    external_storage::lww_commands::external_lww_prepare_new_device,
    external_storage::runtime::external_storage_set_sync_target,
    external_storage::runtime::external_storage_start_job,
    external_storage::runtime::external_storage_cancel_job,
    external_storage::runtime::external_storage_stop_restore,
    external_storage::runtime::external_storage_get_quota,
    external_storage::history::external_storage_list_history,
    external_storage::history_deletion::external_storage_prepare_history_delete,
    external_storage::snapshot_export_commands::external_storage_export_snapshot,
    #[cfg(target_os = "macos")]
    macos_lifecycle::macos_exit_response,
    native_request,
    app_update::commands::app_update_check,
    app_update::commands::app_update_install,
    app_update::commands::app_update_stage_deb,
    native_media::streaming::native_media_ensure,
    server_sync::commands::server_sync_asset_status,
    server_sync::commands::asset_residency_connection_objects,
    server_sync::commands::server_sync_status,
    server_sync::commands::server_sync_configure,
    server_sync::commands::server_sync_lww_push,
    server_sync::commands::server_sync_lww_pull,
    server_sync::commands::server_sync_lww_ack,
    server_sync::commands::server_sync_lww_fence,
    server_sync::commands::server_sync_lww_activate,
    server_sync::commands::server_sync_lww_hydrate,
    server_sync::commands::server_sync_lww_pending_count,
    server_sync::commands::server_sync_progress,
    server_sync::commands::server_sync_lww_inspect,
    server_sync::commands::server_sync_lww_pending_binding,
    server_sync::commands::server_sync_lww_stage_target,
    server_sync::commands::server_sync_lww_prepare_new_device,
    server_sync::commands::server_sync_lww_prepare_fresh_writer,
    server_sync::commands::server_sync_lww_activate_new_device,
    server_sync::commands::server_sync_lww_retry,
    server_sync::commands::server_sync_lww_check_format,
    server_sync::commands::server_sync_lww_switch_format,
    server_sync::commands::server_sync_lww_drain,
    server_sync::commands::server_sync_cancel,
    server_sync::commands::server_sync_notify_start,
    server_sync::commands::server_sync_notify_stop,
    server_sync::commands::server_sync_asset_policy,
    server_sync::commands::asset_residency_download_remote,
    server_sync::commands::server_sync_asset_evict,
    server_sync::commands::server_sync_cache_usage,
    server_sync::commands::server_sync_cache_cleanup,
    #[cfg(desktop)]
    appimage_integration::appimage_integration_state,
    #[cfg(desktop)]
    appimage_integration::appimage_integration_register,
    native_tokenizer::tokenize_batch,
    native_media::native_media_encode_inlay_image,
    native_media::ipc::native_media_encode_canvas_webp,
    native_media::ipc::native_media_encode_canvas_webp_finish,
    native_media::ipc::native_media_inlay_input_open,
    native_media::ipc::native_media_inlay_input_chunk,
    native_media::ipc::native_media_inlay_input_chunk_raw,
    native_media::ipc::native_media_encode_inlay_finish,
    native_media::ipc::native_media_inlay_output_read,
    asset_repository::commands::asset_cas_read_bytes,
    asset_repository::commands::asset_cas_stat_object,
    asset_repository::commands::asset_cas_stat_objects,
    asset_repository::commands::asset_remote_stat_objects,
    asset_repository::commands::asset_remote_stat_object,
    asset_repository::commands::asset_remote_hydrate_object,
    asset_repository::commands::asset_cas_job_begin,
    asset_repository::commands::asset_cas_job_prepare,
    asset_repository::commands::asset_cas_job_upload_open,
    asset_repository::commands::asset_cas_job_upload_chunk,
    asset_repository::commands::asset_cas_job_upload_chunk_raw,
    asset_repository::commands::asset_cas_job_upload_finish,
    asset_repository::commands::asset_cas_job_upload_cancel,
    asset_repository::commands::asset_cas_job_pin_existing,
    asset_repository::commands::asset_cas_job_seal,
    asset_repository::commands::asset_cas_job_finalize_content,
    asset_repository::commands::asset_cas_job_seal_prepared_content,
    asset_repository::commands::asset_cas_job_release,
    native_file_jobs::native_content_source_metadata,
    native_file_jobs::native_file_job_prepared_content,
    native_file_jobs::native_file_job_stage_inline_asset,
    native_file_jobs::screenshot_output::native_file_job_screenshot_output_publish,
    persistent_store::commands::gallery::pds_gallery_page,
    persistent_store::commands::gallery::pds_gallery_keys,
    persistent_store::commands::gallery::pds_gallery_index,
    persistent_store::commands::gallery::pds_gallery_delete,
    persistent_store::commands::gallery::pds_gallery_missing_keys,
    persistent_store::commands::gallery::pds_gallery_scan_start,
    persistent_store::commands::gallery::pds_gallery_scan_status,
    persistent_store::commands::gallery::pds_gallery_scan_result,
    persistent_store::commands::gallery::pds_gallery_scan_cancel,
    persistent_store::commands::gallery::pds_gallery_locations,
    persistent_store::commands::gallery::pds_gallery_usage_count,
    persistent_store::commands::pds_archive_character,
    persistent_store::commands::pds_restore_character,
    persistent_store::commands::image_geometry::pds_read_image_geometry,
    persistent_store::commands::image_geometry::pds_write_image_geometry,
    persistent_store::commands::image_geometry::pds_compute_image_geometry,
    #[cfg(target_os = "android")]
    android_commit_transport::pds_commit_android_finish,
    #[cfg(target_os = "android")]
    android_commit_transport::pds_replace_android_finish,
    #[cfg(target_os = "android")]
    android_commit_transport::pds_plugin_part_android_finish,
    #[cfg(target_os = "android")]
    android_commit_transport::pds_plugin_write_android_finish,
    #[cfg(windows)]
    persistent_commit_transport::pds_commit_shared_open,
    #[cfg(windows)]
    persistent_commit_transport::pds_commit_shared_chunk,
    #[cfg(windows)]
    persistent_commit_transport::pds_commit_shared_finish,
    #[cfg(windows)]
    persistent_commit_transport::pds_commit_shared_cancel,
    #[cfg(feature = "official-publication-upload-pilot")]
    persistent_store::commands::official_publication_upload_file,
    #[cfg(feature = "native-kei-upload-pilot")]
    persistent_store::commands::pds_kei_backup_upload,
    regex_shadow::regex_execute_batch,
];

fn compact(path: &str) -> String {
    path.split_whitespace().collect()
}

#[test]
fn async_command_futures_stay_small() {
    let mut sizes = measured();
    sizes.sort_by(|a, b| b.1.cmp(&a.1));
    let table = sizes
        .iter()
        .map(|(path, bytes)| format!("{bytes:>9} {}", compact(path)))
        .collect::<Vec<_>>()
        .join("\n");
    let oversized: Vec<_> = sizes
        .iter()
        .filter(|(_, bytes)| *bytes > MAX_FUTURE_BYTES)
        .map(|(path, _)| compact(path))
        .collect();
    assert!(
        oversized.is_empty(),
        "await these command bodies through Box::pin: {oversized:?}\n{table}"
    );
}

/// Handler entries without their attributes, as written in `lib.rs`.
fn handler_entries() -> Vec<String> {
    // Built from pieces so the harness command scan does not read this line as the macro.
    const OPENING: &str = concat!("tauri::generate_handler", "![");
    let source = include_str!("lib.rs");
    let start = source.find(OPENING).expect("handler list") + OPENING.len();
    let end = start + source[start..].find("]);").expect("handler list end");
    let mut text = String::new();
    let mut depth = 0usize;
    let mut chars = source[start..end].chars().peekable();
    while let Some(c) = chars.next() {
        if depth == 0 && c == '#' && chars.peek() == Some(&'[') {
            chars.next();
            depth = 1;
        } else if depth > 0 {
            match c {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        } else {
            text.push(c);
        }
    }
    text.split(',').map(compact).filter(|entry| !entry.is_empty()).collect()
}

/// Names of the `#[tauri::command]` functions under `src`, each with whether
/// any declaration of it is async.
fn command_declarations() -> BTreeMap<String, bool> {
    fn visit(dir: &Path, found: &mut BTreeMap<String, bool>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, found);
                continue;
            }
            if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                if !line.trim_start().starts_with("#[tauri::command") {
                    continue;
                }
                let Some(declaration) = lines[index + 1..].iter().take(6).find(|line| line.contains("fn ")) else {
                    continue;
                };
                let after = &declaration[declaration.find("fn ").unwrap() + 3..];
                let name: String = after.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
                *found.entry(name).or_default() |= declaration.contains("async fn");
            }
        }
    }
    let mut found = BTreeMap::new();
    visit(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut found);
    found
}

#[test]
fn every_async_command_in_the_handler_is_measured() {
    let listed: BTreeSet<String> = LISTED.iter().map(|path| compact(path)).collect();
    let declarations = command_declarations();
    let async_entries: BTreeSet<String> = handler_entries()
        .into_iter()
        .filter(|entry| {
            let name = entry.rsplit("::").next().unwrap();
            *declarations.get(name).unwrap_or_else(|| panic!("no command declaration for {entry}"))
        })
        .collect();
    assert!(async_entries.len() > 50, "handler parsing found {} async commands", async_entries.len());
    let unlisted: Vec<_> = async_entries.difference(&listed).collect();
    let stale: Vec<_> = listed.difference(&async_entries).collect();
    assert!(unlisted.is_empty() && stale.is_empty(), "unlisted: {unlisted:?}, not async handler entries: {stale:?}");
}

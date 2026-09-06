#![cfg(target_os = "android")]

#[used]
static FORCE_RISUAI_LIBRARY_LINK: fn() = risuai_lib::run;

unsafe extern "system" {
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_resume();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_pause();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_setForegroundAllowed();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_requestCancel();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_cancelAndCleanup();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_cleanupCompleted();
    fn Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_attach();
    fn Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_cancel();
    fn Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_detach();
}

#[test]
fn android_peer_clone_library_compiles_without_desktop_host_modules() {
    std::hint::black_box(risuai_lib::run as fn());
}

#[test]
fn android_peer_clone_jni_symbols_are_linked() {
    let symbols = [
        Java_co_aiclient_risu_PeerCloneNativeBridge_resume as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_pause as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_setForegroundAllowed as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_requestCancel as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_cancelAndCleanup as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_cleanupCompleted as *const (),
        Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_attach as *const (),
        Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_cancel as *const (),
        Java_co_aiclient_risu_PeerSyncForegroundNativeBridge_detach as *const (),
    ];

    assert!(symbols.iter().all(|symbol| !symbol.is_null()));
}

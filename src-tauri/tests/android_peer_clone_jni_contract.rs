#![cfg(target_os = "android")]

unsafe extern "system" {
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_resume();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_pause();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_cancelAndCleanup();
    fn Java_co_aiclient_risu_PeerCloneNativeBridge_cleanupCompleted();
}

#[test]
fn android_peer_clone_jni_symbols_are_linked() {
    let symbols = [
        Java_co_aiclient_risu_PeerCloneNativeBridge_resume as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_pause as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_cancelAndCleanup as *const (),
        Java_co_aiclient_risu_PeerCloneNativeBridge_cleanupCompleted as *const (),
    ];

    assert!(symbols.iter().all(|symbol| !symbol.is_null()));
}

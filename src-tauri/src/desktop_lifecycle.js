window.RisuLifecycleBridge = {
    onFlushComplete(token) {
        void window.__TAURI_INTERNALS__.invoke('desktop_flush_complete', { token })
            .catch(() => {})
    },
};

// Older WebKit normalizes string POST bodies to NFC. Preserve the exact JSON
// bytes across Tauri IPC, including decomposed text in storage and tokenization.
// Install with Tauri's main-frame invoke initialization hook. Authentication,
// capability checks, serialization and response handling remain owned by Tauri.
(() => {
  const originalFetch = window.fetch.bind(window);
  window.fetch = (input, init) => {
    if (
      typeof input === "string" &&
      input.startsWith("ipc://localhost/") &&
      typeof init?.body === "string"
    ) {
      return originalFetch(input, {
        ...init,
        body: new TextEncoder().encode(init.body),
      });
    }
    return originalFetch(input, init);
  };
})();

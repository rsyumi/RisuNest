# Local Android scanner patch

Source: tauri-apps/plugins-workspace, crates.io tauri-plugin-barcode-scanner 2.4.6, MIT/Apache-2.0. Rust API and permissions remain upstream.

Android changes: finish pending invokes before destroying state; invalidate late camera/model callbacks by scan generation; remove preview even before provider readiness; close ML Kit scanner; report bounded camera errors; use the bundled barcode model for first-use offline scanning; make camera hardware optional. These address observed upstream cancel/destroy ordering and asynchronous setup paths.

Keep this patch narrow when updating the plugin. JS bindings remain the official 2.4.6 package.

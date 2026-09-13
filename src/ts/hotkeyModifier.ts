import { platform } from '@tauri-apps/plugin-os'

/** Keep existing Ctrl bindings usable and add the native Mac Command equivalent. */
export function shortcutModifier(
    event: Pick<KeyboardEvent, 'ctrlKey' | 'metaKey'>,
    os = (globalThis as typeof globalThis & { __TAURI_INTERNALS__?: unknown })
        .__TAURI_INTERNALS__
        ? platform()
        : undefined,
): boolean {
    return event.ctrlKey || (os === 'macos' && event.metaKey)
}

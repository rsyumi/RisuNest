import { readFile } from '@tauri-apps/plugin-fs'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { alertError } from './alert'
import { isTauriDesktop, isTauriIOS } from 'src/ts/platform'
import { discardIOSFile } from './storage/iosFiles'

/**
 * Name shared by the DOM event Android dispatches on a warm start and by the Tauri event the
 * desktop single instance callback emits. The Android event carries `detail.files`, while the
 * desktop event only announces that `opened_files_take` has something to hand over.
 */
export const OPENED_FILES_EVENT = 'risu-opened-files'

/** Desktop command that returns the pending opened files and clears them in the same call. */
export const OPENED_FILES_TAKE_COMMAND = 'opened_files_take'

export type OpenedFileImporter = (name: string, data: Uint8Array) => Promise<void>

let openedPathImporter: ((path: string) => Promise<boolean>) | undefined
let openedFileImporter: OpenedFileImporter | null = null
let domListener: ((event: Event) => void) | null = null
let queue: Promise<void> = Promise.resolve()

/**
 * Reads and imports every opened file in order. A failing file is reported and skipped so the
 * remaining files still arrive.
 */
export async function consumeOpenedFiles(files: string[]): Promise<void> {
    const importer = openedFileImporter
    if (!importer || !Array.isArray(files)) {
        return
    }
    const run = queue.then(async () => {
        for (const file of files) {
            if (typeof file !== 'string' || file.length === 0) {
                continue
            }
            try {
                if (await openedPathImporter?.(file)) continue
                const data = await readFile(file)
                await importer(file, data)
            } catch (error) {
                alertError(
                    `Failed to open the selected file: ${file}\n${error}`,
                )
            } finally {
                if (isTauriIOS) await discardIOSFile(file).catch(() => {
                    alertError('The imported temporary file could not be removed.')
                })
            }
        }
    })
    queue = run.catch(() => {})
    await run
}

/**
 * Wires every path a file association can take: the Android cold start injection, the Android warm
 * start DOM event, and the desktop launch arguments plus single instance forwarding.
 */
export function registerOpenedFileListeners(
    importFile: OpenedFileImporter,
    importPath?: (path: string) => Promise<boolean>,
): void {
    openedFileImporter = importFile
    openedPathImporter = importPath
    if (domListener) {
        return
    }

    const injected = takeInjectedOpenedFiles()
    if (injected.length > 0) {
        void consumeOpenedFiles(injected)
    }

    domListener = (event: Event) => {
        const detail = (event as CustomEvent<{ files?: unknown }>).detail
        const files = detail?.files
        if (Array.isArray(files) && files.length > 0) {
            // Android watches the cancelation to tell a delivered payload from one that arrived
            // before this listener existed, which it then parks in the startup queue instead.
            event.preventDefault()
            void consumeOpenedFiles(files as string[])
        }
    }
    window.addEventListener(OPENED_FILES_EVENT, domListener)

    if (isTauriIOS) {
        window.addEventListener('risunest-ios-opened-files', () => void drainIOSOpenedFiles())
        void drainIOSOpenedFiles()
    }

    if (isTauriDesktop) {
        void listen(OPENED_FILES_EVENT, () => {
            void drainDesktopOpenedFiles()
        })
            .then(() => {
                // Subscribe before draining so a launch during setup cannot lose its notification.
                void drainDesktopOpenedFiles()
            })
            .catch((error) => {
                console.warn('Failed to subscribe to opened files:', error)
                void drainDesktopOpenedFiles()
            })
    }
}

async function drainIOSOpenedFiles(): Promise<void> {
    try {
        const { files } = await invoke<{ files: Array<{ path?: string; error?: string }> }>(
            'plugin:ios-native|take_opened_files',
        )
        for (const file of files) {
            if (file.error) alertError(file.error)

        }
        await consumeOpenedFiles(files.flatMap(file => file.path ? [file.path] : []))
    } catch (error) {
        alertError(`Failed to receive the selected file: ${error}`)
    }
}

function takeInjectedOpenedFiles(): string[] {
    const holder = window as Window & { tauriOpenedFiles?: unknown }
    const injected = holder.tauriOpenedFiles
    if (!Array.isArray(injected)) {
        return []
    }
    // The Android cold start injection runs once per page load, so drop it after draining.
    delete holder.tauriOpenedFiles
    return injected as string[]
}

async function drainDesktopOpenedFiles(): Promise<void> {
    try {
        const files = await invoke<string[]>(OPENED_FILES_TAKE_COMMAND)
        if (Array.isArray(files) && files.length > 0) {
            await consumeOpenedFiles(files)
        }
    } catch (error) {
        console.warn('Failed to read the opened files handed over by the launcher:', error)
    }
}

import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { alertError } from './alert'
import { isTauriAndroid, isTauriDesktop, isTauriIOS } from 'src/ts/platform'
import { discardIOSFile } from './storage/iosFiles'
import type { ImportIntakeSource } from './importIntake/types'

/**
 * Name shared by the DOM event Android dispatches on a warm start and by the Tauri event the
 * desktop single instance callback emits. The Android event carries `detail.files`, while the
 * desktop event only announces that `opened_files_take` has something to hand over.
 */
export const OPENED_FILES_EVENT = 'risu-opened-files'

/** Desktop command that returns the pending opened files and clears them in the same call. */
export const OPENED_FILES_TAKE_COMMAND = 'opened_files_take'

/** A file one opening handed over: the path of its prepared copy, or a file that could not be prepared. */
export type OpenedFile = string | { name: string; code: string }

/** Hands the files of one opening over to the intake, which reports how each of them ended. */
export type OpenedFilesDelivery = (files: OpenedFile[]) => Promise<void>

let openedFilesDelivery: OpenedFilesDelivery | null = null
let domListener: ((event: Event) => void) | null = null

function isOpenedFile(value: unknown): value is OpenedFile {
    if (typeof value === 'string') return value.length > 0
    return value !== null && typeof value === 'object'
        && typeof (value as { name?: unknown }).name === 'string' && typeof (value as { code?: unknown }).code === 'string'
}

/** The intake source of an opened file; one that could not be prepared is refused there. */
export function openedFileSource(file: OpenedFile): ImportIntakeSource {
    return typeof file === 'string' ? { type: 'path', path: file } : { type: 'failed', ...file }
}

/**
 * Delivers the files of one opening together as soon as they arrive, so the intake queues them in
 * arrival order with dropped files. Each file is settled once the delivery has ended, also when it failed.
 */
export async function consumeOpenedFiles(files: unknown[]): Promise<void> {
    const deliver = openedFilesDelivery
    if (!deliver || !Array.isArray(files)) return
    const opened = files.filter(isOpenedFile)
    if (!opened.length) return
    await settleOpenedFilesAfter(opened, () => deliver(opened))
}

/**
 * Runs the delivery that holds these opened files, then settles each prepared copy once it has
 * ended, also when it failed. An Android share whose other files were spooled delivers its opened
 * files with that spool batch, and they are settled here as well.
 */
export async function settleOpenedFilesAfter(files: OpenedFile[], delivery: () => Promise<void>): Promise<void> {
    try {
        await delivery()
    } catch (error) {
        const names = files.map(file => typeof file === 'string' ? file : file.name)
        alertError(`Failed to open the selected files: ${names.join(', ')}\n${error}`)
    } finally {
        for (const file of files) if (typeof file === 'string') await settleOpenedFile(file)
    }
}

/**
 * Hands opened files over as the Android opened-files event does: to the listener when one is
 * registered, otherwise to the startup queue it drains.
 */
export function handOverOpenedFiles(files: OpenedFile[]): void {
    const event = new CustomEvent(OPENED_FILES_EVENT, { detail: { files }, cancelable: true })
    if (!window.dispatchEvent(event)) return
    const holder = window as Window & { tauriOpenedFiles?: unknown }
    holder.tauriOpenedFiles = (Array.isArray(holder.tauriOpenedFiles) ? holder.tauriOpenedFiles : []).concat(files)
}

async function settleOpenedFile(file: string): Promise<void> {
    // Every outcome has been reported by now, so Android must not offer the file again.
    if (isTauriAndroid) {
        try {
            window.RisuLifecycleBridge?.acknowledgeOpenedFile?.(file)
        } catch (error) {
            console.warn('Failed to acknowledge the opened file:', error)
        }
    }
    if (isTauriIOS) await discardIOSFile(file).catch(() => {
        alertError('The imported temporary file could not be removed.')
    })
}

/**
 * Wires every path a file association can take: the Android cold start injection, the Android warm
 * start DOM event, and the desktop launch arguments plus single instance forwarding.
 */
export function registerOpenedFileListeners(deliver: OpenedFilesDelivery): void {
    openedFilesDelivery = deliver
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
            void consumeOpenedFiles(files)
        }
    }
    window.addEventListener(OPENED_FILES_EVENT, domListener)

    if (isTauriIOS) {
        window.addEventListener('risunest-ios-opened-files', () => void drainIOSOpenedFiles())
        void drainIOSOpenedFiles()
    }

    if (isTauriDesktop) {
        void listen(OPENED_FILES_EVENT, scheduleDesktopDrain)
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
        const { files } = await invoke<{ files: Array<{ path?: string; name?: string; code?: string }> }>(
            'plugin:ios-native|take_opened_files',
        )
        // A file that could not be prepared stays in its opening, so a backup opened with it is refused.
        await consumeOpenedFiles(files.map(file => file.path ?? { name: file.name ?? '', code: file.code ?? 'source-copy-failed' }))
    } catch (error) {
        alertError(`Failed to receive the selected file: ${error}`)
    }
}

function takeInjectedOpenedFiles(): unknown[] {
    const holder = window as Window & { tauriOpenedFiles?: unknown }
    const injected = holder.tauriOpenedFiles
    if (!Array.isArray(injected)) {
        return []
    }
    // The Android cold start injection runs once per page load, so drop it after draining.
    delete holder.tauriOpenedFiles
    return injected
}

/**
 * Explorer starts the app once per selected file, so the files of one selection reach the running
 * app a few notifications apart. Draining once the notifications stop imports them together.
 */
export const DESKTOP_OPENED_FILES_SETTLE_MS = 500
let desktopDrainTimer: ReturnType<typeof setTimeout> | undefined

function scheduleDesktopDrain(): void {
    clearTimeout(desktopDrainTimer)
    desktopDrainTimer = setTimeout(() => {
        desktopDrainTimer = undefined
        void drainDesktopOpenedFiles()
    }, DESKTOP_OPENED_FILES_SETTLE_MS)
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

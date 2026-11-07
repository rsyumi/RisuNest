import { invoke } from '@tauri-apps/api/core'
import type { NativeExportResult } from './storage/nativeExportFile'

export interface NativeLogEntry {
    tsMs: number
    level: string
    target: string
    message: string
}

export type NativeLogInvoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

export async function getNativeLogTail(
    invokeCommand: NativeLogInvoke = invoke,
): Promise<NativeLogEntry[]> {
    return (await invokeCommand('native_log_tail', { limit: 500 })) as NativeLogEntry[]
}

/** Target of the saved line that marks where one app run begins. */
export const NATIVE_LOG_RUN_START_TARGET = 'app-start'

export type NativeLogFile = 'current' | 'previous'

/** Where a page of the saved log ends; `before: null` reads from the end of the file. */
export interface NativeLogFileCursor {
    file: NativeLogFile
    before: number | null
}

export interface NativeLogFilePage {
    /** Oldest first. An entry with an empty level is a line the reader could not split. */
    entries: NativeLogEntry[]
    older: NativeLogFileCursor | null
}

export const NATIVE_LOG_NEWEST_PAGE: NativeLogFileCursor = { file: 'current', before: null }

export async function readNativeLogFilePage(
    cursor: NativeLogFileCursor = NATIVE_LOG_NEWEST_PAGE,
    invokeCommand: NativeLogInvoke = invoke,
): Promise<NativeLogFilePage> {
    return (await invokeCommand('native_log_file_read', { file: cursor.file, before: cursor.before })) as NativeLogFilePage
}

/** Refusal from `native_log_file_export` when neither log file holds anything. */
export const NATIVE_LOG_FILE_EMPTY = 'native-log-file-empty'

/** The saved log file name: the product name and the time it was saved. */
export function nativeLogFileName(now = Date.now()): string {
    const stamp = new Date(now).toISOString().replace(/[:.]/g, '-').replace(/-\d{3}Z$/, 'Z')
    return `risunest-error-log-${stamp}.txt`
}

export type NativeLogFileSave = 'saved' | 'cancelled' | 'empty'

/**
 * Saves the whole saved log, the previous file and then the current one, as one file the user picks.
 * The log is copied natively; nothing is asked for when there is nothing to save.
 */
export async function saveNativeLogFile(
    now = Date.now(),
    invokeCommand: NativeLogInvoke = invoke,
): Promise<NativeLogFileSave> {
    const newest = await readNativeLogFilePage(NATIVE_LOG_NEWEST_PAGE, invokeCommand)
    if (newest.entries.length === 0) return 'empty'
    const { saveNativeExportFile, nativeExportErrorCode, exportMayLeavePartialFile } = await import('./storage/nativeExportFile')
    try {
        const saved = await saveNativeExportFile(
            nativeLogFileName(now),
            [{ name: 'Text', extensions: ['txt'] }],
            async (destination) => (await invokeCommand('native_log_file_export', { destination })) as NativeExportResult,
        )
        return saved ? 'saved' : 'cancelled'
    } catch (error) {
        if (nativeExportErrorCode(error) === NATIVE_LOG_FILE_EMPTY && !exportMayLeavePartialFile(error)) return 'empty'
        throw error
    }
}

export async function recordNativeLogError(
    message: string,
    invokeCommand: NativeLogInvoke = invoke,
): Promise<void> {
    await invokeCommand('native_log_error', { message })
}

export async function getNativeLogFilePath(
    invokeCommand: NativeLogInvoke = invoke,
): Promise<string> {
    return (await invokeCommand('native_log_file_path')) as string
}

export async function setNativeLogFileEnabled(
    enabled: boolean,
    invokeCommand: NativeLogInvoke = invoke,
): Promise<void> {
    await invokeCommand('native_log_set_file_enabled', { enabled })
}

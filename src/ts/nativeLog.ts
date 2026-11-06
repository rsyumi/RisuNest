import { invoke } from '@tauri-apps/api/core'

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

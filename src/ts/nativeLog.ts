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

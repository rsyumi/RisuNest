import { invoke } from '@tauri-apps/api/core'
import { isTauri } from './platform'

type NativeStartupInvoke = (command: string) => Promise<unknown>

let startupFailed = false
let startupFailure: unknown

export async function checkNativeStartupStatus(
    native = isTauri,
    invokeCommand: NativeStartupInvoke = invoke,
): Promise<void> {
    if (!native) return
    if (startupFailed) throw startupFailure
    try {
        await invokeCommand('native_startup_status')
    } catch (error) {
        startupFailed = true
        startupFailure = error
        throw error
    }
}

import { invoke } from '@tauri-apps/api/core'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import type {
    NativeUpdateCheckResult,
    NativeUpdateEnvironment,
    NativeUpdateProgress,
    StagedDebUpdate,
} from './manifest'

export const nativeUpdate = {
    environment: () => invoke<NativeUpdateEnvironment>('app_update_environment'),
    check: () => invoke<NativeUpdateCheckResult>('app_update_check'),
    install: (handleId: string) => invoke<void>('app_update_install', { handleId }),
    stageDeb: (handleId: string) => invoke<StagedDebUpdate>('app_update_stage_deb', { handleId }),
    cancel: (handleId: string) => invoke<void>('app_update_cancel', { handleId }),
    onProgress: (callback: (progress: NativeUpdateProgress) => void): Promise<UnlistenFn> =>
        listen<NativeUpdateProgress>('app-update://progress', event => callback(event.payload)),
    onApplying: (callback: (handleId: string) => void): Promise<UnlistenFn> =>
        listen<string>('app-update://applying', event => callback(event.payload)),
}

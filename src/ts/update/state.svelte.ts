import { writable } from 'svelte/store'
import type { AvailableAppUpdate, NativeUpdateEnvironment, NativeUpdateProgress, StagedDebUpdate } from './manifest'

export type AppUpdatePhase =
    | 'idle'
    | 'checking'
    | 'current'
    | 'available'
    | 'downloading'
    | 'applying'
    | 'staged'
    | 'disabled'
    | 'error'

export interface AppUpdateState {
    phase: AppUpdatePhase
    environment: NativeUpdateEnvironment | null
    update: AvailableAppUpdate | null
    progress: NativeUpdateProgress | null
    stagedDeb: StagedDebUpdate | null
    error: string
    popupVisible: boolean
    manual: boolean
}

export const initialAppUpdateState: AppUpdateState = {
    phase: 'idle',
    environment: null,
    update: null,
    progress: null,
    stagedDeb: null,
    error: '',
    popupVisible: false,
    manual: false,
}

export const appUpdateState = writable<AppUpdateState>({ ...initialAppUpdateState })

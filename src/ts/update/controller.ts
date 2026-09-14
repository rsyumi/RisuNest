import { get } from 'svelte/store'
import { relaunch } from '@tauri-apps/plugin-process'
import { openURL } from '../globalApi.svelte'
import { isTauri } from '../platform'
import type { AvailableAppUpdate, NativeUpdateProgress } from './manifest'
import { nativeUpdate } from './native'
import {
    getAppUpdateSettings,
    subscribeAppUpdateSettings,
    updateAppUpdateSettings,
    type AppUpdateSettings,
} from './settings'
import { appUpdateState } from './state.svelte'

export const UPDATE_CHECK_INTERVAL_MS = 6 * 60 * 60 * 1000
const UPDATE_CHECK_TIMEOUT_MS = 30_000

let activeCheck: Promise<void> | null = null
let started = false
let dismissedVersion = ''
let stopListeners: (() => void)[] = []
let automaticTimer: ReturnType<typeof setTimeout> | null = null
let stopSettingsListener: (() => void) | null = null

export interface UpdateControllerDependencies {
    now: () => number
    settings: () => AppUpdateSettings
    saveSettings: typeof updateAppUpdateSettings
    check: typeof nativeUpdate.check
    environment: typeof nativeUpdate.environment
    install: typeof nativeUpdate.install
    stageDeb: typeof nativeUpdate.stageDeb
    cancel: typeof nativeUpdate.cancel
    open: (url: string) => void
    restart: () => Promise<void>
}

const production: UpdateControllerDependencies = {
    now: () => Date.now(),
    settings: getAppUpdateSettings,
    saveSettings: updateAppUpdateSettings,
    check: nativeUpdate.check,
    environment: nativeUpdate.environment,
    install: nativeUpdate.install,
    stageDeb: nativeUpdate.stageDeb,
    cancel: nativeUpdate.cancel,
    open: openURL,
    restart: relaunch,
}

export async function checkForAppUpdate(
    manual = false,
    dependencies: UpdateControllerDependencies = production,
): Promise<void> {
    if (!isTauri && dependencies === production) return
    if (['downloading', 'applying'].includes(get(appUpdateState).phase)) return
    if (activeCheck) return activeCheck
    activeCheck = runCheck(manual, dependencies).finally(() => { activeCheck = null })
    return activeCheck
}

async function runCheck(manual: boolean, dependencies: UpdateControllerDependencies): Promise<void> {
    let settings: AppUpdateSettings
    try {
        settings = dependencies.settings()
    } catch (error) {
        appUpdateState.update(state => ({
            ...state,
            phase: 'error',
            error: describe(error),
            manual,
            popupVisible: manual,
        }))
        return
    }
    if (!manual) {
        if (!settings.autoUpdateCheck) return
        if (dependencies.now() - settings.lastCheckedAt < UPDATE_CHECK_INTERVAL_MS) return
    }
    appUpdateState.update(state => ({ ...state, phase: 'checking', error: '', manual, popupVisible: manual }))
    try {
        dependencies.saveSettings({ lastCheckedAt: dependencies.now() })
        const result = await withTimeout(dependencies.check(), UPDATE_CHECK_TIMEOUT_MS)
        if (result.status === 'disabled') {
            appUpdateState.update(state => ({ ...state, phase: 'disabled', popupVisible: manual }))
            return
        }
        if (result.status === 'unsupported') {
            appUpdateState.update(state => ({
                ...state,
                phase: 'error',
                error: 'This installation cannot be updated automatically.',
                popupVisible: manual,
            }))
            return
        }
        if (result.status === 'current') {
            appUpdateState.update(state => ({
                ...state,
                phase: 'current',
                update: null,
                popupVisible: manual,
            }))
            return
        }
        if (!result.update) throw new Error('Native updater returned an incomplete update')
        const suppressed = !manual && (
            settings.skippedVersion === result.update.version
            || dismissedVersion === result.update.version
        )
        appUpdateState.update(state => ({
            ...state,
            phase: 'available',
            update: result.update,
            progress: null,
            stagedDeb: null,
            popupVisible: !suppressed,
            manual,
        }))
    } catch (error) {
        appUpdateState.update(state => ({
            ...state,
            phase: 'error',
            error: describe(error),
            popupVisible: manual,
            manual,
        }))
    }
}

export async function startAppUpdateChecks(): Promise<void> {
    if (started || !isTauri) return
    started = true
    try {
        const environment = await nativeUpdate.environment()
        appUpdateState.update(state => ({ ...state, environment }))
        stopListeners = await Promise.all([
            nativeUpdate.onProgress(updateProgress),
            nativeUpdate.onApplying(handleId => {
                if (get(appUpdateState).update?.handleId === handleId) {
                    appUpdateState.update(state => ({ ...state, phase: 'applying', progress: null }))
                }
            }),
        ])
        if (!environment.configured) {
            appUpdateState.update(state => ({ ...state, phase: 'disabled' }))
            return
        }
        stopSettingsListener = subscribeAppUpdateSettings(scheduleAutomaticCheck)
        const run = () => scheduleAutomaticCheck()
        if ('requestIdleCallback' in window) window.requestIdleCallback(run, { timeout: 5_000 })
        else setTimeout(run, 0)
    } catch (error) {
        appUpdateState.update(state => ({ ...state, phase: 'error', error: describe(error) }))
    }
}

export function stopAppUpdateChecks(): void {
    if (automaticTimer !== null) clearTimeout(automaticTimer)
    automaticTimer = null
    stopSettingsListener?.()
    stopSettingsListener = null
    for (const stop of stopListeners) stop()
    stopListeners = []
    started = false
}

function scheduleAutomaticCheck(): void {
    if (automaticTimer !== null) clearTimeout(automaticTimer)
    automaticTimer = null
    if (!started) return
    let settings: AppUpdateSettings
    try {
        settings = getAppUpdateSettings()
    } catch (error) {
        appUpdateState.update(state => ({ ...state, phase: 'error', error: describe(error) }))
        return
    }
    if (!settings.autoUpdateCheck) return
    const delay = Math.max(0, UPDATE_CHECK_INTERVAL_MS - (Date.now() - settings.lastCheckedAt))
    automaticTimer = setTimeout(async () => {
        automaticTimer = null
        await checkForAppUpdate(false)
        scheduleAutomaticCheck()
    }, delay)
}

export function dismissAppUpdate(): void {
    const update = get(appUpdateState).update
    if (update) dismissedVersion = update.version
    appUpdateState.update(state => ({ ...state, popupVisible: false }))
}

export function skipAppUpdate(): void {
    const update = get(appUpdateState).update
    if (!update) return
    updateAppUpdateSettings({ skippedVersion: update.version })
    appUpdateState.update(state => ({ ...state, popupVisible: false }))
}

export function clearSkippedAppUpdate(): void {
    updateAppUpdateSettings({ skippedVersion: '' })
}

export async function applyAppUpdate(dependencies: UpdateControllerDependencies = production): Promise<void> {
    const update = get(appUpdateState).update
    if (!update) return
    if (update.installStrategy === 'open-link' || update.installStrategy === 'disabled') {
        dependencies.open(update.installStrategy === 'open-link' ? update.downloadUrl : update.releasePage)
        dismissAppUpdate()
        return
    }
    appUpdateState.update(state => ({ ...state, phase: 'downloading', progress: null, error: '' }))
    try {
        if (update.installStrategy === 'stage-deb') {
            const stagedDeb = await dependencies.stageDeb(update.handleId)
            appUpdateState.update(state => ({ ...state, phase: 'staged', stagedDeb, progress: null }))
            return
        }
        await dependencies.install(update.handleId)
        await dependencies.restart()
    } catch (error) {
        const message = describe(error)
        appUpdateState.update(state => ({
            ...state,
            phase: message === 'cancelled' ? 'available' : 'error',
            error: message === 'cancelled' ? '' : message,
            progress: null,
        }))
    }
}

export async function cancelAppUpdateDownload(dependencies: UpdateControllerDependencies = production): Promise<void> {
    const update = get(appUpdateState).update
    if (!update || get(appUpdateState).phase !== 'downloading') return
    await dependencies.cancel(update.handleId)
}

function updateProgress(progress: NativeUpdateProgress): void {
    const state = get(appUpdateState)
    if (state.update?.handleId !== progress.handleId) return
    appUpdateState.update(current => ({ ...current, progress }))
}

function withTimeout<T>(promise: Promise<T>, timeout: number): Promise<T> {
    return new Promise<T>((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('Update check timed out')), timeout)
        promise.then(
            value => { clearTimeout(timer); resolve(value) },
            error => { clearTimeout(timer); reject(error) },
        )
    })
}

function describe(error: unknown): string {
    return error instanceof Error ? error.message : String(error)
}

export type { AvailableAppUpdate }

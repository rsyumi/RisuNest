import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { get } from 'svelte/store'

vi.mock('../globalApi.svelte', () => ({ openURL: vi.fn() }))
vi.mock('../platform', () => ({ isTauri: true }))
vi.mock('@tauri-apps/plugin-process', () => ({ relaunch: vi.fn() }))
const nativeMocks = vi.hoisted(() => ({
    environment: vi.fn(async () => ({
        currentVersion: '1.0.0',
        installStrategy: 'self-install' as const,
        configured: true,
        disabledReason: null,
    })),
    check: vi.fn(),
    install: vi.fn(),
    stageDeb: vi.fn(),
    cancel: vi.fn(),
    onProgress: vi.fn(async () => () => undefined),
    onApplying: vi.fn(async () => () => undefined),
}))
vi.mock('./native', () => ({ nativeUpdate: nativeMocks }))

import {
    checkForAppUpdate,
    applyAppUpdate,
    startAppUpdateChecks,
    stopAppUpdateChecks,
    UPDATE_CHECK_INTERVAL_MS,
    type UpdateControllerDependencies,
} from './controller'
import { appUpdateState, initialAppUpdateState } from './state.svelte'
import type { NativeUpdateCheckResult } from './manifest'
import { getAppUpdateSettings, updateAppUpdateSettings } from './settings'

const available: NativeUpdateCheckResult = {
    status: 'available',
    currentVersion: '1.0.0',
    disabledReason: null,
    update: {
        handleId: 'verified-handle',
        version: '2.0.0',
        pubDate: '2026-09-15T00:00:00Z',
        notes: 'Notes',
        localizedNotes: { en: 'Notes' },
        releasePage: 'https://github.com/rsyumi/RisuNest/releases/tag/app-v2.0.0',
        installStrategy: 'self-install',
        downloadUrl: 'https://github.com/rsyumi/RisuNest/releases/download/app-v2.0.0/app.exe',
        downloadSize: 100,
        format: 'nsis',
    },
}

function dependencies(options: { auto?: boolean; skipped?: string; check?: () => Promise<NativeUpdateCheckResult> } = {}) {
    const check = vi.fn(options.check ?? (async () => available))
    const settings = {
        schema: 'risunest.app-update-settings/v1' as const,
        autoUpdateCheck: options.auto ?? true,
        skippedVersion: options.skipped ?? '',
        lastCheckedAt: 0,
    }
    const saveSettings = vi.fn((patch) => ({
        ...Object.assign(settings, patch),
    }))
    const value: UpdateControllerDependencies = {
        now: () => 30_000_000,
        settings: () => ({ ...settings }),
        saveSettings,
        check,
        environment: vi.fn(),
        install: vi.fn(),
        stageDeb: vi.fn(),
        cancel: vi.fn(),
        open: vi.fn(),
        restart: vi.fn(),
        prepareInstall: vi.fn(async () => ({ release: vi.fn() })),
    }
    return { value, check, saveSettings }
}

describe('app update controller', () => {
    beforeEach(() => {
        stopAppUpdateChecks()
        appUpdateState.set({ ...initialAppUpdateState })
        nativeMocks.check.mockReset()
        nativeMocks.check.mockResolvedValue(available)
    })

    afterEach(() => {
        stopAppUpdateChecks()
        vi.unstubAllGlobals()
        vi.useRealTimers()
    })

    it('does not issue an automatic request while checks are off', async () => {
        const fixture = dependencies({ auto: false })
        await checkForAppUpdate(false, fixture.value)
        expect(fixture.check).not.toHaveBeenCalled()
    })

    it('manual checks bypass the off setting and show their result', async () => {
        const fixture = dependencies({ auto: false })
        await checkForAppUpdate(true, fixture.value)
        expect(fixture.check).toHaveBeenCalledTimes(1)
        expect(fixture.saveSettings).toHaveBeenCalledWith({ lastCheckedAt: 30_000_000 })
        expect(get(appUpdateState)).toMatchObject({ phase: 'available', popupVisible: true })
    })

    it('suppresses only the exact skipped version during automatic checks', async () => {
        const fixture = dependencies({ skipped: '2.0.0' })
        await checkForAppUpdate(false, fixture.value)
        expect(get(appUpdateState)).toMatchObject({ phase: 'available', popupVisible: false })

        const newer = structuredClone(available)
        newer.update!.version = '3.0.0'
        const next = dependencies({ skipped: '2.0.0', check: async () => newer })
        await checkForAppUpdate(false, next.value)
        expect(get(appUpdateState)).toMatchObject({ phase: 'available', popupVisible: true })
    })

    it('coalesces concurrent checks into one native request', async () => {
        let resolve!: (value: NativeUpdateCheckResult) => void
        const pending = new Promise<NativeUpdateCheckResult>(done => { resolve = done })
        const fixture = dependencies({ check: () => pending })
        const first = checkForAppUpdate(true, fixture.value)
        const second = checkForAppUpdate(true, fixture.value)
        expect(fixture.check).toHaveBeenCalledTimes(1)
        resolve(available)
        await Promise.all([first, second])
        expect(fixture.check).toHaveBeenCalledTimes(1)
    })

    it('never reports an unsupported newer release as current', async () => {
        const fixture = dependencies({
            check: async () => ({
                status: 'unsupported',
                currentVersion: '1.0.0',
                disabledReason: null,
                update: null,
            }),
        })
        await checkForAppUpdate(true, fixture.value)
        expect(get(appUpdateState)).toMatchObject({ phase: 'error', popupVisible: true })
    })

    it('waits six hours before retrying a sustained automatic failure', async () => {
        vi.useFakeTimers()
        vi.setSystemTime(UPDATE_CHECK_INTERVAL_MS + 1_000)
        vi.stubGlobal('requestIdleCallback', (callback: IdleRequestCallback) => {
            callback({ didTimeout: false, timeRemaining: () => 50 })
            return 1
        })
        updateAppUpdateSettings({ autoUpdateCheck: true, skippedVersion: '', lastCheckedAt: 0 })
        nativeMocks.check.mockRejectedValue(new Error('synthetic network failure'))

        await startAppUpdateChecks()
        vi.advanceTimersByTime(0)
        for (let index = 0; index < 5; index += 1) await Promise.resolve()
        expect(nativeMocks.check).toHaveBeenCalledTimes(1)
        expect(getAppUpdateSettings().lastCheckedAt).toBe(Date.now())
        expect(vi.getTimerCount()).toBe(1)

        vi.advanceTimersByTime(UPDATE_CHECK_INTERVAL_MS - 1)
        expect(nativeMocks.check).toHaveBeenCalledTimes(1)
        expect(vi.getTimerCount()).toBe(1)
        await vi.advanceTimersToNextTimerAsync()
        expect(nativeMocks.check).toHaveBeenCalledTimes(2)
    })

    it('awaits local persistence and retains the fence through installation and restart', async () => {
        const fixture = dependencies()
        const order: string[] = []
        let finishPreparation!: (value: { release(): void }) => void
        const release = vi.fn(() => { order.push('release') })
        fixture.value.prepareInstall = vi.fn(() => new Promise<{ release(): void }>(resolve => { finishPreparation = resolve }))
        fixture.value.install = vi.fn(async () => {
            expect(release).not.toHaveBeenCalled()
            order.push('install')
        })
        fixture.value.restart = vi.fn(async () => {
            expect(release).not.toHaveBeenCalled()
            order.push('restart')
        })
        appUpdateState.update(state => ({ ...state, update: available.update }))
        const pending = applyAppUpdate(fixture.value)
        expect(fixture.value.install).not.toHaveBeenCalled()
        await applyAppUpdate(fixture.value)
        expect(fixture.value.prepareInstall).toHaveBeenCalledTimes(1)
        finishPreparation({ release })
        await pending
        expect(fixture.value.install).toHaveBeenCalledWith('verified-handle')
        expect(order).toEqual(['install', 'restart', 'release'])
    })

    it('does not install or restart when local persistence fails', async () => {
        const fixture = dependencies()
        fixture.value.prepareInstall = vi.fn().mockRejectedValue(new Error('local save failed'))
        appUpdateState.update(state => ({ ...state, update: available.update }))
        await applyAppUpdate(fixture.value)
        expect(fixture.value.install).not.toHaveBeenCalled()
        expect(fixture.value.restart).not.toHaveBeenCalled()
        expect(get(appUpdateState)).toMatchObject({ phase: 'error', error: 'local save failed' })
    })

    it.each(['install', 'restart'] as const)('releases editing protection after %s failure', async (step) => {
        const fixture = dependencies()
        const release = vi.fn()
        fixture.value.prepareInstall = vi.fn(async () => ({ release }))
        fixture.value[step] = vi.fn().mockRejectedValue(new Error('synthetic failure'))
        appUpdateState.update(state => ({ ...state, update: available.update }))
        await applyAppUpdate(fixture.value)
        expect(release).toHaveBeenCalledTimes(1)
        expect(get(appUpdateState)).toMatchObject({ phase: 'error', error: 'synthetic failure' })
        if (step === 'install') expect(fixture.value.restart).not.toHaveBeenCalled()
    })

    it('releases editing protection after installer cancellation', async () => {
        const fixture = dependencies()
        const release = vi.fn()
        fixture.value.prepareInstall = vi.fn(async () => ({ release }))
        fixture.value.install = vi.fn().mockRejectedValue(new Error('cancelled'))
        appUpdateState.update(state => ({ ...state, update: available.update }))
        await applyAppUpdate(fixture.value)
        expect(release).toHaveBeenCalledTimes(1)
        expect(fixture.value.restart).not.toHaveBeenCalled()
        expect(get(appUpdateState)).toMatchObject({ phase: 'available', error: '' })
    })

    it.each(['stage-deb', 'open-link', 'disabled'] as const)('does not fence editing for %s', async (installStrategy) => {
        const fixture = dependencies()
        appUpdateState.update(state => ({ ...state, update: { ...available.update!, installStrategy } }))
        await applyAppUpdate(fixture.value)
        expect(fixture.value.prepareInstall).not.toHaveBeenCalled()
        expect(fixture.value.install).not.toHaveBeenCalled()
        expect(fixture.value.restart).not.toHaveBeenCalled()
        if (installStrategy === 'stage-deb') expect(fixture.value.stageDeb).toHaveBeenCalledTimes(1)
        else expect(fixture.value.open).toHaveBeenCalledTimes(1)
    })

    it('does not start a check while an install is active', async () => {
        const fixture = dependencies()
        appUpdateState.update(state => ({ ...state, phase: 'applying' }))
        await checkForAppUpdate(true, fixture.value)
        expect(fixture.check).not.toHaveBeenCalled()
    })
})

// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const maintenance = vi.hoisted(() => ({
    scanNativeDataHealth: vi.fn(),
    deepScanNativeDataHealth: vi.fn(),
    getNativeDataHealthResult: vi.fn().mockResolvedValue(null),
    cancelNativeDataHealthScan: vi.fn(),
    holdNativeDataHealthScan: vi.fn(<T,>(operation: (signal?: AbortSignal) => Promise<T>) => operation()),
    planNativeDataHealthRepair: vi.fn().mockResolvedValue([]),
    previewNativeDataHealthRepair: vi.fn(),
    applyNativeDataHealthRepair: vi.fn(),
    discardNativeDataHealthIntent: vi.fn(),
    completeNativeDataHealthIntent: vi.fn(),
    listNativeDataHealthJournals: vi.fn().mockResolvedValue([]),
    undoNativeDataHealthRepair: vi.fn(),
}))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
vi.mock('src/ts/alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
const core = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue({ revision: 0 }) }))
const rawRecovery = vi.hoisted(() => ({
    exportOriginalData: vi.fn().mockResolvedValue({ warningCodes: [] }),
}))
vi.mock('@tauri-apps/api/core', () => core)
vi.mock('src/ts/storage/rawRecoveryExport', () => rawRecovery)

import RecoveryShell from './RecoveryShell.svelte'
import { languageEnglish } from 'src/lang/en'
import {
    clearBootTrail,
    markBootStage,
    markBootSuspect,
} from 'src/ts/storage/bootAttempt'
import { decideBoot } from 'src/ts/storage/recoveryMode.svelte'
import { rendererDocument } from 'src/ts/storage/rendererDocument'

const strings = languageEnglish.risuNest.recovery

async function settle(): Promise<void> {
    for (let index = 0; index < 12; index += 1) await tick()
}

describe('RecoveryShell', () => {
    let mounted: ReturnType<typeof mount> | undefined

    beforeEach(() => {
        localStorage.clear()
        clearBootTrail()
    })

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    async function setup(
        props: Record<string, unknown> = {},
    ): Promise<HTMLElement> {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RecoveryShell, {
            target,
            props: { onStart: vi.fn(), ...props },
        })
        await settle()
        return target
    }

    it('summarises the start that did not finish, including the suspect', async () => {
        markBootStage('plugins')
        markBootSuspect('plugin:translator')
        await decideBoot({
            begin: vi.fn().mockResolvedValue({
                consecutiveFailures: 2,
                previous: {
                    startedAt: Date.UTC(2026, 8, 15, 3, 0, 0),
                    appVersion: '1.2.3',
                    consecutiveFailures: 1,
                },
            }),
            complete: vi.fn(),
        })
        const body = await setup()
        const summary = body.querySelector('[data-recovery-summary]')
        expect(summary?.textContent).toContain(strings.failures.replace('{0}', '2'))
        expect(summary?.textContent).toContain(strings.stage.replace('{0}', languageEnglish.risuNest.startup.plugins))
        expect(summary?.textContent).toContain(
            strings.suspect.replace('{0}', 'plugin:translator'),
        )
    })

    it.each([
        ['app-data-directories', 'storage'],
        ['persistent-storage', 'storage'],
        ['plugin-compatibility-data', 'compatibility'],
        ['persistent-database', 'data'],
        ['format-update', 'data'],
        ['account-bootstrap', 'account'],
        ['drive-sync', 'account'],
        ['account-data', 'account'],
        ['service-worker', 'serviceWorker'],
        ['plugins', 'plugins'],
        ['ui-state', 'ui'],
    ] as const)('names the %s stage by the loading text it showed', async (stage, label) => {
        markBootStage(stage)
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 2 }),
            complete: vi.fn(),
        })
        const summary = (await setup()).querySelector('[data-recovery-summary]')?.textContent
        expect(summary).toContain(strings.stage.replace('{0}', languageEnglish.risuNest.startup[label]))
        expect(summary).not.toContain(strings.stage.replace('{0}', stage))
    })

    it('does not show a stage it has no name for', async () => {
        markBootStage('unknown-stage')
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 2 }),
            complete: vi.fn(),
        })
        const summary = (await setup()).querySelector('[data-recovery-summary]')?.textContent
        expect(summary).toContain(strings.stageUnknown)
        expect(summary).not.toContain('unknown-stage')
    })

    it('says so when the trail was lost', async () => {
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 2 }),
            complete: vi.fn(),
        })
        const body = await setup()
        expect(
            body.querySelector('[data-recovery-summary]')?.textContent,
        ).toContain(strings.stageUnknown)
    })

    it('hands the exclusions to the ordinary start rather than saving them', async () => {
        markBootStage('plugins')
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 1 }),
            complete: vi.fn(),
        })
        const onStart = vi.fn()
        const body = await setup({ onStart })
        const start = [...body.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.startNormally,
        )
        start?.click()
        await settle()
        expect(onStart).toHaveBeenCalledWith(['plugins'])
        expect(localStorage.getItem('risuNestDeviceSettings')).toBeNull()
    })

    it('opens the store itself before the data check asks it anything', async () => {
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 2 }),
            complete: vi.fn(),
        })
        const body = await setup()
        expect(body.querySelector('[data-data-health]')).toBeTruthy()
        // The start that would have opened it is exactly what this shell replaced.
        expect(core.invoke).toHaveBeenCalledWith('pds_open', { document: rendererDocument() })
        expect(maintenance.getNativeDataHealthResult).toHaveBeenCalled()
        expect(
            core.invoke.mock.invocationCallOrder[0],
        ).toBeLessThan(
            maintenance.getNativeDataHealthResult.mock.invocationCallOrder[0],
        )
    })

    it('retries the actual rejected store-open promise before enabling diagnosis', async () => {
        core.invoke.mockRejectedValueOnce(new Error('synthetic open failure'))
        const body = await setup()
        const copy = languageEnglish.risuNest.dataHealth
        expect(body.textContent).toContain(copy.openFailed)
        expect(maintenance.getNativeDataHealthResult).not.toHaveBeenCalled()
        const find = (text: string) => [...body.querySelectorAll('button')].find(button => button.textContent?.trim() === text)!
        expect(find(copy.quickScan).disabled).toBe(true)
        find(languageEnglish.retry).click()
        await settle()
        expect(core.invoke.mock.calls.filter(([command]) => command === 'pds_open')).toHaveLength(2)
        expect(maintenance.getNativeDataHealthResult).toHaveBeenCalledOnce()
        expect(find(copy.quickScan).disabled).toBe(false)
        expect(body.textContent).not.toContain(copy.openFailed)
    })
    it('uses the shared runtime-independent route for original data export', async () => {
        await decideBoot({
            begin: vi.fn().mockResolvedValue({ consecutiveFailures: 2 }),
            complete: vi.fn(),
        })
        const body = await setup()
        const exportButton = [...body.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.exportAction,
        )
        exportButton?.click()
        await settle()
        expect(rawRecovery.exportOriginalData).toHaveBeenCalledOnce()
        expect(body.querySelector('[data-recovery-export]')?.textContent)
            .toContain(strings.exportComplete)
    })
})

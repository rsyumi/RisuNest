// @vitest-environment happy-dom

import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const maintenance = vi.hoisted(() => ({
    scanNativeDataHealth: vi.fn(),
    deepScanNativeDataHealth: vi.fn(),
    getNativeDataHealthResult: vi.fn().mockResolvedValue(null),
    cancelNativeDataHealthScan: vi.fn(),
    planNativeDataHealthRepair: vi.fn().mockResolvedValue([]),
    previewNativeDataHealthRepair: vi.fn(),
    applyNativeDataHealthRepair: vi.fn(),
    listNativeDataHealthJournals: vi.fn().mockResolvedValue([]),
    undoNativeDataHealthRepair: vi.fn(),
}))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
vi.mock('src/ts/alert', () => ({ alertError: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/ts/globalApi.svelte', () => ({ downloadFile: vi.fn() }))
vi.mock('src/ts/platform', () => ({ isTauri: true }))
const core = vi.hoisted(() => ({ invoke: vi.fn().mockResolvedValue({ revision: 0 }) }))
const rawRecovery = vi.hoisted(() => ({
    exportOriginalData: vi.fn().mockResolvedValue({ warningCodes: [] }),
}))
vi.mock('@tauri-apps/api/core', () => core)
vi.mock('src/ts/storage/rawRecoveryExport', () => rawRecovery)
vi.mock('src/lang', async () => ({
    language: (await import('src/lang/en')).languageEnglish,
}))

import RecoveryShell from './RecoveryShell.svelte'
import { languageEnglish } from 'src/lang/en'
import { languageKorean } from 'src/lang/ko'
import {
    clearBootTrail,
    markBootStage,
    markBootSuspect,
} from 'src/ts/storage/bootAttempt'
import { decideBoot } from 'src/ts/storage/recoveryMode.svelte'

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
        expect(summary?.textContent).toContain(strings.stage.replace('{0}', 'plugins'))
        expect(summary?.textContent).toContain(
            strings.suspect.replace('{0}', 'plugin:translator'),
        )
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
        expect(core.invoke).toHaveBeenCalledWith('pds_open')
        expect(maintenance.getNativeDataHealthResult).toHaveBeenCalled()
        expect(
            core.invoke.mock.invocationCallOrder[0],
        ).toBeLessThan(
            maintenance.getNativeDataHealthResult.mock.invocationCallOrder[0],
        )
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

    it('keeps every string it shows in both shipped languages', () => {
        for (const key of Object.keys(strings)) {
            expect(
                languageKorean.risuNest.recovery[key as keyof typeof strings],
                key,
            ).toBeTruthy()
        }
    })
})

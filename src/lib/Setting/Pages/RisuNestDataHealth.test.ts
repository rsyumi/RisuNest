// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const maintenance = vi.hoisted(() => ({
    scanNativeDataHealth: vi.fn(),
    deepScanNativeDataHealth: vi.fn(),
    getNativeDataHealthResult: vi.fn(),
    cancelNativeDataHealthScan: vi.fn(),
    planNativeDataHealthRepair: vi.fn(),
    previewNativeDataHealthRepair: vi.fn(),
    applyNativeDataHealthRepair: vi.fn(),
    listNativeDataHealthJournals: vi.fn(),
    undoNativeDataHealthRepair: vi.fn(),
}))
const alerts = vi.hoisted(() => ({
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))
const files = vi.hoisted(() => ({ downloadFile: vi.fn() }))

vi.mock('src/ts/storage/nativePersistentMaintenance', () => maintenance)
vi.mock('src/ts/alert', () => alerts)
vi.mock('src/ts/globalApi.svelte', () => files)
vi.mock('src/lang', async () => ({
    language: (await import('src/lang/en')).languageEnglish,
}))

import RisuNestDataHealth from './RisuNestDataHealth.svelte'
import { languageKorean } from 'src/lang/ko'
import { languageEnglish } from 'src/lang/en'
import type { DataHealthResult } from 'src/ts/storage/dataHealth'

const strings = languageEnglish.risuNest.dataHealth

const damaged: DataHealthResult = {
    revision: 12,
    scannedAt: Date.UTC(2026, 8, 15, 4, 0, 0),
    depth: 'quick',
    counts: { blocking: 1, degraded: 1, informational: 0 },
    omitted: 4,
    items: [
        {
            code: 'record-invalid',
            severity: 'blocking',
            owner: { kind: 'character', id: 'char-1' },
            locator: null,
            target: null,
            detail: 'record JSON is invalid',
        },
        {
            code: 'reference-missing',
            severity: 'degraded',
            owner: { kind: 'character', id: 'char-2' },
            locator: { sourcePath: '$.image', occurrence: 0 },
            target: { kind: 'asset', key: 'assets/portrait.png' },
            detail: 'reference has no target in this library',
        },
    ],
}

const candidates = [
    {
        id: '1:drop-reference',
        action: {
            action: 'drop-reference' as const,
            owner: { kind: 'character', id: 'char-2' },
            sourcePath: '$.image',
            occurrence: 0,
        },
        finding: 1,
        preferred: true,
        discards: true,
    },
]

const journals = [
    {
        id: 'repair-1',
        createdAt: Date.UTC(2026, 8, 15, 5, 0, 0),
        fromRevision: 11,
        toRevision: 12,
        changes: 2,
        heldObjects: 1,
        current: true,
    },
]

async function settle(): Promise<void> {
    for (let index = 0; index < 12; index += 1) await tick()
}

describe('RisuNestDataHealth', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    async function setup(
        stored: DataHealthResult | null = null,
        props: Record<string, unknown> = {},
        plan = candidates,
    ): Promise<HTMLElement> {
        maintenance.getNativeDataHealthResult.mockResolvedValue(stored)
        maintenance.planNativeDataHealthRepair.mockResolvedValue(plan)
        maintenance.previewNativeDataHealthRepair.mockImplementation(
            async (selection: string[]) => ({
                selected: candidates.filter((candidate) => selection.includes(candidate.id)),
                answered: selection.length,
                remaining: 2 - selection.length,
                droppedReferences: 1,
                droppedAliases: 0,
                discarding: selection,
                tables: ['characters'],
                proposesSnapshot: true,
            }),
        )
        maintenance.listNativeDataHealthJournals.mockResolvedValue(journals)
        maintenance.applyNativeDataHealthRepair.mockResolvedValue({
            revision: 13,
            journalId: 'repair-1',
            snapshot: null,
            result: { ...damaged, items: [], counts: { blocking: 0, degraded: 0, informational: 0 } },
        })
        maintenance.undoNativeDataHealthRepair.mockResolvedValue({
            revision: 14,
            skipped: ['characters:char-9'],
            result: damaged,
        })
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestDataHealth, { target, props })
        await settle()
        return target
    }

    it('shows the last diagnosis on open without scanning again', async () => {
        const target = await setup(damaged)
        expect(maintenance.scanNativeDataHealth).not.toHaveBeenCalled()
        const summary = target.querySelector('[data-data-health-summary]')
        expect(summary?.textContent).toContain(strings.severityBlocking)
        expect(summary?.textContent).toContain(
            strings.omitted.replace('{0}', '4'),
        )
        expect(target.querySelectorAll('[data-data-health-group]')).toHaveLength(2)
        expect(target.querySelectorAll('[data-data-health-item]')).toHaveLength(2)
    })

    it('says nothing has been checked before the first scan', async () => {
        const target = await setup(null)
        expect(
            target.querySelector('[data-data-health-summary]')?.textContent,
        ).toContain(strings.never)
        expect(target.querySelectorAll('[data-data-health-group]')).toHaveLength(0)
    })

    it('runs a quick check on request and shows what it found', async () => {
        maintenance.scanNativeDataHealth.mockResolvedValue(damaged)
        const target = await setup(null)
        const button = [...target.querySelectorAll('button')].find(
            (candidate) => candidate.textContent?.trim() === strings.quickScan,
        )
        button?.click()
        await settle()
        expect(maintenance.scanNativeDataHealth).toHaveBeenCalledOnce()
        expect(target.querySelectorAll('[data-data-health-group]')).toHaveLength(2)
    })

    it('offers to continue a full check that stopped before it finished', async () => {
        const target = await setup({
            ...damaged,
            depth: 'deep',
            deep: {
                cursor: 'hash-1',
                completedObjects: 1,
                totalObjects: 4,
                completedBytes: 1024,
                totalBytes: 4096,
                complete: false,
            },
        })
        const labels = [...target.querySelectorAll('button')].map((button) =>
            button.textContent?.trim(),
        )
        expect(labels).toContain(strings.resume)
        expect(labels).toContain(strings.restart)
        expect(
            target.querySelector('[data-data-health-summary]')?.textContent,
        ).toContain(strings.deepStopped)
    })

    it('links the cleanup group to the unused image screen instead of deleting here', async () => {
        const onOpenUnusedImages = vi.fn()
        const target = await setup(
            {
                ...damaged,
                counts: { blocking: 0, degraded: 0, informational: 1 },
                items: [
                    {
                        code: 'object-unreferenced',
                        severity: 'informational',
                        owner: { kind: 'asset', id: 'abcd' },
                        locator: null,
                        target: null,
                        detail: 'no alias points at this object',
                    },
                ],
            },
            { onOpenUnusedImages },
        )
        const link = [...target.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.gcLink,
        )
        expect(link).toBeTruthy()
        link?.click()
        expect(onOpenUnusedImages).toHaveBeenCalledOnce()
    })

    it('saves a report that carries no name unless the reader asks', async () => {
        const target = await setup(damaged)
        const save = [...target.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.saveReport,
        )
        save?.click()
        await settle()
        const [, report] = files.downloadFile.mock.calls[0] as [string, string]
        expect(report).toContain('names\tmasked')
        // Identities and locators stay; a display name never appears under this profile.
        expect(report).toContain('character:char-1')
        expect(report).toContain('target=asset:assets/portrait.png')
        expect(report).not.toContain('name=')
    })


    it('offers one answer per problem and previews what it will do', async () => {
        const target = await setup(damaged)
        const choices = target.querySelectorAll('[data-data-health-choice]')
        expect(choices).toHaveLength(1)
        expect(choices[0].textContent).toContain(strings.actionDropReference)
        const preview = target.querySelector('[data-data-health-preview]')
        expect(preview?.textContent).toContain(strings.previewTitle)
        expect(preview?.textContent).toContain(
            strings.previewAnswered.replace('{0}', '1').replace('{1}', '2'),
        )
        expect(preview?.textContent).toContain(
            strings.previewDiscards.replace('{0}', '1'),
        )
    })

    it('applies the selection and shows the diagnosis of the repaired library', async () => {
        const target = await setup(damaged)
        const apply = [...target.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.repairApply,
        )
        apply?.click()
        await settle()
        expect(maintenance.applyNativeDataHealthRepair).toHaveBeenCalledWith(
            ['1:drop-reference'],
            true,
        )
        expect(target.querySelectorAll('[data-data-health-group]')).toHaveLength(0)
    })

    it('lists the repairs that can still be undone and reports what an undo left', async () => {
        const target = await setup(damaged)
        const entries = target.querySelector('[data-data-health-journals]')
        expect(entries?.textContent).toContain('2')
        const undo = [...target.querySelectorAll('button')].find(
            (button) => button.textContent?.trim() === strings.undoAction,
        )
        undo?.click()
        await settle()
        expect(maintenance.undoNativeDataHealthRepair).toHaveBeenCalledWith('repair-1')
        expect(
            target.querySelector('[data-data-health-skipped]')?.textContent,
        ).toContain('characters:char-9')
    })

    it('says so when nothing can be fixed automatically', async () => {
        const target = await setup(damaged, {}, [])
        expect(
            target.querySelector('[data-data-health-repair]')?.textContent,
        ).toContain(strings.repairNone)
    })

    it('keeps every string it shows in both shipped languages', () => {
        for (const key of Object.keys(strings)) {
            expect(
                languageKorean.risuNest.dataHealth[
                    key as keyof typeof strings
                ],
                key,
            ).toBeTruthy()
        }
    })
})

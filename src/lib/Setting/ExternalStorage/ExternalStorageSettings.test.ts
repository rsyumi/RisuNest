// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const state = vi.hoisted(() => ({
    getState: vi.fn(),
    getQuota: vi.fn(),
    setRetentionPolicy: vi.fn(),
    listHistory: vi.fn(),
    listConflicts: vi.fn(),
    recheckConflict: vi.fn(),
    deleteConflict: vi.fn(),
    beginConnectionSettingsExport: vi.fn(),
    saveConnectionSettingsFile: vi.fn(),
}))

vi.mock('src/ts/platform', () => ({ isTauri: true, isTauriAndroid: false, isTauriIOS: false }))
vi.mock('@tauri-apps/plugin-os', () => ({ type: () => 'windows' }))
vi.mock('@tauri-apps/plugin-opener', () => ({ openUrl: vi.fn() }))
vi.mock('qrcode', () => ({ default: { toDataURL: vi.fn() } }))
vi.mock('src/ts/alert', () => ({ alertConfirm: vi.fn(), alertNormal: vi.fn() }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: { db: { language: 'en' } } }))
vi.mock('src/ts/storage/sync/external/production', () => ({
    refreshExternalStorageProductionState: vi.fn(),
    requestExternalStorageNow: vi.fn(),
    requestExternalConflictExport: vi.fn(),
    requestExternalConflictRestore: vi.fn(),
    requestExternalStorageResolveConflict: vi.fn(),
    requestExternalStorageRestore: vi.fn(),
}))
vi.mock('src/ts/storage/sync/external/bridge', () => ({
    getExternalStorageBridge: () => ({
        getState: state.getState,
        getQuota: state.getQuota,
        setRetentionPolicy: state.setRetentionPolicy,
        listHistory: state.listHistory,
        listConflicts: state.listConflicts,
        recheckConflict: state.recheckConflict,
        deleteConflict: state.deleteConflict,
        beginConnectionSettingsExport: state.beginConnectionSettingsExport,
        saveConnectionSettingsFile: state.saveConnectionSettingsFile,
    }),
}))

import ExternalStorageSettings from './ExternalStorageSettings.svelte'
import { requestExternalStorageNow } from 'src/ts/storage/sync/external/production'
import { externalStorageStrings } from './strings'

const strings = externalStorageStrings('en')
let target: HTMLDivElement
let component: ReturnType<typeof mount> | undefined

function connection(keepCount: number, keepDays: number) {
    return {
        id: 'connection-1',
        providerId: 'webdav' as const,
        purpose: 'backup' as const,
        strategy: 'backup-only' as const,
        mode: 'existing' as const,
        displayName: 'Synthetic',
        endpoint: {
            providerId: 'webdav' as const,
            authority: 'https://synthetic.invalid',
            repositoryHint: 'RisuNest',
            warnings: [],
            remoteVerified: true,
        },
        retentionPolicy: { keepCount, keepDays },
        capabilities: {
            immutableCreate: true, directCompleteRead: true, atomicCreateHead: false,
            conditionalHeadUpdate: false, stableHeadReplace: false, headReadAfterWrite: false,
            headRetryControl: false, leaseOperations: false, deleteObjects: false,
            conditionalGet: false, resumableUpload: false, range: false,
            snapshotDiscovery: true, maxStoredBytes: null, sdkOverheadBytes: 0, uploadAlignment: 1,
        },
        status: 'ready' as const,
    }
}

function labelled(text: string): HTMLInputElement {
    const label = [...target.querySelectorAll('label')].find(item => item.textContent?.includes(text))
    const control = label?.querySelector('input')
    if (!control) throw new Error(`Missing control: ${text}`)
    return control
}

async function openStorageUsage(): Promise<void> {
    const tab = [...target.querySelectorAll('button')].find(item => item.textContent?.trim() === strings.quota)
    if (!tab) throw new Error('Missing the storage usage tab')
    tab.click()
    await tick()
    await Promise.resolve()
    await tick()
}

async function settle(): Promise<void> {
    for (let step = 0; step < 4; step += 1) {
        await Promise.resolve()
        await tick()
    }
}

describe('the storage usage tab', () => {
    beforeEach(async () => {
        state.getState.mockResolvedValue({
            supported: true,
            selection: { kind: 'none', selectionEpoch: '0', paused: false, decisionRequired: false },
            connections: [connection(10, 30)],
            jobs: [],
        })
        // The rows have to stand on their own: usage is a separate request.
        state.getQuota.mockRejectedValue({ kind: 'transient', httpStatus: null, retryAtMs: null })
        state.setRetentionPolicy.mockResolvedValue(undefined)
        state.listHistory.mockResolvedValue({ items: [] })
        state.beginConnectionSettingsExport.mockResolvedValue({
            transferId: 'settings-transfer',
            expiresAtMs: '1000',
            qrPayload: null,
        })
        state.saveConnectionSettingsFile.mockResolvedValue(undefined)
        target = document.createElement('div')
        document.body.append(target)
        component = mount(ExternalStorageSettings, { target })
        await settle()
    })

    afterEach(() => {
        if (component) unmount(component)
        component = undefined
        target.remove()
        vi.clearAllMocks()
    })

    it('runs manual cleanup through the production queue only for a capable connection', async () => {
        await openStorageUsage()
        expect([...target.querySelectorAll('button')].some(button => button.textContent?.trim() === strings.cleanup)).toBe(false)
        if (component) unmount(component)
        const capable = connection(10, 30)
        capable.capabilities.leaseOperations = true
        capable.capabilities.deleteObjects = true
        state.getState.mockResolvedValue({ supported: true, selection: { kind: 'none', selectionEpoch: '0', paused: false, decisionRequired: false }, connections: [capable], jobs: [] })
        component = mount(ExternalStorageSettings, { target })
        await settle()
        await openStorageUsage()
        const button = [...target.querySelectorAll('button')].find(button => button.textContent?.trim() === strings.cleanup)
        expect(button).toBeDefined()
        button!.click()
        await settle()
        expect(requestExternalStorageNow).toHaveBeenCalledWith('connection-1', 'cleanup')
    })

    it('shows the stored limits even when the usage request fails', async () => {
        await openStorageUsage()
        expect(labelled(strings.retentionCount).value).toBe('10')
        expect(labelled(strings.retentionDays).value).toBe('30')
    })

    it('exports connection settings without offering recovery-key replacement', async () => {
        const button = [...target.querySelectorAll('button')].find(
            item => item.textContent?.trim() === strings.connectionSettings,
        )
        expect(button).toBeDefined()
        button?.click()
        await settle()

        expect(state.beginConnectionSettingsExport).toHaveBeenCalledWith('connection-1')
        expect(target.getAttribute('role')).not.toBe('dialog')
        expect(target.textContent).toContain(strings.connectionSettingsNotice)
        expect(target.textContent).not.toMatch(/reissue|rotate/i)

        const save = [...target.querySelectorAll('button')].find(
            item => item.textContent?.trim() === strings.saveConnectionSettings,
        )
        save?.click()
        await settle()
        expect(state.saveConnectionSettingsFile).toHaveBeenCalledWith('settings-transfer')
    })

    it('does not render provider-supplied prose from ordinary history rows', async () => {
        state.listHistory.mockResolvedValue({
            items: [{
                id: 'snapshot', snapshotId: 'snapshot', kind: 'backup-point',
                createdAtMs: '1', logicalRevision: 1, storedBytes: '1', pinned: false,
                complete: true, verified: true, includedSections: [], sameDevice: true,
                warning: 'UNTRUSTED REMOTE WARNING',
            }],
        })
        const tab = [...target.querySelectorAll('button')].find(
            item => item.textContent?.trim() === strings.history,
        )
        tab?.click()
        await settle()

        expect(target.textContent).not.toContain('UNTRUSTED REMOTE WARNING')
        expect(target.textContent).toContain(strings.historyKinds['backup-point'])
    })

    it('rechecks an unconfirmed remote point before offering destructive conflict choices', async () => {
        const conflict = {
            id: 'conflict-1',
            connectionId: 'connection-1',
            detectedAtMs: '1',
            localRevision: '2',
            remoteRevision: '3',
            localAvailable: true,
            remoteAvailable: true,
            remotePointConfirmed: false,
            resolved: false,
        }
        state.listConflicts.mockResolvedValue({ conflicts: [conflict] })
        state.recheckConflict.mockResolvedValue({
            ...conflict,
            remotePointConfirmed: true,
        })
        const tab = [...target.querySelectorAll('button')].find(
            item => item.textContent?.trim() === strings.conflicts,
        )
        tab?.click()
        await settle()

        const action = [...target.querySelectorAll('button')].find(
            item => item.textContent?.trim() === strings.retryPreservation,
        )
        expect(action).toBeDefined()
        expect(target.textContent).not.toContain(strings.local)
        action?.click()
        await settle()

        expect(state.recheckConflict).toHaveBeenCalledWith('conflict-1')
        expect(target.textContent).toContain(strings.local)
        expect(target.textContent).toContain(strings.remote)
        expect(target.textContent).toContain(strings.exportLocalCopy)
        expect(target.textContent).toContain(strings.exportRemoteCopy)
    })

    it.each(['cas', 'sequential'])('shows the same single-device guidance for %s synchronization', async (strategy) => {
        if (component) unmount(component)
        state.getState.mockResolvedValue({
            supported: true,
            selection: { kind: 'none', selectionEpoch: '0', paused: false, decisionRequired: false },
            connections: [{ ...connection(10, 30), purpose: 'sync', strategy }],
            jobs: [],
        })
        component = mount(ExternalStorageSettings, { target })
        await settle()
        expect(target.textContent).toContain(strings.sequentialWarning)
        expect(target.querySelector('.card-sub')?.textContent).toBe('https://synthetic.invalid · RisuNest')
        expect(target.textContent).not.toContain('Concurrent-use protection')
    })

    it('sends a changed limit and leaves the other one alone', async () => {
        await openStorageUsage()
        const count = labelled(strings.retentionCount)
        count.value = '25'
        count.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        expect(state.setRetentionPolicy).toHaveBeenCalledWith('connection-1', {
            keepCount: 25,
            keepDays: 30,
        })
    })

    it.each([false, true])('shows local request estimates only when provided (%s)', async (hasCounters) => {
        state.getQuota.mockResolvedValue({
            connectionId: 'connection-1',
            buckets: hasCounters ? [{
                id: 'mybox-download-day', used: '4', limit: '500',
                unit: 'requests', localEstimate: true,
            }] : [],
            storage: {
                providerPhysicalBytes: null, providerPhysicalKnown: false,
                locallyUploadedBytesLowerBound: '0', locallyUploadedObjectCountLowerBound: '0',
                locallyUploadedCoverage: 'cached-upload-receipts',
            },
        })
        await openStorageUsage()
        await settle()
        expect(target.textContent?.includes(strings.requestUsage)).toBe(hasCounters)
        expect(target.textContent?.includes('4 / 500 requests')).toBe(hasCounters)
        expect(target.textContent).not.toContain('496')
    })

    it('puts back the stored value when the typed one is out of range', async () => {
        await openStorageUsage()
        const days = labelled(strings.retentionDays)
        days.value = '3'
        days.dispatchEvent(new Event('change', { bubbles: true }))
        await settle()
        expect(state.setRetentionPolicy).not.toHaveBeenCalled()
        expect(days.value).toBe('30')
    })
})

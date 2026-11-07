import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { language } from 'src/lang'
import { nativeFileOperationOutcome, waitForActiveNativeFileOperation } from 'src/ts/storage/nativeFileJobManager'
import NativeFileJobDialog from './NativeFileJobDialog.svelte'
import { syntheticNativeFileJobStatus } from 'src/ts/storage/nativeFileJobs'
import { failureReason } from 'src/ts/gui/nativeFileJobDialogModel'

import { get } from 'svelte/store'
import type { OfficialAccountAssetOutcome } from 'src/ts/storage/sync/officialAccountAssetOutcome'

const h = vi.hoisted(() => ({ alertError: vi.fn<typeof import('src/ts/alert')['alertError']>(), busy: true }))
vi.mock('src/ts/alert', () => ({ alertError: h.alertError }))
vi.mock('src/ts/storage/sync/serverSyncProduction', () => ({
    getServerSyncController: () => ({ assertFileOperationAvailable() { if (h.busy) throw Object.assign(new Error('server-sync-busy'), { code: 'server-sync-busy' }) } }),
}))

let resetFlow: (() => void) | undefined
let component: ReturnType<typeof mount> | undefined
let target: HTMLDivElement | undefined

afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    target?.remove()
    nativeFileOperationOutcome.set(null)
    h.alertError.mockClear()
    h.busy = true
    resetFlow?.()
    resetFlow = undefined
    vi.restoreAllMocks()
})

describe('file operation error details', () => {
    it('offers account asset retry only while the restored library and account still match', async () => {
        target = document.createElement('div')
        document.body.append(target)
        component = mount(NativeFileJobDialog, { target })
        for (const reason of ['service', 'account-changed', 'library-changed'] as const) {
            const status = syntheticNativeFileJobStatus({ kind: 'restore-official-account-snapshot' }, 'finishing-account-restore')
            status.officialAssets = { operationId: 'synthetic-operation', phase: 'finishing', state: 'failed', reason,
                counts: { required: 1, alreadyPresent: 0, installed: 0, missing: 0, failed: 1, notAttempted: 0 } }
            nativeFileOperationOutcome.set({ kind: 'import', format: 'library-backup', state: 'failed', startedAt: 0, finishedAt: 1,
                status, observedStages: ['downloading-account-assets'], warningCodes: [], partialWritesPossible: true })
            await tick()
            const retry = [...target.querySelectorAll('button')].find(button => button.textContent?.trim() === language.retry)
            expect(!!retry).toBe(reason === 'service')
            if (reason === 'service') {
                retry!.click()
                await vi.waitFor(() => expect(h.alertError).toHaveBeenCalledWith(failureReason('server-sync-busy')))
            }
        }
    })
    it.each(['cancelled', 'failed'] as const)('keeps an admitted account retry %s result visible without a second alert', async ending => {
        h.busy = false
        const { configureNativeOfficialAccountFlow, createNativeOfficialAccountFlow } = await import('src/ts/storage/sync/nativeOfficialAccountFlow')
        resetFlow = () => configureNativeOfficialAccountFlow(null)
        const operationId = 'synthetic-retry'
        let finishTransfer: (() => void) | undefined
        configureNativeOfficialAccountFlow(createNativeOfficialAccountFlow({
            initialCredential: { id: 'synthetic-account', token: 'synthetic-token', data: {} },
            credentialVault: { read: async () => null, write: async () => {}, clear: async () => {} },
            adapter: { pull: async () => { throw new Error('unexpected database restore') }, pin: async () => { throw new Error('unexpected publication') }, resetAccountAssociation() {} },
            flushPendingData: async () => {}, getRevision: () => 7, restart: async () => { throw new Error('unexpected restart') },
            setRouting() {}, clearLegacyFallback() {}, flushMetadata: async () => {}, clearMetadata: async () => {}, resetAccountSession() {},
            completeRestoreAssets: async (_accountId, options) => {
                const progress: OfficialAccountAssetOutcome = { operationId, phase: 'downloading', state: 'running', failedKeys: [],
                    counts: { required: 1, alreadyPresent: 0, installed: 0, missing: 0, failed: 0, notAttempted: 1 } }
                options.onProgress?.(progress)
                await new Promise<void>(resolve => {
                    finishTransfer = resolve
                    options.signal!.addEventListener('abort', () => resolve(), { once: true })
                })
                progress.phase = 'finishing'
                progress.state = ending
                if (ending === 'failed') {
                    progress.reason = 'files'; progress.counts.failed = 1; progress.counts.notAttempted = 0
                    progress.failedKeys = ['assets/avatar']
                }
                options.onProgress?.(progress)
                return progress
            },
            stopRestoreAssets: async () => {},
        }))
        const status = syntheticNativeFileJobStatus({ kind: 'restore-official-account-snapshot', activationRevision: 7 }, 'finishing-account-restore')
        status.officialAssets = { operationId, phase: 'finishing', state: 'cancelled',
            counts: { required: 1, alreadyPresent: 0, installed: 0, missing: 0, failed: 0, notAttempted: 1 } }
        const previous = { kind: 'import' as const, format: 'library-backup' as const, state: 'cancelled' as const,
            startedAt: 0, finishedAt: 1, status, observedStages: [], warningCodes: [], partialWritesPossible: true }
        nativeFileOperationOutcome.set(previous)
        target = document.createElement('div'); document.body.append(target)
        component = mount(NativeFileJobDialog, { target })
        await tick()
        const button = (text: string) => [...target!.querySelectorAll('button')].find(item => item.textContent?.trim() === text)!
        button(language.retry).click()
        await vi.waitFor(() => expect(finishTransfer).toBeDefined())
        if (ending === 'cancelled') button(language.risuNest.importDialog.stopAccountAssets).click()
        else finishTransfer!()
        await waitForActiveNativeFileOperation()
        await vi.waitFor(() => {
            expect(get(nativeFileOperationOutcome)).not.toBe(previous)
            expect(get(nativeFileOperationOutcome)?.state).toBe(ending)
            expect(target!.textContent).toContain(ending === 'cancelled' ? language.risuNest.importDialog.resultAccountAssetsStopped : language.risuNest.importDialog.resultAccountAssetsIncomplete)
        })
        await vi.dynamicImportSettled()
        expect(h.alertError).not.toHaveBeenCalled()
    })

    it.each([true, false])('shows and copies diagnostics with raw error available: %s', async rawError => {
        const writeText = vi.fn(async () => {})
        Object.defineProperty(navigator, 'clipboard', { configurable: true, value: { writeText } })
        nativeFileOperationOutcome.set({
            kind: 'import', format: 'content', state: 'failed', startedAt: 0, finishedAt: 1,
            observedStages: [], warningCodes: [], partialWritesPossible: false,
            error: rawError ? { code: 'source-read-failed', message: 'synthetic file failure', recoveryRequired: false } : undefined,
        })
        target = document.createElement('div')
        document.body.append(target)
        component = mount(NativeFileJobDialog, { target })
        await tick()
        const show = [...target.querySelectorAll('button')].find(button => button.textContent?.trim() === language.risuNest.importDialog.errorDetails)
        expect(show).toBeDefined()
        show!.click()
        await tick()
        const report = target.querySelector('pre')!.textContent!
        expect(report).toContain('RisuNest Version:')
        expect(report).toContain('\nDevice:')
        expect(report).toContain('\nUser-Agent:')
        expect(report.split('\n\n')[1]).toBeTruthy()
        if (rawError) expect(report).toContain('[source-read-failed] synthetic file failure')
        target.querySelector<HTMLButtonElement>(`button[aria-label="${language.copy}"]`)!.click()
        await vi.waitFor(() => expect(writeText).toHaveBeenCalledWith(report))
    })

    it('lists each file of a batch that was not imported, with its reason when it has one', async () => {
        const io = { code: 'source-read-failed', message: 'synthetic file failure', recoveryRequired: false }
        const refused = { code: 'unsupported-source', message: 'synthetic refusal', recoveryRequired: false }
        nativeFileOperationOutcome.set({
            kind: 'import', format: 'content', state: 'failed', startedAt: 0, finishedAt: 1,
            observedStages: [], warningCodes: [], partialWritesPossible: false, error: io,
            batch: {
                total: 4,
                imported: 1,
                failures: [{ name: 'first.png', error: io }, { name: 'program.exe', error: refused }],
                declined: ['locked.png'],
            },
        })
        target = document.createElement('div')
        document.body.append(target)
        component = mount(NativeFileJobDialog, { target })
        await tick()
        const rows = [...target.querySelectorAll('[data-testid="native-file-job-failed-files"] li')]
            .map(row => [...row.querySelectorAll('span')].map(span => span.textContent))
        expect(rows).toEqual([
            ['first.png', failureReason('source-read-failed', 'import')],
            ['program.exe', failureReason('unsupported-source', 'import')],
            ['locked.png'],
        ])
    })
})

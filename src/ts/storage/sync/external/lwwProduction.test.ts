// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { trackAbortListeners, withoutAbortSignalAny } from '../../../../../tests/support/abortSignalAny'
import type { BindingContext, SyncBindingTransport } from '../bindingFlow'
import type { ExternalStorageState } from './types'
const fixture = vi.hoisted(() => ({
    native: true, os: 'windows' as 'windows' | 'android' | 'ios',
    invoke: vi.fn(), listen: vi.fn(), state: vi.fn(), apply: vi.fn(), flush: vi.fn(), outbox: vi.fn(), storageMutation: vi.fn(), resumeCurrent: vi.fn(), download: vi.fn(),
    registrations: new Map<string, SyncBindingTransport>(), viewport: undefined as undefined | ((source: unknown) => void),
    revision: undefined as undefined | ((revision: number, cause: string) => void),
    diagnostic: vi.fn(async (_command: string, _args: Record<string, unknown>): Promise<unknown> => 7),
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke: fixture.invoke, Channel: class { onmessage = (_value: unknown) => {} } }))
vi.mock('@tauri-apps/api/event', () => ({ listen: fixture.listen }))
vi.mock('../../../platform', async () => (await import('../../../platform.testSupport')).platformModule(() => ({ runtime: fixture.native ? 'tauri' : 'web', os: fixture.os })))
vi.mock('../../../mobileBackgroundTask', async () => (await import('../../../mobileBackgroundTask.testSupport')).mobileBackgroundTaskModule())
vi.mock('../../../generationDiagnostics', async (importOriginal) => (await import('../../../process/tests/sendChatTestHarness'))
    .generationDiagnosticsModule(await importOriginal<typeof import('../../../generationDiagnostics')>(), fixture.diagnostic))
vi.mock('../../generatingConversationRegistry', () => ({ generatingConversations: { snapshot: () => [] } }))
vi.mock('../../persistentDataRuntime.svelte', () => ({
    flushPendingDataLocally: fixture.flush,
    getPersistentDataRuntime: () => ({ store: { lwwReadOutbox: fixture.outbox }, runStorageOnlyMutation: fixture.storageMutation, applyLwwReceive: fixture.apply, subscribeActiveConversationViewportSource(callback: (source: unknown) => void) { fixture.viewport = callback; return () => {} }, captureSelectedConversationTarget: () => ({ characterId: 'char', conversationId: 'conversation' }) }),
}))
vi.mock('../../persistentRevisionEvents', () => ({ subscribeLocalPersistentRevision(callback: (revision: number, cause: string) => void) { fixture.revision = callback; return () => {} } }))
vi.mock('../bindingNative', () => ({ createNativeSyncBindingBridge: () => ({ state: fixture.state }), replaceNativeSyncBinding: vi.fn(), replaceNativeSyncBindingAsNewDevice: vi.fn() }))
vi.mock('../serverAssetResidency', () => ({ downloadRemoteAssets: fixture.download }))
vi.mock('../bindingRegistry', () => ({ resumeCurrentSyncBinding: fixture.resumeCurrent, registerSyncBindingTransport(target: { connectionId: string }, transport: SyncBindingTransport) { fixture.registrations.set(target.connectionId, transport); return () => fixture.registrations.delete(target.connectionId) } }))
import { BackgroundExecutionExpiredError } from '../../../iosNative'
import { beginDeferredMobileBackgroundTask, runWithMobileBackgroundTask, type MobileBackgroundTask } from '../../../mobileBackgroundTask'
import { installExternalLwwAdapters, refreshExternalLwwAdapters, externalLwwExitDrain, requestExternalLwwNow, subscribeExternalLwwFailures, switchExternalLwwLibraryFormat } from './lwwProduction'
import { externalProgressFor, subscribeExternalProgress, type ExternalOperationProgress } from './progress'
const binding = { target: { kind: 'external' as const, connectionId: 'sync' }, targetAuthority: '4', selectionEpoch: 'selection', libraryId: 'library', progress: [] }
const context = (): BindingContext => ({ state: binding, signal: new AbortController().signal })
const state = (providers = ['webdav']): ExternalStorageState => ({ connections: providers.map((providerId, i) => ({ id: i ? providerId : 'sync', providerId, purpose: 'sync' })) } as ExternalStorageState)
let dispose: (() => void) | undefined
withoutAbortSignalAny()
beforeEach(() => {
    vi.useFakeTimers(); fixture.native = true; fixture.os = 'windows'; fixture.registrations.clear()
    fixture.invoke.mockReset(); fixture.invoke.mockImplementation(async command => command === 'external_lww_receive' ? null : command === 'external_lww_publish' ? { segments: '0', more: false } : undefined)
    fixture.state.mockResolvedValue(binding); fixture.listen.mockResolvedValue(() => {})
    fixture.resumeCurrent.mockReset(); fixture.resumeCurrent.mockImplementation(async target => { const current = await fixture.state(); await fixture.registrations.get(target.connectionId)!.resumeBinding({ state: current, signal: new AbortController().signal }) })
    fixture.apply.mockReset(); fixture.flush.mockReset(); vi.mocked(runWithMobileBackgroundTask).mockReset(); vi.mocked(beginDeferredMobileBackgroundTask).mockReset()
    fixture.download.mockReset(); fixture.download.mockResolvedValue(undefined)
    fixture.outbox.mockReset(); fixture.outbox.mockResolvedValue({ revision: 12, entries: [] })
    fixture.storageMutation.mockReset(); fixture.storageMutation.mockImplementation(async work => { await work(12) })
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' })
    Object.defineProperty(navigator, 'onLine', { configurable: true, value: true })
})
afterEach(() => { dispose?.(); dispose = undefined; vi.useRealTimers() })
const settle = async () => { for (let i = 0; i < 20; i++) await Promise.resolve() }
const assetPage = (requestId = 'asset-page') => ({ header: { bindingAuthority: '4', requestId }, changes: [{ key: '["asset","synthetic-image"]', value: { kind: 'inline', value: { objectHash: 'synthetic-hash' } } }] })
describe('external sync asset downloads', () => {
    it('waits for external assets before file work and keeps them stopped across picker visibility changes', async () => {
        const admission = await import('../../libraryFileOperation')
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const release = admission.reserveLibraryFileOperation()
        const ready = vi.fn()
        const waiting = Promise.resolve(admission.waitForLibraryFileOperation(new AbortController().signal)).then(ready)
        try {
            await settle(); expect(ready).not.toHaveBeenCalled()
            finish(); await waiting
            fixture.download.mockClear(); fixture.invoke.mockClear()
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange'))
            await settle()
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
            fixture.revision!(15, 'generation-complete')
            await vi.advanceTimersByTimeAsync(65_000); await settle()
            expect(fixture.download).not.toHaveBeenCalled()
            expect(fixture.invoke.mock.calls.some(([command]) => ['external_lww_publish', 'external_lww_receive'].includes(command))).toBe(false)
        } finally { finish(); release(); await settle() }
        expect(fixture.download).toHaveBeenCalledOnce()
    })
    it('downloads missing bodies on resume without delaying metadata sync', async () => {
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state(['google_drive'])); await settle()
        expect(fixture.download).toHaveBeenCalledExactlyOnceWith(undefined, { progressConnectionId: 'sync', signal: expect.any(AbortSignal), showEmpty: false, routine: true })
        await requestExternalLwwNow('sync')
        expect(fixture.invoke.mock.calls.map(call => call[0])).toContain('external_lww_receive')
        expect(fixture.download).toHaveBeenCalledOnce()
        finish(); await settle()
    })
    it('leaves downloads stopped when sync is off and when the app starts hidden', async () => {
        dispose = await installExternalLwwAdapters(state(), false); await settle()
        expect(fixture.download).not.toHaveBeenCalled()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' })
        await fixture.registrations.get('sync')!.resumeBinding(context()); await settle()
        expect(fixture.download).not.toHaveBeenCalled()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
        await settle(); await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
    })
    it('downloads new references only after application and coalesces changes received during a download', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.download.mockClear()
        let apply!: () => void; let finish!: () => void
        fixture.apply.mockImplementationOnce(() => new Promise<void>(resolve => { apply = resolve }))
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        fixture.invoke.mockResolvedValueOnce({ segments: '0', more: false }).mockResolvedValueOnce(assetPage())
        const first = requestExternalLwwNow('sync'); await settle()
        expect(fixture.download).not.toHaveBeenCalled()
        apply(); await first; await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
        for (let i = 0; i < 2; i++) {
            fixture.invoke.mockResolvedValueOnce({ segments: '0', more: false }).mockResolvedValueOnce(assetPage(String(i)))
            await requestExternalLwwNow('sync')
        }
        expect(fixture.download).toHaveBeenCalledOnce()
        finish(); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
    it('does not scan all assets for empty listings or ordinary chat edits', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.download.mockClear()
        await requestExternalLwwNow('sync')
        fixture.invoke.mockResolvedValueOnce({ segments: '0', more: false }).mockResolvedValueOnce({ ...assetPage(), changes: [{ key: '["messages","char","chat"]', value: { kind: 'object', descriptor: { dependencies: ['message-body'] } } }] })
        await requestExternalLwwNow('sync'); await settle()
        expect(fixture.download).not.toHaveBeenCalled()
    })
    it.each([
        { key: '["inlay","synthetic-inlay"]', value: { kind: 'inline', value: { objectHash: 'inlay-hash' } } },
        { key: '["module","synthetic-module"]', value: { kind: 'object', descriptor: { dependencies: ['module-image'] } } },
        { key: '["character","synthetic-character"]', value: { kind: 'object', descriptor: { dependencyRoot: 'dependency-root' } } },
    ])('downloads bodies referenced by $key', async change => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.download.mockClear()
        fixture.invoke.mockResolvedValueOnce({ segments: '0', more: false }).mockResolvedValueOnce({ ...assetPage(), changes: [change] })
        await requestExternalLwwNow('sync'); await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
    })
    it.each([
        { code: 'download-unavailable' },
        { code: 'asset-jobs-busy', status: 409, retryable: true },
    ])('retains $code across successful metadata sync and retries pending bodies', async failure => {
        fixture.download.mockRejectedValueOnce(failure)
        dispose = await installExternalLwwAdapters(state()); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        expect(failures.get('sync')).toEqual(failure)
        fixture.revision!(15, 'generation-complete'); await settle()
        expect(failures.get('sync')).toEqual(failure)
        expect(fixture.download).toHaveBeenCalledOnce()
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('respects a service retry deadline even when Sync Now is requested', async () => {
        fixture.download.mockRejectedValueOnce({ kind: 'rateLimited', retryAtMs: String(Date.now() + 120_000) })
        dispose = await installExternalLwwAdapters(state()); await settle()
        await requestExternalLwwNow('sync'); await vi.advanceTimersByTimeAsync(119_999)
        expect(fixture.download).toHaveBeenCalledOnce()
        await vi.advanceTimersByTimeAsync(1); await requestExternalLwwNow('sync'); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
    it.each([
        { kind: 'localStorageFull', retryable: false },
        { code: 'asset-jobs-unresolved', status: 409, retryable: false },
        { code: 'external-reauth-required', status: 502, retryable: true },
    ])('retries a blocked download only after recovery or an explicit request: %j', async failure => {
        fixture.download.mockRejectedValueOnce(failure)
        dispose = await installExternalLwwAdapters(state()); await settle()
        await vi.advanceTimersByTimeAsync(120_000)
        expect(fixture.download).toHaveBeenCalledOnce()
        await requestExternalLwwNow('sync'); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
    it.each([
        ['sign-in', 'signs in again', 'external-reauth-required', '2'],
        ['sign-in', 'is removed', 'external-reauth-required', undefined],
        ['recovery key', 'takes its recovery key', 'external-key-locked', '2'],
        ['recovery key', 'is removed', 'external-key-locked', undefined],
    ] as const)('keeps a download stopped on another storage %s until that storage %s', async (_, __, code, renewed) => {
        const storages = (sync: string, other: string | undefined) => ({ connections: [
            { id: 'sync', providerId: 'webdav', purpose: 'sync', status: 'ready', lastVerifiedAtMs: sync },
            ...other === undefined ? [] : [{ id: 'drive-2', providerId: 'google_drive', purpose: 'backup', status: 'ready', lastVerifiedAtMs: other }],
        ] } as unknown as ExternalStorageState)
        const refused = { code, status: 502, retryable: true, connectionId: 'drive-2' }
        fixture.download.mockRejectedValueOnce(refused)
        dispose = await installExternalLwwAdapters(storages('1', '1')); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        expect(failures.get('sync')).toEqual(refused)

        // Renewing the sync connection leaves the other storage's stop in place.
        await refreshExternalLwwAdapters(storages('2', '1')); await vi.advanceTimersByTimeAsync(120_000); await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
        expect(failures.get('sync')).toEqual(refused)

        await refreshExternalLwwAdapters(storages('2', renewed)); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('cancels and settles the old download before completing a binding fence', async () => {
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        let fenced = false
        const fence = fixture.registrations.get('sync')!.fenceOldJobs(context()).then(() => { fenced = true })
        await settle()
        expect(signal.aborted).toBe(true)
        expect(fenced).toBe(false)
        finish(); await fence
        await vi.advanceTimersByTimeAsync(120_000)
        expect(fixture.download).toHaveBeenCalledOnce()
    })
    it('cancels a download when offline and resumes without overlapping the cancelled worker', async () => {
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        Object.defineProperty(navigator, 'onLine', { configurable: true, value: false })
        window.dispatchEvent(new Event('offline')); await settle()
        expect(signal.aborted).toBe(true)
        Object.defineProperty(navigator, 'onLine', { configurable: true, value: true })
        window.dispatchEvent(new Event('online')); await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
        finish(); await settle(); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
    it('keeps a download running while hidden and starts no second one beside it on return', async () => {
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        expect(signal.aborted).toBe(false)
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange')); await settle(); await settle()
        expect(fixture.download).toHaveBeenCalledOnce()
        finish(); await settle(); await settle()
        expect(signal.aborted).toBe(false)
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
    it('cancels removal and ignores late download failures', async () => {
        let fail!: (error: unknown) => void
        fixture.download.mockImplementationOnce(() => new Promise<void>((_, reject) => { fail = reject }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        await refreshExternalLwwAdapters({ connections: [] } as unknown as ExternalStorageState)
        expect(signal.aborted).toBe(true)
        fail(new Error('late failure')); await settle(); await vi.advanceTimersByTimeAsync(120_000)
        expect(fixture.download).toHaveBeenCalledOnce()
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('cancels the old download when its binding lifetime ends', async () => {
        dispose = await installExternalLwwAdapters(state(), false)
        const controller = new AbortController()
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        await fixture.registrations.get('sync')!.resumeBinding({ state: binding, signal: controller.signal }); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        controller.abort()
        expect(signal.aborted).toBe(true)
        finish(); await settle(); await vi.advanceTimersByTimeAsync(120_000)
        expect(fixture.download).toHaveBeenCalledOnce()
    })
    it('settles asset cancellation before exit and resumes unfinished bodies if exit is cancelled', async () => {
        let finish!: () => void
        fixture.download.mockImplementationOnce(() => new Promise<void>(resolve => { finish = resolve }))
        dispose = await installExternalLwwAdapters(state()); await settle()
        const signal = fixture.download.mock.calls[0][1].signal as AbortSignal
        const drain = externalLwwExitDrain('sync', 'selection', true)!
        let drained = false
        const pending = drain.drain({ revision: 12, libraryEpoch: 'epoch', selectionEpoch: 'selection', selectionId: drain.id }, new AbortController().signal).then(result => { drained = true; return result })
        await settle()
        expect(signal.aborted).toBe(true)
        expect(drained).toBe(false)
        finish(); expect(await pending).toEqual({ kind: 'complete' })
        await drain.resumeAfterExitCancel!(); await settle()
        expect(fixture.download).toHaveBeenCalledTimes(2)
    })
})
describe('native external LWW adapter', () => {
    it('observes confirmation waiting within the binding attempt', async () => {
        dispose = await installExternalLwwAdapters(state(), false)
        let progress: ReadonlyMap<string, ExternalOperationProgress> = new Map()
        const stop = subscribeExternalProgress(value => progress = value)
        const transport = fixture.registrations.get('sync')!
        await transport.observeBinding!(async () => {
            transport.setConfirmationPending!(true)
            expect(externalProgressFor(progress, 'sync')).toMatchObject({ kind: 'binding', stage: 'waiting', state: 'running' })
            transport.setConfirmationPending!(false)
            expect(externalProgressFor(progress, 'sync')?.stage).toBe('checking')
            return { kind: 'cancelled' }
        })
        expect(externalProgressFor(progress, 'sync')?.state).toBe('cancelled')
        stop()
    })
    it('resumes a persisted external target through shared flow ownership exactly once for the current authority', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        expect(fixture.resumeCurrent).toHaveBeenCalledExactlyOnceWith(binding.target)
        await refreshExternalLwwAdapters(state()); await settle()
        expect(fixture.resumeCurrent).toHaveBeenCalledOnce()
    })
    it('registers the transports but leaves a bound target stopped when the start left sync off', async () => {
        dispose = await installExternalLwwAdapters(state(), false); await settle()
        expect(fixture.registrations.has('sync')).toBe(true)
        await refreshExternalLwwAdapters(state()); await vi.advanceTimersByTimeAsync(60_000); await settle()
        expect(fixture.resumeCurrent).not.toHaveBeenCalled()
        expect(fixture.invoke).not.toHaveBeenCalled()
        // Turning the switch on in settings resumes the target through the registered transport.
        await fixture.registrations.get('sync')!.resumeBinding(context()); await settle()
        expect(fixture.invoke.mock.calls.map(call => call[0])).toContain('external_lww_resume')
        dispose(); dispose = undefined
        dispose = await installExternalLwwAdapters(state()); await settle()
        expect(fixture.resumeCurrent).toHaveBeenCalledExactlyOnceWith(binding.target)
    })
    it('manual Sync Now resumes a bound target the start left stopped before it publishes', async () => {
        dispose = await installExternalLwwAdapters(state(), false); await settle()
        fixture.resumeCurrent.mockClear(); fixture.invoke.mockClear()
        await requestExternalLwwNow('sync')
        expect(fixture.resumeCurrent).toHaveBeenCalledExactlyOnceWith(binding.target)
        const commands = fixture.invoke.mock.calls.map(call => call[0])
        expect(commands.indexOf('external_lww_resume')).toBeGreaterThanOrEqual(0)
        expect(commands.indexOf('external_lww_resume')).toBeLessThan(commands.indexOf('external_lww_publish'))
    })
    it('resumes a stopped binding before it switches the library format', async () => {
        dispose = await installExternalLwwAdapters(state(), false); await settle()
        fixture.resumeCurrent.mockClear(); fixture.invoke.mockClear()
        await switchExternalLwwLibraryFormat('sync')
        expect(fixture.resumeCurrent).toHaveBeenCalledExactlyOnceWith(binding.target)
        const commands = fixture.invoke.mock.calls.map(call => call[0])
        expect(commands.indexOf('external_lww_resume')).toBeGreaterThanOrEqual(0)
        expect(commands.indexOf('external_lww_resume')).toBeLessThan(commands.indexOf('external_lww_switch_format'))
    })
    it('registers four eligible native providers and guards the web', async () => {
        dispose = await installExternalLwwAdapters(state(['webdav', 's3', 'google_drive', 'onedrive', 'mybox', 'github_releases', 'gitlab_packages']))
        expect([...fixture.registrations.keys()]).toEqual(['sync', 's3', 'google_drive', 'onedrive'])
        dispose(); dispose = undefined; fixture.native = false; fixture.invoke.mockClear(); fixture.state.mockClear()
        dispose = await installExternalLwwAdapters(state())
        expect(fixture.registrations.size).toBe(0); expect(fixture.invoke).not.toHaveBeenCalled(); expect(fixture.state).not.toHaveBeenCalled()
    })
    it('receives available changes through runtime admission and propagates failures', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const transport = fixture.registrations.get('sync') as SyncBindingTransport & { receiveAvailableChanges(context: BindingContext): Promise<void> }
        const request = { header: { bindingAuthority: '4', requestId: 'synthetic-receive' }, changes: [] }
        const next = { header: { bindingAuthority: '4', requestId: 'synthetic-receive-next' }, changes: [] }
        fixture.invoke.mockClear(); fixture.apply.mockClear()
        fixture.invoke.mockResolvedValueOnce(request).mockResolvedValueOnce(next); await transport.receiveAvailableChanges(context())
        // Every call asks native code to list again after a batch, so the final null means nothing is left.
        expect(fixture.invoke.mock.calls.map(([command, args]) => [command, args.request.completeAvailable])).toEqual(Array(3).fill(['external_lww_receive', true]))
        expect(fixture.apply.mock.calls).toEqual([[request], [next]])
        fixture.invoke.mockRejectedValueOnce({ kind: 'corrupt' })
        await expect(transport.receiveAvailableChanges(context())).rejects.toEqual({ kind: 'corrupt' })

        // A routine receive in flight keeps it from taking the same first page.
        let complete: (() => void) | undefined
        fixture.invoke.mockClear()
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive' && !complete) { await new Promise<void>(resolve => { complete = resolve }); return null }
            return command === 'external_lww_receive' ? null : command === 'external_lww_publish' ? { segments: '0', more: false } : undefined
        })
        const receives = () => fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_receive').length
        fixture.viewport!({}); await settle()
        expect(receives()).toBe(1)
        const startup = transport.receiveAvailableChanges(context()); await settle()
        expect(receives()).toBe(1)
        complete!(); await startup
        expect(receives()).toBe(2)
    })
    it('shows why sync stopped when a committed switch cannot resume', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        const transport = fixture.registrations.get('sync')!
        transport.reportStopped!(new DOMException('aborted', 'AbortError'))
        expect(failures.has('sync')).toBe(false)
        transport.reportStopped!({ kind: 'previousStorageUnavailable' })
        expect(failures.get('sync')).toEqual({ kind: 'previousStorageUnavailable' })
        transport.reportStopped!(new AggregateError([{ kind: 'corrupt' }], 'stopped'))
        expect(failures.get('sync')).toEqual({ kind: 'corrupt' })
        // A renewed connection clears only a sign-in stop, so this one stays named.
        await refreshExternalLwwAdapters({ connections: [{ id: 'sync', providerId: 'webdav', purpose: 'sync', lastVerifiedAtMs: '2' }] } as unknown as ExternalStorageState); await settle()
        expect(failures.get('sync')).toEqual({ kind: 'corrupt' })
        unsubscribe()
    })
    it('keeps checking after clock skew and stops only on corruption', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const receives = () => fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_receive').length
        fixture.invoke.mockImplementation(async command => { if (command === 'external_lww_receive') throw { kind: 'clockSkew' } })
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        const skewed = receives()
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        expect(receives()).toBe(skewed + 1)
        fixture.invoke.mockImplementation(async command => command === 'external_lww_receive' ? null : command === 'external_lww_publish' ? { segments: '0', more: false } : undefined)
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        expect(receives()).toBe(skewed + 2)
        fixture.invoke.mockImplementation(async command => { if (command === 'external_lww_receive') throw { kind: 'corrupt' } })
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        const corrupt = receives()
        await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(receives()).toBe(corrupt)
    })
    it('stops when the connection reaches another repository', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const receives = () => fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_receive').length
        fixture.invoke.mockImplementation(async command => { if (command === 'external_lww_receive') throw { kind: 'repositoryMismatch' } })
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        const refused = receives()
        expect(refused).toBeGreaterThan(0)
        await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(receives()).toBe(refused)
    })
    it.each([
        ['an expired sign-in', 'signs in again', { kind: 'reauthRequired' }],
        ['a missing recovery key', 'takes its recovery key', { kind: 'repositoryKeyUnavailable' }],
    ] as const)('stops on %s until the connection %s, then receives at once', async (_, __, refusal) => {
        const signedIn = (verifiedAtMs: string) => ({ connections: [{ id: 'sync', providerId: 'google_drive', purpose: 'sync', status: 'ready', lastVerifiedAtMs: verifiedAtMs }] } as unknown as ExternalStorageState)
        dispose = await installExternalLwwAdapters(signedIn('1')); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        const receives = () => fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_receive').length
        fixture.invoke.mockImplementation(async command => { if (command === 'external_lww_receive') throw refusal })
        await vi.advanceTimersByTimeAsync(20_000); await settle()
        const refused = receives()
        expect(failures.get('sync')).toEqual(refusal)
        // The sync failure leaves the connection unmarked, so reading the same state again changes nothing.
        await refreshExternalLwwAdapters(signedIn('1')); await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(receives()).toBe(refused)

        let release: (() => void) | undefined
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') { await new Promise<void>(resolve => { release = resolve }); return null }
            return command === 'external_lww_publish' ? { segments: '0', more: false } : undefined
        })
        await refreshExternalLwwAdapters(signedIn('2')); await vi.advanceTimersByTimeAsync(1_000); await settle()
        // The stop clears with the new sign-in or key instead of after the receive it starts.
        expect(receives()).toBe(refused + 1)
        expect(failures.has('sync')).toBe(false)
        fixture.invoke.mockImplementation(async command => command === 'external_lww_receive' ? null : command === 'external_lww_publish' ? { segments: '0', more: false } : undefined)
        release!(); await settle()
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('rechecks a library format stop natively on return to the screen and switches the library on request', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        const stop = { kind: 'libraryFormatSwitchRequired' }
        let switched = false
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_switch_format') { switched = true; return }
            if (!switched && ['external_lww_receive', 'external_lww_publish', 'external_lww_check_format'].includes(command)) throw stop
            return command === 'external_lww_receive' ? null : command === 'external_lww_publish' ? { segments: '0', more: false } : undefined
        })
        const checks = () => fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_check_format')
        fixture.viewport!({}); await settle()
        expect(failures.get('sync')).toEqual(stop)
        // A renewed connection leaves the stop and its switch in place instead of clearing them until the next check.
        const renewed = refreshExternalLwwAdapters({ connections: [{ id: 'sync', providerId: 'webdav', purpose: 'sync', lastVerifiedAtMs: '2' }] } as unknown as ExternalStorageState)
        expect(failures.get('sync')).toEqual(stop)
        await renewed; await settle()
        await vi.advanceTimersByTimeAsync(120_000); await settle()
        expect(checks()).toHaveLength(0)
        // A body download that fails meanwhile does not hide the stop and its switch.
        fixture.download.mockClear(); fixture.download.mockRejectedValueOnce({ code: 'download-unavailable' })
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange')); await settle(); await settle()
        expect(checks()).toEqual([['external_lww_check_format', { request: expect.objectContaining({ connectionId: 'sync', bindingAuthority: '4' }) }]])
        expect(fixture.download).toHaveBeenCalledOnce()
        expect(failures.get('sync')).toEqual(stop)
        await switchExternalLwwLibraryFormat('sync')
        expect(fixture.invoke).toHaveBeenCalledWith('external_lww_switch_format', { request: expect.objectContaining({ connectionId: 'sync', bindingAuthority: '4' }), progress: expect.anything() })
        fixture.invoke.mockClear()
        await requestExternalLwwNow('sync'); await settle()
        expect(fixture.invoke.mock.calls.map(call => call[0])).toEqual(['external_lww_check_format', 'external_lww_publish', 'external_lww_receive'])
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('publishes the actual generation-complete revision cause immediately over a pending ordinary debounce', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        fixture.invoke.mockClear(); vi.mocked(runWithMobileBackgroundTask).mockClear(); fixture.flush.mockClear()
        fixture.revision!(12, 'edit')
        await vi.advanceTimersByTimeAsync(1_000)
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_publish')).toHaveLength(0)
        const completedAt = Date.now()
        fixture.revision!(13, 'generation-complete'); await settle()
        expect(Date.now()).toBe(completedAt)
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_publish')).toHaveLength(1)
        expect(fixture.flush).toHaveBeenCalledWith('external-lww-publish')
        expect(runWithMobileBackgroundTask).not.toHaveBeenCalled()
        await vi.advanceTimersByTimeAsync(15_000); await settle()
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_publish')).toHaveLength(1)
    })
    it.each(['on screen', 'hidden and shown as it begins'] as const)('flushes once on background entry after a start %s and suspends all timed work', async start => {
        if (start === 'on screen') dispose = await installExternalLwwAdapters(state())
        else {
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' })
            let listened!: () => void
            fixture.listen.mockImplementationOnce(() => new Promise(resolve => { listened = () => resolve(() => {}) }))
            const installing = installExternalLwwAdapters(state()).then(value => { dispose = value })
            await settle()
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
            listened(); await installing
        }
        await settle(); fixture.invoke.mockClear()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange'))
        await settle(); expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_publish')).toHaveLength(1)
        document.dispatchEvent(new Event('visibilitychange')); await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_publish')).toHaveLength(1)
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_receive')).toHaveLength(0)
    })
    it('passes frozen exit identity, waits for publication, and blocks stale selection', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const drain = externalLwwExitDrain('sync', 'selection', true)!; expect(drain.id).toBe('external:sync:selection')
        const target = { revision: 12, libraryEpoch: 'epoch', selectionEpoch: 'selection', selectionId: drain.id }
        let finish!: () => void
        fixture.invoke.mockImplementation(async command => { if (command === 'external_lww_publish') { await new Promise<void>(resolve => { finish = resolve }); return { segments: '0', more: false } } })
        let completed = false; const pending = drain.drain(target, new AbortController().signal).then(result => { completed = true; return result })
        await settle(); expect(completed).toBe(false)
        expect(fixture.invoke).toHaveBeenCalledWith('external_lww_publish', expect.objectContaining({ request: expect.objectContaining({ exitTarget: { revision: '12', libraryEpoch: 'epoch', selectionEpoch: 'selection' } }) }))
        finish(); expect(await pending).toEqual({ kind: 'complete' })
        expect(await drain.drain({ ...target, selectionEpoch: 'changed' }, new AbortController().signal)).toEqual({ kind: 'blocked', reason: 'sync-binding-changed' })
    })
    it('manual Sync Now publishes after local flush and completes available receive', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
        await requestExternalLwwNow('sync')
        expect(fixture.flush).toHaveBeenCalledWith('external-lww-publish')
        expect(fixture.invoke.mock.calls.map(call => call[0])).toEqual(['external_lww_publish', 'external_lww_receive'])
    })
    it('tells the native fence whether a binding change makes this a new device', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const transport = fixture.registrations.get('sync')!
        fixture.invoke.mockClear()
        await transport.fenceOldJobs(context())
        await transport.fenceOldJobs({ ...context(), mode: 'new-device' })
        await externalLwwExitDrain('sync', 'selection', true)!.cancel('cancel-exit')
        expect(fixture.invoke.mock.calls.filter(call => call[0] === 'external_lww_fence').map(call => call[1])).toEqual([
            { connectionId: 'sync', newDevice: false }, { connectionId: 'sync', newDevice: true }, { connectionId: 'sync', newDevice: false },
        ])
    })
    it('finishes an owed initial queue natively when the binding resumes and publishes it through the ordinary publication', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const transport = fixture.registrations.get('sync')!
        fixture.invoke.mockClear()
        await transport.resumeBinding(context()); await settle()
        expect(fixture.invoke.mock.calls.slice(0, 2)).toEqual([
            ['pds_lww_finish_initial_publication', { request: { bindingAuthority: '4', requestId: expect.any(String) } }],
            ['external_lww_resume', { connectionId: 'sync' }],
        ])
        fixture.invoke.mockClear(); fixture.flush.mockClear()
        await transport.publishInitialSharedState(context())
        expect(fixture.invoke.mock.calls.map(call => call[0])).toEqual(['external_lww_publish'])
        expect(fixture.invoke.mock.calls[0][1]).not.toHaveProperty('initial')
        expect(fixture.invoke.mock.calls[0][1].request).not.toHaveProperty('turnLimit')
        expect(fixture.flush).not.toHaveBeenCalled()
    })
    it('reconciles the authoritative revision when publication fails after native clock repair', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const transport = fixture.registrations.get('sync')!
        fixture.invoke.mockRejectedValueOnce({ kind: 'transient' })
        fixture.outbox.mockResolvedValueOnce({ revision: 13, entries: [] })
        let reconciled = 0
        fixture.storageMutation.mockImplementationOnce(async work => { reconciled = await work(12) })
        fixture.diagnostic.mockClear()
        await expect(transport.publishInitialSharedState!(context())).rejects.toEqual({ kind: 'transient' })
        expect(reconciled).toBe(13)
        expect(fixture.outbox).toHaveBeenCalledWith(expect.objectContaining({ bindingAuthority: '4', limit: '1' }))
        // The failed native call may have sent segments before it failed, so the log does not claim a count.
        const runs = fixture.diagnostic.mock.calls.map(([, args]) => args.event as Record<string, unknown>).filter(event => event.kind === 'sync-run')
        expect(runs).toHaveLength(1)
        expect(runs[0]).toMatchObject({ operation: 'publish' })
        expect(runs[0].count).toBeUndefined()
    })
})

describe('bounded production calls and lifecycle ownership', () => {
    it('yields receive after four pages so a bounded send can advance', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        const calls: string[] = []; let pages = 0
        fixture.invoke.mockImplementation(async (command, args) => {
            if (command === 'external_lww_receive') {
                calls.push('receive')
                if (++pages === 1) fixture.revision!(14, 'generation-complete')
                return pages <= 9 ? { header: { bindingAuthority: '4', requestId: String(pages) }, changes: [] } : null
            }
            if (command === 'external_lww_publish') {
                calls.push('publish')
                expect(args.request.turnLimit).toBe(4)
                return { segments: '1', more: false }
            }
        })
        fixture.viewport!({}); for (let i = 0; i < 8; i++) await settle()
        expect(calls).toEqual(['receive', 'receive', 'receive', 'receive', 'publish', 'receive', 'receive', 'receive', 'receive', 'receive', 'receive'])
        expect(fixture.apply).toHaveBeenCalledTimes(9)
    })
    it('manual completion drains more than four pages with no publish limit', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
        let received = 0
        fixture.invoke.mockImplementation(async (command, args) => {
            if (command === 'external_lww_publish') { expect(args.request).not.toHaveProperty('turnLimit'); return { segments: '7', more: false } }
            if (command === 'external_lww_receive') return ++received <= 9 ? { header: { bindingAuthority: '4', requestId: String(received) }, changes: [] } : null
        })
        await requestExternalLwwNow('sync')
        expect(received).toBe(10)
        expect(fixture.apply).toHaveBeenCalledTimes(9)
    })
    it('clears a receive failure only after successful receive', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        let failures: ReadonlyMap<string, unknown> = new Map()
        const unsubscribe = subscribeExternalLwwFailures(value => { failures = value })
        const failure = { kind: 'transient', retryAtMs: String(Date.now() + 120_000) }
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') throw failure
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.viewport!({}); await settle()
        expect(failures.get('sync')).toEqual(failure)
        fixture.revision!(15, 'generation-complete'); await settle()
        expect(failures.get('sync')).toEqual(failure)
        fixture.invoke.mockImplementation(async command => command === 'external_lww_receive' ? null : { segments: '0', more: false })
        await vi.advanceTimersByTimeAsync(120_000); await settle()
        expect(failures.has('sync')).toBe(false)
        unsubscribe()
    })
    it('continues a receive catch-up hidden until no pages remain, then starts no new check, and logs each run', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        let pages = 0; let release!: () => void
        const gate = new Promise<void>(resolve => { release = resolve })
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') {
                if (++pages === 1) await gate
                return pages <= 9 ? { header: { bindingAuthority: '4', requestId: String(pages) }, changes: [] } : null
            }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.diagnostic.mockClear()
        fixture.viewport!({}); await settle()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange'))
        release(); for (let i = 0; i < 12; i++) await settle()
        expect(fixture.apply).toHaveBeenCalledTimes(9)
        const receives = () => fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_receive').length
        const settled = receives()
        await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(receives()).toBe(settled)
        const runs = fixture.diagnostic.mock.calls.map(([, args]) => args.event as Record<string, unknown>)
            .filter(event => event.kind === 'sync-run' && event.operation === 'receive')
        expect(runs.map(event => [event.count, event.more, event.outcome]))
            .toEqual([[4, true, 'complete'], [4, true, 'complete'], [1, false, 'complete']])
    })
    it('leaves changes made while hidden for the return to the screen', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        fixture.invoke.mockClear()
        fixture.revision!(20, 'generation-complete'); await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(0)
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
        for (let i = 0; i < 6; i++) await settle()
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(1)
    })
    it('holds one deferred sync hold while work runs and releases it when sync is idle', async () => {
        const holds: Array<{ disposed: boolean }> = []
        vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => {
            const hold = { disposed: false }
            holds.push(hold)
            return { progress() {}, async dispose() { hold.disposed = true } }
        })
        dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 4; i++) await settle()
        expect(holds.length).toBeGreaterThan(0)
        expect(holds.filter(hold => !hold.disposed)).toHaveLength(0)
        let complete!: () => void
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') { await new Promise<void>(resolve => { complete = resolve }); return null }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.viewport!({}); await settle()
        expect(vi.mocked(beginDeferredMobileBackgroundTask).mock.calls.at(-1)?.[0]).toBe('sync')
        expect(holds.filter(hold => !hold.disposed)).toHaveLength(1)
        complete(); await settle(); await settle()
        expect(holds.filter(hold => !hold.disposed)).toHaveLength(0)
    })
    it('stops hidden routine work when its background time expires', async () => {
        const expiration = new AbortController()
        vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => ({ signal: expiration.signal, progress() {}, async dispose() {} }))
        dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 4; i++) await settle()
        let pages = 0; let release!: () => void
        const gate = new Promise<void>(resolve => { release = resolve })
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') {
                if (++pages === 1) await gate
                return pages <= 40 ? { header: { bindingAuthority: '4', requestId: String(pages) }, changes: [] } : null
            }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.viewport!({}); await settle()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        expiration.abort(); release(); for (let i = 0; i < 8; i++) await settle()
        const stopped = pages
        await vi.advanceTimersByTimeAsync(600_000); await settle()
        expect(pages).toBe(stopped)
        expect(pages).toBeLessThanOrEqual(4)
    })
    it('stops the plugin startup receive when its background time expires', async () => {
        const expiration = new AbortController()
        vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => ({ signal: expiration.signal, progress() {}, async dispose() {} }))
        dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 4; i++) await settle()
        let pages = 0; let release!: () => void
        const gate = new Promise<void>(resolve => { release = resolve })
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') {
                if (++pages === 1) await gate
                return pages <= 40 ? { header: { bindingAuthority: '4', requestId: String(pages) }, changes: [] } : null
            }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        const outcome = fixture.registrations.get('sync')!.receiveAvailableChanges!(context()).then(() => 'complete', (error: unknown) => error)
        await settle()
        fixture.diagnostic.mockClear()
        const expired = new BackgroundExecutionExpiredError()
        expiration.abort(expired); release()
        expect(await outcome).toBe(expired)
        expect(pages).toBe(1)
        // Running out of background time ends the run as cancelled, not as a sync failure.
        const runs = fixture.diagnostic.mock.calls.map(([, args]) => args.event as Record<string, unknown>).filter(event => event.kind === 'sync-run')
        expect(runs).toEqual([expect.objectContaining({ operation: 'receive', outcome: 'cancelled' })])
        expect(runs[0].more).toBeUndefined()
    })
    it('expires a hide-time publication waiting behind a receive without sending it', async () => {
        const expiration = new AbortController()
        vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => ({ signal: expiration.signal, progress() {}, async dispose() {} }))
        dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 4; i++) await settle(); fixture.invoke.mockClear()
        let complete!: () => void
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') { await new Promise<void>(resolve => { complete = resolve }); return null }
            if (command === 'external_lww_publish') return { segments: '1', more: false }
        })
        fixture.viewport!({}); await settle()
        Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        document.dispatchEvent(new Event('visibilitychange')); await settle()
        expiration.abort(); complete(); await settle()
        await vi.advanceTimersByTimeAsync(600_000)
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(0)
    })
    describe('holds sync time where a departure would otherwise leave sync stopped', () => {
        const track = () => {
            const holds: Array<{ disposed: boolean }> = []
            vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => {
                const hold = { disposed: false }
                holds.push(hold)
                return { progress() {}, async dispose() { hold.disposed = true } }
            })
            return () => holds.filter(hold => !hold.disposed).length
        }
        // Holds the first call of `command` until released; later calls answer at once.
        const gate = (command: string) => {
            let release!: () => void
            const released = new Promise<void>(resolve => { release = resolve })
            const answer = fixture.invoke.getMockImplementation()!
            fixture.invoke.mockImplementation(async (name: string, args: unknown) => {
                if (name === command) await released
                return answer(name, args)
            })
            return release
        }
        // Checks the holds while the gated call waits, then releases it and checks that every hold ended.
        const whileWaiting = async (release: () => void, waiting: () => void, done?: Promise<unknown>) => {
            try { await settle(); waiting() }
            finally { release(); await done; for (let i = 0; i < 6; i++) await settle() }
        }
        it('while the start waits to resume the binding, and not when sync is off', async () => {
            const held = track()
            const release = gate('external_lww_resume')
            const installing = installExternalLwwAdapters(state()).then(value => { dispose = value })
            await whileWaiting(release, () => expect(held()).toBeGreaterThan(0), installing)
            expect(held()).toBe(0)
            // A stored sync connection that is not the sync target leaves nothing to resume.
            dispose?.(); dispose = undefined
            vi.mocked(beginDeferredMobileBackgroundTask).mockClear()
            fixture.state.mockResolvedValue({ ...binding, target: { kind: 'none' } })
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' })
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            expect(beginDeferredMobileBackgroundTask).not.toHaveBeenCalled()
        })
        it('while the plugin startup receive runs beside an idle scheduler, also when it starts off screen', async () => {
            const held = track()
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            expect(held()).toBe(0)
            let release = gate('external_lww_receive')
            await whileWaiting(release, () => {
                expect(vi.mocked(beginDeferredMobileBackgroundTask).mock.calls.at(-1)?.[0]).toBe('sync')
                expect(held()).toBe(1)
            }, fixture.registrations.get('sync')!.receiveAvailableChanges!(context()))
            expect(held()).toBe(0)
            // Left during launch: the page is hidden before the receive begins.
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' })
            release = gate('external_lww_receive')
            await whileWaiting(release, () => expect(held()).toBe(1), fixture.registrations.get('sync')!.receiveAvailableChanges!(context()))
            expect(held()).toBe(0)
        })
        it('while the return to the screen waits to resume', async () => {
            const held = track()
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange'))
            for (let i = 0; i < 6; i++) await settle()
            expect(held()).toBe(0)
            const release = gate('external_lww_resume')
            Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
            await whileWaiting(release, () => expect(held()).toBe(1))
            expect(held()).toBe(0)
        })
        // Every hold that has not ended expires at once; one that begins afterwards is refused and never expires.
        const expiringHolds = () => {
            const held = new Set<AbortController>()
            let expired = false
            vi.mocked(beginDeferredMobileBackgroundTask).mockImplementation((): MobileBackgroundTask => {
                const controller = new AbortController()
                if (expired) return { signal: controller.signal, protection: { state: 'refused', reason: 'background' }, progress() {}, async dispose() {} }
                held.add(controller)
                return { signal: controller.signal, protection: { state: 'deferred' }, progress() {}, async dispose() { held.delete(controller) } }
            })
            return () => { expired = true; for (const controller of held) controller.abort(new BackgroundExecutionExpiredError()) }
        }
        const show = (value: 'hidden' | 'visible') => { Object.defineProperty(document, 'visibilityState', { configurable: true, value }); document.dispatchEvent(new Event('visibilitychange')) }
        // Android's own start, which a page it keeps running off screen hears without becoming visible again.
        const returnToApp = () => window.dispatchEvent(new CustomEvent('risu-native-lifecycle', { detail: { reason: 'start' } }))
        // Lets routine sync run, then tells whether it sent, received and downloaded since the last check.
        let resumes = 0
        const synced = async () => {
            for (let i = 0; i < 6; i++) await settle()
            await vi.advanceTimersByTimeAsync(600_000)
            const commands = fixture.invoke.mock.calls.map(([command]) => command)
            resumes = commands.filter(command => command === 'external_lww_resume').length
            const ran = commands.some(command => command === 'external_lww_publish' || command === 'external_lww_receive') && fixture.download.mock.calls.length > 0
            fixture.invoke.mockClear(); fixture.download.mockClear()
            return ran
        }
        it.each([
            ['the start', 'kept away'],
            ['the return to the screen', 'kept away'],
            ['a cancelled exit', 'kept away'],
            ['the start', 'kept away before the binding resumes'],
            ['the start', 'back and kept away again'],
            ['the start', 'hidden away'],
            ['the start', 'hidden away and back while it waits'],
            ['the start', 'back before the expiry arrives'],
        ] as const)('holds routine sync after %s outlives its background time until the app returns (%s)', async (entry, order) => {
            const android = order === 'kept away' || order === 'kept away before the binding resumes' || order === 'back and kept away again'
            const backBeforeItEnds = order === 'back before the expiry arrives' || order === 'hidden away and back while it waits'
            fixture.os = android ? 'android' : 'ios'
            const expire = expiringHolds()
            if (entry !== 'the start') { dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle() }
            if (entry === 'the return to the screen') { show('hidden'); for (let i = 0; i < 6; i++) await settle() }
            const drain = entry === 'a cancelled exit' ? externalLwwExitDrain('sync', 'selection')! : undefined
            await drain?.cancel('cancel-exit')
            fixture.invoke.mockClear(); fixture.download.mockClear()
            const release = gate('external_lww_resume')
            // The binding check before the transport resumes still waits when the start's hold expires.
            let checked!: () => void
            const checking = new Promise<void>(resolve => { checked = resolve })
            if (order === 'kept away before the binding resumes') {
                const resume = fixture.resumeCurrent.getMockImplementation()!
                fixture.resumeCurrent.mockImplementation(async target => { await checking; await resume(target) })
            } else checked()
            const resumed = entry === 'the start' ? installExternalLwwAdapters(state()).then(value => { dispose = value })
                : drain ? drain.resumeAfterExitCancel!() : (returnToApp(), show('visible'))
            await settle()
            // The app leaves with the page kept running and visible, unless it went hidden first.
            if (order === 'back and kept away again') { show('hidden'); returnToApp(); show('visible') }
            if (order === 'back before the expiry arrives') { show('hidden'); show('visible') }
            else if (!android) show('hidden')
            expire()
            if (order === 'hidden away and back while it waits') show('visible')
            checked(); release(); await resumed
            if (!backBeforeItEnds) {
                // Nothing more starts natively while the app stays away.
                expect(await synced()).toBe(false); expect(resumes).toBe(1)
                if (android) returnToApp(); else show('visible')
            }
            expect(await synced()).toBe(true)
        })
        it('keeps routine sync stopped after its own hold expires away, also when the network returns or the page is hidden, until the app returns', async () => {
            fixture.os = 'android'
            const expire = expiringHolds()
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            const release = gate('external_lww_receive')
            fixture.revision!(13, 'local'); await vi.advanceTimersByTimeAsync(600_000)
            expire(); release(); await synced()
            vi.mocked(beginDeferredMobileBackgroundTask).mockClear()
            Object.defineProperty(navigator, 'onLine', { configurable: true, value: false }); window.dispatchEvent(new Event('offline'))
            Object.defineProperty(navigator, 'onLine', { configurable: true, value: true }); window.dispatchEvent(new Event('online'))
            expect(await synced()).toBe(false); expect(resumes).toBe(0)
            expect(beginDeferredMobileBackgroundTask).not.toHaveBeenCalled()
            // Android stops keeping the page shortly after, while the app stays away.
            show('hidden'); for (let i = 0; i < 6; i++) await settle()
            expect(fixture.invoke.mock.calls.some(([command]) => command === 'external_lww_publish')).toBe(false)
            show('visible'); returnToApp()
            expect(await synced()).toBe(true)
        })
        it.each([
            ['stays', 1],
            ['is kept away until its time runs out', 2],
            ['leaves the screen', 2],
        ] as const)('resumes routine sync after the return to the screen when the app %s while it resumes', async (away, passes) => {
            fixture.os = 'android'
            const expire = expiringHolds()
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            show('hidden'); for (let i = 0; i < 6; i++) await settle()
            fixture.invoke.mockClear()
            // Android tells a return from a hidden page twice: the page becomes visible, and the app starts.
            const release = gate('external_lww_receive')
            show('visible'); returnToApp(); for (let i = 0; i < 6; i++) await settle()
            if (away === 'is kept away until its time runs out') { expire(); returnToApp() }
            if (away === 'leaves the screen') { show('hidden'); show('visible'); returnToApp() }
            release(); for (let i = 0; i < 6; i++) await settle()
            expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(passes)
            fixture.invoke.mockClear()
            fixture.revision!(14, 'local'); await vi.advanceTimersByTimeAsync(600_000)
            expect(fixture.invoke.mock.calls.some(([command]) => command === 'external_lww_publish')).toBe(true)
        })
        it.each(['the app returns after its time ran out', 'the network returns', 'a file operation ends'] as const)('starts routine sync when %s while the start finishes', async event => {
            fixture.os = 'android'
            const expire = expiringHolds()
            const admission = await import('../../libraryFileOperation')
            const reserved = event === 'a file operation ends' ? admission.reserveLibraryFileOperation() : undefined
            if (event === 'the network returns') Object.defineProperty(navigator, 'onLine', { configurable: true, value: false })
            // The start still runs after the binding resumed.
            let finish!: () => void
            const finishing = new Promise<void>(resolve => { finish = resolve })
            const resume = fixture.resumeCurrent.getMockImplementation()!
            fixture.resumeCurrent.mockImplementation(async target => { await resume(target); await finishing })
            // The time runs out while the binding resumes, so the start leaves sync stopped.
            const release = event === 'the app returns after its time ran out' ? gate('external_lww_resume') : undefined
            const installing = installExternalLwwAdapters(state()).then(value => { dispose = value })
            for (let i = 0; i < 6; i++) await settle()
            if (release) { expire(); release(); for (let i = 0; i < 6; i++) await settle(); returnToApp() }
            if (event === 'the network returns') { Object.defineProperty(navigator, 'onLine', { configurable: true, value: true }); window.dispatchEvent(new Event('online')) }
            reserved?.()
            finish(); await installing
            fixture.invoke.mockClear()
            fixture.revision!(14, 'local'); await vi.advanceTimersByTimeAsync(600_000)
            expect(fixture.invoke.mock.calls.some(([command]) => command === 'external_lww_publish')).toBe(true)
        })
        it('while a cancelled exit waits to resume sync', async () => {
            const held = track()
            dispose = await installExternalLwwAdapters(state()); for (let i = 0; i < 6; i++) await settle()
            const release = gate('external_lww_resume')
            await whileWaiting(release, () => expect(held()).toBe(1), externalLwwExitDrain('sync', 'selection')!.resumeAfterExitCancel!())
            expect(held()).toBe(0)
        })
    })
    it('coalesces foreground notifications and does not revive a removed binding after a late result', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
        let complete!: () => void
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') { await new Promise<void>(resolve => { complete = resolve }); return null }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.viewport!({}); await settle()
        for (let i = 0; i < 20; i++) window.dispatchEvent(new Event('online'))
        await settle()
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(0)
        fixture.state.mockResolvedValue({ ...binding, target: { kind: 'none' }, targetAuthority: '5' })
        await refreshExternalLwwAdapters({ connections: [] } as unknown as ExternalStorageState)
        complete(); await settle()
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(0)
    })
    it('coalesces foreground notifications into one restart after a held old call', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
        let complete!: () => void; let first = true
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') {
                if (first) { first = false; await new Promise<void>(resolve => { complete = resolve }) }
                return null
            }
            if (command === 'external_lww_publish') return { segments: '0', more: false }
        })
        fixture.viewport!({}); await settle()
        for (let i = 0; i < 20; i++) window.dispatchEvent(new Event('online'))
        complete(); await settle(); await settle()
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(1)
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(1)
        expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_receive')).toHaveLength(2)
    })
    it('leaves no listener on the binding lifetime once routine turns, downloads and catch-up receives settle', async () => {
        const lifetime = new AbortController()
        const attached = trackAbortListeners(lifetime.signal)
        fixture.resumeCurrent.mockImplementation(async target => { await fixture.registrations.get(target.connectionId)!.resumeBinding({ state: await fixture.state(), signal: lifetime.signal }) })
        let pages = 0
        fixture.invoke.mockImplementation(async command => {
            if (command === 'external_lww_receive') return ++pages <= 5 ? assetPage(String(pages)) : null
            if (command === 'external_lww_publish') return { segments: '1', more: false }
        })
        dispose = await installExternalLwwAdapters(state()); await settle()
        fixture.viewport!({}); fixture.revision!(14, 'generation-complete'); for (let i = 0; i < 8; i++) await settle()
        const transport = fixture.registrations.get('sync') as SyncBindingTransport & { receiveAvailableChanges(context: BindingContext): Promise<void> }
        await transport.receiveAvailableChanges(context()); for (let i = 0; i < 4; i++) await settle()
        expect(fixture.apply).toHaveBeenCalledTimes(5)
        expect(fixture.invoke.mock.calls.map(call => call[0])).toContain('external_lww_publish')
        expect(fixture.download.mock.calls.length).toBeGreaterThan(1)
        expect(attached()).toEqual([])
    })
    it('keeps empty sync progress hidden and exposes meaningful bounded publication', async () => {
        dispose = await installExternalLwwAdapters(state()); await settle()
        let progress: ReadonlyMap<string, ExternalOperationProgress> = new Map()
        const unsubscribe = subscribeExternalProgress(value => { progress = value })
        await requestExternalLwwNow('sync')
        expect(externalProgressFor(progress, 'sync')).toBeUndefined()
        fixture.invoke.mockImplementation(async command => command === 'external_lww_publish' ? { segments: '1', more: false } : null)
        fixture.revision!(20, 'generation-complete'); await settle()
        expect(externalProgressFor(progress, 'sync')).toMatchObject({ state: 'complete', visible: true })
        unsubscribe()
    })
})

it('rechecks foreground intent after hide and show supersede a pending restart', async () => {
    dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
    let complete!: () => void; let first = true
    fixture.invoke.mockImplementation(async command => {
        if (command === 'external_lww_receive') {
            if (first) { first = false; await new Promise<void>(resolve => { complete = resolve }) }
            return null
        }
        if (command === 'external_lww_publish') return { segments: '0', more: false }
    })
    fixture.viewport!({}); await settle()
    window.dispatchEvent(new Event('online')); await settle()
    Object.defineProperty(navigator, 'onLine', { configurable: true, value: false }); window.dispatchEvent(new Event('offline'))
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange'))
    Object.defineProperty(navigator, 'onLine', { configurable: true, value: true })
    Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange'))
    complete(); for (let i = 0; i < 8; i++) await settle()
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(1)
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(1)
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_receive')).toHaveLength(2)
})

it('does not restart a disposed adapter after exit cancellation resume returns late', async () => {
    dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
    const drain = externalLwwExitDrain('sync', 'selection')!
    let complete!: () => void
    fixture.invoke.mockImplementation(async command => {
        if (command === 'external_lww_resume') await new Promise<void>(resolve => { complete = resolve })
        if (command === 'external_lww_publish') return { segments: '0', more: false }
        if (command === 'external_lww_receive') return null
    })
    const resume = drain.resumeAfterExitCancel!(); await settle()
    dispose!(); dispose = undefined
    complete(); await resume; await vi.advanceTimersByTimeAsync(120_000)
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_publish')).toHaveLength(0)
})

it('preserves a valid manual lifetime when foreground notifications arrive during publication', async () => {
    dispose = await installExternalLwwAdapters(state()); await settle(); fixture.invoke.mockClear()
    let complete!: () => void; let first = true
    fixture.invoke.mockImplementation(async command => {
        if (command === 'external_lww_publish') {
            if (first) { first = false; await new Promise<void>(resolve => { complete = resolve }) }
            return { segments: '0', more: false }
        }
        if (command === 'external_lww_receive') return null
    })
    const manual = requestExternalLwwNow('sync'); await settle()
    for (let i = 0; i < 20; i++) window.dispatchEvent(new Event('online'))
    await settle(); complete(); await expect(manual).resolves.toBeUndefined()
    for (let i = 0; i < 4; i++) await settle()
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_resume')).toHaveLength(1)
    expect(fixture.invoke.mock.calls.filter(([command]) => command === 'external_lww_receive')).toHaveLength(2)
})

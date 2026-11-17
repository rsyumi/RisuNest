// @vitest-environment happy-dom
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from 'vitest'
import type { SyncBindingState } from './bindingFlow'
import { language } from 'src/lang'

const f = vi.hoisted(() => ({
    invoke: vi.fn(), offline: true, repairError: undefined as unknown, activationError: undefined as unknown, fenceError: undefined as unknown,
    configured: true, pending: null as unknown, inspectError: undefined as unknown, pushError: undefined as unknown,
    sharedData: false, marker: false, queued: 0, outbox: 0, pushed: 0,
    binding: undefined as unknown as SyncBindingState,
    runtime: { revision: 0, getStorageAuthorityEpoch: () => 'storage', subscribeActiveConversationViewportSource: () => () => {}, captureSelectedConversationTarget: () => null, setActivatedLibraryRecoveryLifecycle() {}, markCommittedWorkingSetRefreshRequired() {} },
}))
vi.mock('@tauri-apps/api/core', () => ({ invoke: f.invoke }))
vi.mock('@tauri-apps/api/event', () => ({ listen: async () => () => {} }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'tauri', os: 'windows' }))
vi.mock('src/ts/stores.svelte', () => ({ selectedCharID: { subscribe: (run: (value: number) => void) => { run(-1); return () => {} } } }))
vi.mock('../database.svelte', () => ({ getDatabase: () => ({ characters: [] }) }))
vi.mock('./bindingDialog', () => ({ confirmSyncBindingReplacement: async () => true, confirmPreviousStorageFiles: async () => 'connect', downloadPreviousStorageFiles: async () => {} }))
vi.mock('./bindingLocalData', () => ({ hasLocalSharedBindingData: async () => f.sharedData }))
vi.mock('../libraryCharacters', () => ({ hasLibraryCharacters: async () => false }))
vi.mock('../persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: () => f.runtime,
    withPausedPersistentWrites: async (_reason: string, operation: (token: unknown) => Promise<unknown>) => operation({ id: 'paused' }),
    beginActivatedLibraryGuard: () => ({ complete() {}, async abortUnchanged() {} }),
    refreshActivatedLibraryUnderPause: async () => ({ projection: 'applied' }), applyPersistentLwwReceive: async () => {}, flushPendingDataLocally: async () => {},
}))
vi.mock('../../mobileBackgroundTask', async () => (await import('../../mobileBackgroundTask.testSupport')).mobileBackgroundTaskModule())
vi.mock('../committedWorkingSetContinuation', () => ({ registerCommittedWorkingSetContinuation: vi.fn() }))
vi.mock('src/ts/plugins/apiV3/v3.svelte', () => ({ fencePluginExecutionForAuthorityReplacement: async () => {}, invalidatePluginCachesAfterAuthorityReplacement: async () => {}, restartPluginsAfterAuthorityReplacement: async () => {} }))
vi.mock('../persistentRevisionEvents', () => ({ subscribeLocalPersistentRevision: () => () => {} }))
vi.mock('../generatingConversationRegistry', () => ({ generatingConversations: { snapshot: () => [] } }))
vi.mock('src/ts/alert', () => ({ alertConfirm: async () => false, alertActionConfirm: vi.fn(async () => true), alertCheckboxConfirm: vi.fn(async () => ({ confirmed: true, checked: false })) }))
vi.mock('@lucide/svelte', () => ({ CheckIcon: () => {}, CloudDownloadIcon: () => {}, HardDriveIcon: () => {}, InfoIcon: () => {}, LoaderCircleIcon: () => {}, TriangleAlertIcon: () => {} }))
vi.mock('./serverSyncRegistrationInbox', () => ({ serverRegistrationInbox: { changed: { subscribe: () => () => {} }, releaseConsumed() {}, take: () => undefined } }))
vi.mock('./serverSyncQr', () => ({ canScanServerRegistration: false, createServerQrScanner: () => ({ cancel() {} }) }))
vi.mock('./serverAssetResidency', async () => {
    const { assetResidencyStatus } = await import('./serverAssetResidency.testSupport')
    return { getAssetResidencyStatus: vi.fn(async () => assetResidencyStatus()), setAssetResidencyPolicy: vi.fn(), evictLocalAssets: vi.fn(), cancelAssetResidencyOperation: vi.fn() }
})

let production: typeof import('./serverSyncProduction')
let ui: typeof import('svelte')
let component: ReturnType<(typeof import('svelte'))['mount']> | undefined
const settle = async () => { for (let i = 0; i < 40; i++) await Promise.resolve(); await ui.tick() }
const commands = () => f.invoke.mock.calls.map(([command]) => command)
const button = (text: string) => [...document.querySelectorAll('button')].find(value => value.textContent?.trim() === text)
// The first import compiles the settings page, which can outlast a test's time limit in a full run.
beforeAll(async () => { await import('src/lib/Setting/Pages/ServerSyncSettings.svelte') }, 60_000)
beforeEach(async () => {
    vi.resetModules(); f.invoke.mockReset(); f.offline = true; f.repairError = undefined; f.activationError = undefined; f.fenceError = undefined
    f.configured = true; f.pending = null; f.inspectError = undefined; f.pushError = undefined
    f.sharedData = false; f.marker = false; f.queued = 0; f.outbox = 0; f.pushed = 0
    f.binding = { target: { kind: 'server', connectionId: 'server' }, targetAuthority: '4', selectionEpoch: 'persisted', libraryId: 'library', progress: null }
    Object.defineProperty(document, 'visibilityState', { value: 'hidden', configurable: true })
    f.invoke.mockImplementation(async (command, args) => {
        if (command === 'pds_lww_binding_state') return structuredClone(f.binding)
        if (command === 'server_sync_status') return { configured: f.configured, writerId: 'writer', bindingAuthority: f.binding.targetAuthority, libraryId: 'library', deviceId: 'device' }
        if (command === 'server_sync_lww_activate' && f.activationError) throw f.activationError
        if (command === 'server_sync_lww_activate' && f.offline) throw { code: 'server-unreachable', status: 503, retryable: true }
        if (command === 'server_sync_lww_activate') {
            // Native activation commits the pending registration and queues an initial publication the switch marked.
            f.configured = true; f.pending = null
            if (f.marker) { f.marker = false; f.queued += 1; f.outbox += 2 }
        }
        if (command === 'server_sync_configure') f.pending = { endpoint: args.config.endpoint, libraryId: args.config.libraryId, epoch: 'epoch', serverEmpty: true }
        if (command === 'server_sync_lww_push') { if (f.pushError) throw f.pushError; if (!f.outbox) return null; f.outbox -= 1; f.pushed += 1; return { accepted: 1 } }
        if (command === 'server_sync_lww_pull') return { bindingAuthority: f.binding.targetAuthority, requestId: 'pull', changes: [] }
        if (command === 'server_sync_lww_pending_binding') return structuredClone(f.pending)
        if (command === 'server_sync_lww_pending_count') return f.outbox
        if (command === 'server_sync_lww_inspect') {
            if (f.inspectError) throw f.inspectError
            return { inspectionId: 'inspection', targetId: 'target', libraryId: 'library', empty: true, previouslyBoundLibrary: false, registrationChanged: false, serverRestored: false }
        }
        if (command === 'server_sync_lww_retry' && f.repairError) throw f.repairError
        if (command === 'server_sync_lww_fence' && f.fenceError) throw f.fenceError
        if (command === 'pds_lww_switch_target') {
            expect(args.request).toMatchObject({ bindingAuthority: f.binding.targetAuthority, expectedSelectionEpoch: f.binding.selectionEpoch })
            f.binding = { ...f.binding, target: args.request.target, targetAuthority: String(Number(f.binding.targetAuthority) + 1), selectionEpoch: 'disconnected', libraryId: args.request.target.kind === 'none' ? null : 'library' }
            f.marker = args.request.initialPublication === true
            return structuredClone(f.binding)
        }
        return null
    })
    production = await import('./serverSyncProduction')
    ui = await import('svelte')
    production.initializeNativeSyncBindings()
})
afterEach(async () => { if (component) await ui.unmount(component); component = undefined; production.disposeNativeSyncBindings(); document.body.replaceChildren() })
async function offlineStartup() {
    // Activation retries after 1 s and 2 s before the launch failure counts. Only those waits run on a
    // fake clock; the rest of each test keeps real timers.
    vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
    try {
        const installed = production.installServerSyncProduction()
        await vi.advanceTimersByTimeAsync(3_000)
        await expect(installed).resolves.toBeUndefined()
    } finally { vi.useRealTimers() }
    expect(commands().filter(command => command === 'server_sync_lww_activate')).toHaveLength(3)
    expect(commands()).not.toContain('server_sync_lww_fence')
    expect(commands()).not.toContain('server_sync_notify_start')
    expect(commands()).not.toContain('server_sync_lww_retry')
    expect(commands()).not.toContain('pds_lww_switch_target')
    expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, running: false, paused: false, error: 'server-unreachable' })
}
async function settings() { const Settings = (await import('src/lib/Setting/Pages/ServerSyncSettings.svelte')).default; component = ui.mount(Settings, { target: document.body }); await settle() }

describe('persisted server offline startup', () => {
    it.each([
        { code: 'unauthorized', status: 401, retryable: false },
        { code: 'invalid-device-token', status: 401, retryable: false },
        { code: 'rate-limited', status: 429, retryable: true },
    ])('reports the launch failure to the plugin receive and clears it after retry ($code)', async failure => {
        f.activationError = failure
        await production.installServerSyncProduction()
        const { getSyncBindingTransport } = await import('./bindingRegistry')
        const transport = getSyncBindingTransport({ kind: 'server', connectionId: 'server' })!
        const context = { state: structuredClone(f.binding), signal: new AbortController().signal }
        await expect(transport.receiveAvailableChanges!(context)).rejects.toThrow(failure.code)
        expect(production.getServerSyncController().snapshot().error).toBe(failure.code)

        const { configurePluginStartup, pluginStartupGate, retryPluginStartup } = await import('../../plugins/pluginStartupGate')
        const { get } = await import('svelte/store')
        let started = false
        configurePluginStartup(() => true, async () => { if (await pluginStartupGate.ensure()) started = true })
        try {
            await pluginStartupGate.ensure()
            await vi.waitFor(() => expect(get(pluginStartupGate.state)).toMatchObject({ phase: 'failed', target: 'server' }))
            expect(started).toBe(false)
            f.activationError = undefined; f.offline = false
            await retryPluginStartup()
            await vi.waitFor(() => expect(started).toBe(true))
            expect(get(pluginStartupGate.state).phase).toBe('released')
            expect(production.getServerSyncController().snapshot().error).toBe('')
            await expect(transport.receiveAvailableChanges!(context)).resolves.toBeUndefined()
        } finally {
            configurePluginStartup(() => false, async () => {})
            pluginStartupGate.invalidate(); pluginStartupGate.state.set({ phase: 'released', since: 0 })
        }
    })
    it('reports an unavailable receive before resume and retains real authority and selection changes', async () => {
        await production.installServerSyncProduction({ resumeBound: false })
        const { getSyncBindingTransport } = await import('./bindingRegistry')
        const transport = getSyncBindingTransport({ kind: 'server', connectionId: 'server' })!
        const context = { state: structuredClone(f.binding), signal: new AbortController().signal }
        await expect(transport.receiveAvailableChanges!(context)).rejects.toThrow('Sync binding is unavailable')
        f.offline = false
        await production.retryServerSync()
        for (const field of ['targetAuthority', 'selectionEpoch'] as const) {
            await expect(transport.receiveAvailableChanges!({ ...context, state: { ...context.state, [field]: 'changed' } })).rejects.toThrow('Sync binding changed')
        }
    })

    it('keeps real retry and disconnect settings actions while the installer waits to activate offline', async () => {
        await offlineStartup(); await settings()
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: 'server-unreachable' })
        expect(button(language.risuNest.serverSync.syncNow)).toBeDefined(); expect(button(language.risuNest.serverSync.disconnect)).toBeDefined()
        f.offline = false
        button(language.risuNest.serverSync.syncNow)!.click(); await settle()
        // Three launch attempts, then one from Sync now.
        expect(commands().filter(command => command === 'server_sync_lww_activate')).toHaveLength(4)
        expect(f.invoke).toHaveBeenCalledWith('server_sync_lww_retry', { request: { bindingAuthority: '4', requestId: expect.any(String) } })
        await vi.waitFor(() => expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: '' }))
    })
    it('disconnects the actual persisted binding while activation remains offline, without starting services', async () => {
        await offlineStartup(); await settings()
        vi.mocked((await import('./serverAssetResidency')).getAssetResidencyStatus).mockRejectedValueOnce(new Error('server-unreachable'))
        button(language.risuNest.serverSync.disconnect)!.click(); await settle()
        // The file check cannot answer here, so the disconnect is confirmed before it unbinds.
        expect((await import('src/ts/alert')).alertCheckboxConfirm).toHaveBeenCalledOnce()
        expect(f.binding.target).toEqual({ kind: 'none' })
        expect(production.getServerSyncController().snapshot().status.bound).toBe(false)
        expect(button(language.risuNest.serverSync.syncNow)).toBeUndefined(); expect(button(language.risuNest.serverSync.disconnect)).toBeUndefined()
        expect(commands()).not.toContain('server_sync_lww_retry'); expect(commands()).not.toContain('server_sync_notify_start')
    })
    it('rejects a different current target before retry can send the old server header', async () => {
        await offlineStartup(); f.invoke.mockClear()
        f.binding = { ...f.binding, target: { kind: 'external', connectionId: 'other' }, targetAuthority: '8', selectionEpoch: 'other' }
        await expect(production.retryServerSync()).rejects.toThrow('Sync binding is unavailable')
        expect(commands()).not.toContain('server_sync_lww_activate'); expect(commands()).not.toContain('server_sync_lww_retry')
    })
    it('keeps a repeated offline retry recoverable without reinstalling or changing registration', async () => {
        await offlineStartup(); await production.installServerSyncProduction()
        await expect(production.retryServerSync()).rejects.toMatchObject({ code: 'server-unreachable', status: 503, retryable: true })
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: 'server-unreachable' })
        expect(commands()).not.toContain('server_sync_lww_retry'); expect(commands()).not.toContain('server_sync_configure')
        f.offline = false; await production.retryServerSync()
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: '' })
        // Three launch attempts, then one for each manual retry, which does not retry quickly again.
        expect(commands().filter(command => command === 'server_sync_lww_activate')).toHaveLength(5)
    })
    it('preserves native accepted-clock refusal after resumed activation and does not recheck it on foreground return', async () => {
        await offlineStartup(); f.offline = false; f.repairError = { code: 'accepted-clock-correction-required', retryable: false }
        await expect(production.retryServerSync()).rejects.toEqual(f.repairError)
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: true, error: 'accepted-clock-correction-required' })
        const retried = commands().filter(command => command === 'server_sync_lww_retry').length
        Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true }); document.dispatchEvent(new Event('visibilitychange')); await settle()
        expect(commands().filter(command => command === 'server_sync_lww_retry')).toHaveLength(retried)
        expect(commands()).not.toContain('server_sync_notify_start')
    })
    it.each([
        { failure: { code: 'server-timeout', status: 503, retryable: true }, attempts: 3 },
        { failure: { code: 'directory-unreachable', status: 503, retryable: true }, attempts: 3 },
        { failure: { code: 'server-response-error', status: 502, retryable: true }, attempts: 3 },
        // A server update, retries already spent and rate limits are not retried quickly.
        { failure: { code: 'server-updating', status: 503, retryable: true }, attempts: 1 },
        { failure: { code: 'sync-retry-budget-exhausted', status: 503, retryable: true }, attempts: 1 },
        // The activation's server check answered with a rate limit or a coded server failure.
        { failure: { code: 'server-response-error', status: 429, retryable: true, startupLocalReady: true }, attempts: 1 },
        { failure: { code: 'worker-unavailable', status: 503, retryable: true, startupLocalReady: false }, attempts: 3 },
    ])('automatically activates the persisted binding after transient $failure.code ($failure.status) recovers', async ({ failure, attempts }) => {
        vi.useFakeTimers()
        try {
            f.activationError = failure
            Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true })
            const installed = production.installServerSyncProduction()
            // Quick launch retries wait 1 s and then 2 s.
            await vi.advanceTimersByTimeAsync(attempts === 3 ? 3_000 : 0)
            await expect(installed).resolves.toBeUndefined()
            expect(commands().filter(command => command === 'server_sync_lww_activate')).toHaveLength(attempts)
            const transfers = () => commands().filter(command => ['server_sync_lww_push', 'server_sync_lww_pull', 'server_sync_lww_hydrate', 'server_sync_notify_start'].includes(command))
            expect(transfers()).toEqual([])
            expect(commands()).not.toContain('pds_lww_switch_target')
            expect(commands()).toContain('server_sync_claim_offline_notice')
            expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, running: false, paused: false, error: failure.code })
            await vi.advanceTimersByTimeAsync(5000)
            expect(transfers()).toEqual([])
            f.activationError = undefined; f.offline = false
            await vi.advanceTimersByTimeAsync(9999)
            expect(transfers()).toEqual([])
            await vi.advanceTimersByTimeAsync(1); await settle()
            expect(transfers()).toContain('server_sync_lww_pull')
            expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: '' })
        } finally { vi.useRealTimers() }
    })
    it.each([
        { code: 'library-operation-busy', status: 409, retryable: true },
        { code: 'server-sync-busy', status: 409, retryable: true },
        { code: 'asset-jobs-busy', status: 409, retryable: true },
        { code: 'local-store-unavailable', status: 503, retryable: true },
    ])('activates the persisted binding again once the local store recovers from $code, without the offline notice', async failure => {
        f.offline = false
        const base = f.invoke.getMockImplementation()!
        let busy = true
        f.invoke.mockImplementation(async (command, args) => {
            if (command === 'server_sync_lww_activate' && busy) { busy = false; throw failure }
            return base(command, args)
        })
        Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true })
        await expect(production.installServerSyncProduction()).resolves.toBeUndefined(); await settle()
        expect(commands()).not.toContain('server_sync_lww_fence')
        expect(commands().filter(command => command === 'server_sync_lww_activate')).toHaveLength(2)
        expect(commands()).toContain('server_sync_lww_pull')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: false, error: '' })
        expect(commands()).not.toContain('server_sync_claim_offline_notice')
    })
    it('preserves the selected binding when disconnect fencing fails after an offline startup', async () => {
        await offlineStartup()
        const before = structuredClone(f.binding)
        f.fenceError = { code: 'server-unreachable', status: 503, retryable: true }
        await expect(production.disconnectServerSync()).rejects.toEqual(f.fenceError)
        expect(commands()).not.toContain('server_sync_notify_start'); expect(commands()).not.toContain('pds_lww_switch_target')
        expect(f.binding).toEqual(before)
        f.fenceError = undefined
        await production.disconnectServerSync()
        expect(f.binding.target).toEqual({ kind: 'none' })
    })
    it.each([
        { code: 'equal-stamp-integrity', retryable: false },
        { code: 'binding-authority-stale', retryable: false },
    ])('rejects startup when fencing a refused activation fails with $code', async fenceError => {
        f.activationError = { code: 'local-validation', retryable: false }
        f.fenceError = fenceError
        const before = structuredClone(f.binding)
        await expect(production.installServerSyncProduction()).rejects.toBeInstanceOf(AggregateError)
        expect(commands()).not.toContain('server_sync_notify_start')
        expect(f.binding).toEqual(before)
    })
    it.each([
        [{ code: 'unauthorized', status: 401, retryable: false }, language.lwwSync.registrationRevoked],
        [{ code: 'invalid-device-token', status: 401, retryable: false }, language.lwwSync.registrationRevoked],
        [{ code: 'server-epoch-changed', status: 409, retryable: false }, language.lwwSync.registrationRevoked],
        [{ code: 'device-credential-unavailable', status: 409, retryable: false }, language.risuNest.serverSync.credentialUnavailable],
    ])('opens the library and asks for a registration in settings when startup activation fails with %j', async (failure, text) => {
        f.activationError = failure
        await expect(production.installServerSyncProduction()).resolves.toBeUndefined()
        expect(commands()).toContain('server_sync_lww_fence')
        expect(commands()).not.toContain('server_sync_notify_start'); expect(commands()).not.toContain('pds_lww_switch_target')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, running: false, paused: true, error: failure.code })
        await settings()
        expect(document.body.textContent).toContain(text)
        expect(document.querySelector('#server-registration')).not.toBeNull()
        expect(button(language.risuNest.serverSync.disconnect)).toBeDefined()
    })
    it.each([
        { code: 'binding-authority-stale', retryable: false },
        { code: 'server-unreachable', retryable: false },
        { code: 'equal-stamp-integrity', retryable: false },
        { code: 'local-metadata', status: 503, retryable: true },
        new Error('Local activation state is invalid'),
    ])('rejects non-recoverable or unknown activation failures after fencing startup jobs (%j)', async failure => {
        f.activationError = failure
        const before = structuredClone(f.binding)
        await expect(production.installServerSyncProduction()).rejects.toBe(failure)
        expect(commands()).toContain('server_sync_lww_fence')
        expect(commands()).not.toContain('server_sync_notify_start')
        expect(commands()).not.toContain('pds_lww_switch_target')
        expect(f.binding).toEqual(before)
    })
})

describe('first binding stopped after its target switch', () => {
    const pending = (serverEmpty: boolean) => ({ endpoint: 'https://synthetic.invalid', libraryId: 'library', epoch: 'epoch', serverEmpty })
    beforeEach(() => { f.configured = false; f.offline = false })
    it('finishes on its own through the binding flow while the server is still empty', async () => {
        f.pending = pending(true)
        await production.installServerSyncProduction()
        expect(commands()).toContain('server_sync_lww_inspect'); expect(commands()).toContain('server_sync_lww_activate')
        expect(commands()).not.toContain('server_sync_configure'); expect(commands()).not.toContain('pds_lww_switch_target')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { configured: true, bound: true }, bindingIncomplete: false })
        await settings()
        expect(document.body.textContent).not.toContain(language.lwwSync.bindingIncomplete)
    })
    it('waits for Connect when the server may no longer be the empty one it inspected, and Connect finishes it', async () => {
        f.pending = pending(false)
        await production.installServerSyncProduction()
        expect(commands()).toContain('server_sync_lww_pending_binding')
        expect(commands()).not.toContain('server_sync_lww_inspect'); expect(commands()).not.toContain('server_sync_lww_activate')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { configured: false }, bindingIncomplete: true })
        await settings()
        expect(document.body.textContent).toContain(language.lwwSync.bindingIncomplete)
        expect(button(language.risuNest.serverSync.syncNow)).toBeUndefined()
        button(language.risuNest.serverSync.connect)!.click()
        await vi.waitFor(() => expect(document.body.textContent).not.toContain(language.lwwSync.bindingIncomplete))
        expect(commands()).toContain('server_sync_lww_inspect'); expect(commands()).toContain('server_sync_lww_activate')
        expect(commands()).not.toContain('server_sync_configure'); expect(commands()).not.toContain('pds_lww_switch_target')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { configured: true }, bindingIncomplete: false })
        expect(document.body.textContent).not.toContain(language.lwwSync.bindingIncomplete)
    })
    it('keeps startup going and waits for Connect when finishing on its own fails', async () => {
        f.pending = pending(true); f.inspectError = { code: 'server-unreachable', status: 503, retryable: true }
        await expect(production.installServerSyncProduction()).resolves.toBeUndefined()
        expect(commands()).not.toContain('server_sync_lww_activate')
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { configured: false }, bindingIncomplete: true })
    })
    it('changes nothing when no first binding stopped', async () => {
        await production.installServerSyncProduction()
        expect(commands()).toContain('server_sync_lww_pending_binding')
        expect(commands()).not.toContain('server_sync_lww_inspect')
        expect(production.getServerSyncController().snapshot().bindingIncomplete).toBe(false)
    })
    it('never looks for a stopped first binding once the server is configured', async () => {
        f.configured = true
        await production.installServerSyncProduction()
        expect(commands()).not.toContain('server_sync_lww_pending_binding')
    })
})

describe('initial publication owed by a first binding that stopped after its switch', () => {
    const config = { endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId: 'device', token: 'synthetic' }
    beforeEach(() => {
        f.binding = { target: { kind: 'none' }, targetAuthority: '0', selectionEpoch: 'none', libraryId: null, progress: null }
        f.configured = false; f.offline = false
    })
    async function restartVisible() {
        production.disposeNativeSyncBindings(); vi.resetModules(); f.invoke.mockClear()
        Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true })
        production = await import('./serverSyncProduction'); production.initializeNativeSyncBindings()
        await production.installServerSyncProduction(); await settle()
    }
    it.each([true, false])('carries the decision made at the switch through a restart to one publication (shared data %s)', async shared => {
        f.sharedData = shared
        await production.installServerSyncProduction()
        f.activationError = { code: 'server-unreachable', status: 503, retryable: true }
        await expect(production.connectServerSync(config)).rejects.toMatchObject({ code: 'server-unreachable' })
        const switches = f.invoke.mock.calls.filter(([command]) => command === 'pds_lww_switch_target')
        expect(switches).toHaveLength(1)
        expect(switches[0][1].request).toMatchObject({ target: { kind: 'server', connectionId: 'server' }, initialPublication: shared })
        expect(f.marker).toBe(shared); expect(f.queued).toBe(0); expect(f.pushed).toBe(0)
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: true, error: 'server-unreachable' })

        f.activationError = undefined
        await restartVisible()
        expect(commands()).toContain('server_sync_lww_pending_binding')
        expect(commands()).toContain('server_sync_lww_inspect')
        expect(commands()).not.toContain('pds_lww_switch_target')
        await vi.waitFor(() => expect(f.pushed).toBe(shared ? 2 : 0))
        expect(f.marker).toBe(false); expect(f.queued).toBe(shared ? 1 : 0); expect(f.outbox).toBe(0)
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { configured: true, bound: true }, paused: false, error: '', bindingIncomplete: false })
    })
})

async function readCode(deviceId: string, text = language) {
    const { encodeServerRegistration } = await import('./serverSyncRegistration')
    const input = document.querySelector('textarea')!
    input.value = encodeServerRegistration({ endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId, token: 'ab'.repeat(32) })
    input.dispatchEvent(new Event('input', { bubbles: true })); await settle()
    button(text.risuNest.serverSync.readRegistration)!.click(); await settle()
}

describe('a duplicate device connecting again with a registration code', () => {
    it('refuses a used code before reading the library and keeps connecting as a new device available', async () => {
        f.offline = false; f.pushError = { code: 'writer-collision', status: 409, retryable: false }
        Object.defineProperty(document, 'visibilityState', { value: 'visible', configurable: true })
        await production.installServerSyncProduction()
        await vi.waitFor(() => expect(production.getServerSyncController().snapshot()).toMatchObject({ paused: true, error: 'writer-collision', blockedCode: 'writer-collision' }))
        await settings()
        expect(document.body.textContent).toContain(language.lwwSync.writerCollision)
        await readCode('reused')
        f.invoke.mockClear(); f.inspectError = { code: 'registration-integrity', status: 409, retryable: false }
        button(language.lwwSync.newDeviceAction)!.click()
        await vi.waitFor(() => expect(document.querySelector('[role="alert"]')?.textContent).toBe(language.lwwSync.registrationUsed))
        expect(f.invoke).toHaveBeenCalledWith('server_sync_lww_inspect', { request: { bindingAuthority: '4', requestId: expect.any(String) }, newDevice: true })
        for (const command of ['server_sync_lww_stage_target', 'server_sync_lww_fence', 'server_sync_lww_prepare_new_device', 'pds_lww_switch_target']) expect(commands()).not.toContain(command)
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: true }, paused: true, error: 'registration-integrity', blockedCode: 'writer-collision' })
        expect(document.querySelector('textarea')).not.toBeNull()
        await readCode('reused')
        expect(button(language.lwwSync.newDeviceAction)).toBeDefined()
        f.invoke.mockClear(); f.inspectError = { code: 'registration-used', status: 409, retryable: false }
        button(language.risuNest.serverSync.connect)!.click()
        await vi.waitFor(() => expect(document.querySelector('textarea')).not.toBeNull())
        expect(f.invoke).toHaveBeenCalledWith('server_sync_lww_inspect', { request: { bindingAuthority: '4', requestId: expect.any(String) }, newDevice: false })
        expect(commands()).not.toContain('server_sync_lww_stage_target')
        expect(document.querySelector('[role="alert"]')?.textContent).toBe(language.lwwSync.registrationUsed)
    })
})

describe('a new installation connecting with a registration code another installation used', () => {
    it.each(['settings', 'onboarding'] as const)('is refused before the library is read and asks for a new code in %s', async tone => {
        f.binding = { target: { kind: 'none' }, targetAuthority: '0', selectionEpoch: 'none', libraryId: null, progress: null }
        f.configured = false; f.offline = false
        // English words both tones the same, so the page runs in Korean, where they differ.
        const lang = await import('src/lang')
        lang.changeLanguage('ko')
        const text = lang.language
        expect(text.lwwSync.registrationUsedOnboarding).not.toBe(text.lwwSync.registrationUsed)
        await production.installServerSyncProduction()
        const Settings = (await import('src/lib/Setting/Pages/ServerSyncSettings.svelte')).default
        component = ui.mount(Settings, { target: document.body, props: { tone } }); await settle()
        await readCode('used', text)
        f.invoke.mockClear(); f.inspectError = { code: 'registration-used', status: 409, retryable: false }
        button(text.risuNest.serverSync.connect)!.click()
        await vi.waitFor(() => expect(document.querySelector('[role="alert"]')?.textContent).toBe(tone === 'onboarding' ? text.lwwSync.registrationUsedOnboarding : text.lwwSync.registrationUsed))
        expect(f.invoke).toHaveBeenCalledWith('server_sync_lww_inspect', { request: { bindingAuthority: '0', requestId: expect.any(String) }, newDevice: false })
        for (const command of ['server_sync_lww_stage_target', 'server_sync_lww_fence', 'pds_lww_switch_target', 'server_sync_lww_activate']) expect(commands()).not.toContain(command)
        expect(production.getServerSyncController().snapshot()).toMatchObject({ status: { bound: false }, error: 'registration-used' })
        expect(document.querySelector('textarea')).not.toBeNull()
        expect(button(text.risuNest.serverSync.connect)).toBeUndefined()
        expect(button(text.lwwSync.newDeviceAction)).toBeUndefined()
    })
})

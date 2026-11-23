// @vitest-environment happy-dom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, settled as domSettled, unmount } from 'svelte'
import { languageEnglish } from 'src/lang/en'
import type { PlatformShape } from 'src/ts/platform.testSupport'
const f = vi.hoisted(() => ({ pending: vi.fn(async (): Promise<number | undefined> => undefined), ensure: vi.fn(), connect: vi.fn(), dismiss: vi.fn(), complete: vi.fn(), configure: vi.fn(), bind: vi.fn(), disconnect: vi.fn(), hold: vi.fn(), release: vi.fn(), status: vi.fn(), policy: vi.fn(), evict: vi.fn(), cancel: vi.fn(), checkbox: vi.fn(), action: vi.fn(), native: true, scan: false, scanner: { scan: vi.fn(), cancel: vi.fn() }, switchFormat: vi.fn(), retry: vi.fn(), os: 'windows', listeners: new Set<(value: Record<string, unknown>) => void>(), bindingState: vi.fn(), preset: vi.fn(), state: { db: {} as Record<string, unknown> }, view: { status: { configured: false }, paused: false } as Record<string, unknown> }))
vi.mock('src/ts/platform', async () => (await import('src/ts/platform.testSupport')).platformModule(() => ({ runtime: f.native ? 'tauri' : 'web', os: f.os as PlatformShape['os'] })))
vi.mock('@tauri-apps/plugin-os', () => ({ platform: () => f.os }))
vi.mock('src/ts/alert', () => ({ alertConfirm: vi.fn(), alertCheckboxConfirm: f.checkbox, alertActionConfirm: f.action, alertError: vi.fn(), alertNormal: vi.fn(), openRisuAccountLogin: vi.fn() }))
vi.mock('src/ts/storage/sync/serverSyncProduction', () => ({
    connectServerSync: f.connect, dismissServerSyncConnectionFailure: f.dismiss, completeServerSyncBinding: f.complete, configureServerSyncConnection: f.configure, disconnectServerSync: f.disconnect, retryServerSync: f.retry, switchServerSyncLibraryFormat: f.switchFormat, holdServerSync: f.hold,
    getServerSyncCacheUsage: vi.fn(), cleanupServerSyncCache: vi.fn(),
    getServerSyncController: () => ({ snapshot: () => f.view, subscribe: (listener: (value: Record<string, unknown>) => void) => { f.listeners.add(listener); listener(f.view); return () => { f.listeners.delete(listener) } }, ensureStatus: f.ensure, watchProgress: () => () => {}, pendingChanges: f.pending, track: (_stage: string, operation: () => Promise<unknown>) => operation() }),
}))
vi.mock('src/ts/storage/sync/serverAssetResidency', () => ({ getAssetResidencyStatus: f.status, setAssetResidencyPolicy: f.policy, evictLocalAssets: f.evict, cancelLocalAssetCleanup: f.cancel, cancelAssetResidencyOperation: f.cancel }))
vi.mock('src/ts/storage/sync/serverSyncRegistration', () => ({ parseServerRegistration: () => ({ endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId: 'registration', token: 'synthetic' }) }))
vi.mock('src/ts/storage/sync/serverSyncQr', () => ({ get canScanServerRegistration() { return f.scan }, createServerQrScanner: () => f.scanner }))
vi.mock('src/ts/storage/sync/bindingRegistry', () => ({ bindSyncTarget: f.bind }))
vi.mock('src/ts/storage/sync/bindingNative', () => ({ createNativeSyncBindingBridge: () => ({ state: f.bindingState }) }))
vi.mock('src/ts/storage/sync/nativeOfficialAccountOperations', () => ({ restoreNativeOfficialAccountBackup: vi.fn() }))
vi.mock('src/ts/storage/sync/nativeOfficialAccountFlow', () => ({ NativeAccountLoginError: class extends Error {}, getNativeOfficialAccountFlow: vi.fn() }))
vi.mock('src/ts/storage/fileOperationErrorPresentation', () => ({ presentFileOperationError: vi.fn() }))
vi.mock('src/ts/characterCards', () => ({ hubURL: 'https://synthetic.invalid' }))
vi.mock('src/ts/globalApi.svelte', () => ({ getVersionString: () => 'synthetic' }))
vi.mock('src/ts/gui/colorscheme', () => ({ updateTextThemeAndCSS: vi.fn() }))
vi.mock('src/ts/gui/nativeFileJobDialogModel', async importOriginal => ({ formatBytes: (await importOriginal<typeof import('src/ts/gui/nativeFileJobDialogModel')>()).formatBytes, buildNativeFileJobDialogModel: () => ({ open: false }), formatElapsed: (milliseconds: number) => `${Math.floor(milliseconds / 1000)}s` }))
vi.mock('src/ts/process/templates/templates', () => ({ prebuiltPresets: {} }))
vi.mock('src/ts/storage/database.svelte', () => ({ setPreset: f.preset }))
vi.mock('src/ts/storage/nativeFileJobManager', async () => { const { writable } = await import('svelte/store'); return { cancelActiveNativeFileOperation: vi.fn(), dismissNativeFileOperationOutcome: vi.fn(), nativeFileJobHost: writable('dialog'), nativeFileOperation: writable(null), nativeFileOperationOutcome: writable(null) } })
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async () => ({ persistentWorkingSetRefreshRevision: (await import('svelte/store')).readable(null) }))
vi.mock('src/ts/storage/officialAccountMessage',() => ({ isExpectedHubMessage: vi.fn(), resolveExpectedOfficialAccountMessageUrl: vi.fn() }))
vi.mock('src/ts/storage/portableBackupFileRouteProduction.svelte', () => ({ restoreBackupFromSystemPicker: vi.fn() }))
vi.mock('src/ts/storage/risuSaveFileRouteProduction.svelte', () => ({ importRisuSaveFromSystemPicker: vi.fn() }))
vi.mock('src/ts/storage/sync/external/bridge', () => ({ getExternalStorageBridge: () => ({}) }))
vi.mock('src/ts/storage/sync/external/production', () => ({ refreshExternalStorageProductionState: vi.fn(), requestExternalStorageRestore: vi.fn() }))
vi.mock('src/lib/Setting/ExternalStorage/ConnectionForm.svelte', () => ({ default: () => {} }))
vi.mock('src/lib/Others/Onboarding/onboardingWeave', () => ({ observeOnboardingWeave: () => () => {} }))
vi.mock('src/ts/stores.svelte', () => ({ DBState: f.state }))
import ServerSyncSettings from './ServerSyncSettings.svelte'
import { serverRegistrationInbox } from 'src/ts/storage/sync/serverSyncRegistrationInbox'
import { assetCleanupProgress, type AssetCleanupState } from 'src/ts/storage/sync/serverAssetCleanupProgress'
import type { AssetResidencyStatus } from 'src/ts/storage/sync/serverAssetResidency'
import { assetResidencyStatus } from 'src/ts/storage/sync/serverAssetResidency.testSupport'
import { serverSyncLane as lane } from 'src/ts/storage/sync/serverSyncProgress.testSupport'
import { QrScanError } from 'src/ts/ui/qrScanner'
import Onboarding from 'src/lib/Others/Onboarding/Onboarding.svelte'
let component: ReturnType<typeof mount> | undefined
let host: HTMLDivElement
beforeEach(() => {
    serverRegistrationInbox.clear()
    assetCleanupProgress.set(undefined)
    vi.clearAllMocks(); f.native = true; f.scan = false; f.os = 'windows'; f.listeners.clear(); f.view = { status: { configured: false }, paused: false }; host = document.createElement('div'); document.body.append(host)
    for (const mock of [f.disconnect, f.hold, f.release, f.status, f.policy, f.evict, f.cancel, f.checkbox, f.action, f.complete, f.ensure, f.switchFormat, f.retry]) mock.mockReset()
    f.hold.mockResolvedValue(f.release)
    f.state.db = { language: 'en', characters: [] }
    f.preset.mockImplementation((db: Record<string, unknown>) => ({ ...db, preset: 'starting' }))
    f.bindingState.mockResolvedValue({ target: { kind: 'none' } })
    f.pending.mockResolvedValue(undefined)
})
afterEach(async () => { if (component) await unmount(component); component = undefined; host.remove() })
const settle = async () => { for (let i = 0; i < 12; i++) await domSettled() }
function click(text: string) { const button = [...host.querySelectorAll('button')].find(button => button.textContent?.trim() === text); expect(button).toBeDefined(); button!.click() }
async function registration() { await domSettled(); const input = host.querySelector('textarea')!; input.value = 'synthetic-registration'; input.dispatchEvent(new Event('input', { bubbles: true })); await domSettled(); click(languageEnglish.risuNest.serverSync.readRegistration); await domSettled() }
function publish(view: Record<string, unknown>) { f.view = view; for (const listener of f.listeners) listener(view) }
const findButton = (text: string) => [...host.querySelectorAll('button')].find(button => button.textContent?.trim() === text)
it('shows cleanup phases immediately, survives remount, and refreshes after cancellation', async () => {
    const copy = languageEnglish.risuNest.serverSync
    f.view = { status: { configured: true, bound: true, assetPolicy: 'remote' }, running: false, paused: false }
    const status = assetResidencyStatus({ policy: 'remote', localBytes: 8192 })
    f.status.mockResolvedValue(status)
    const reading: AssetCleanupState = { operationId: 'synthetic', state: 'running', sequence: 0, stage: 'preparing', completedItems: 0, totalItems: null, evictedItems: 0, evictedBytes: 0 }
    let finish!: () => void
    f.evict.mockImplementation(() => { assetCleanupProgress.set(reading); return new Promise<void>(resolve => { finish = resolve }) })
    component = mount(ServerSyncSettings, { target: host }); await settle()
    click(copy.residency.clean); await settle()
    const panel = () => host.querySelector('[data-asset-cleanup-progress]')!
    expect(panel().textContent).toContain(copy.residency.cleanup.preparing)
    expect(panel().querySelector('[role="progressbar"]')!.hasAttribute('aria-valuenow')).toBe(false)
    expect(findButton(copy.residency.clean)?.disabled).toBe(true)
    assetCleanupProgress.set({ ...reading, stage: 'checking', completedItems: 2, totalItems: 4 }); await settle()
    expect(panel().textContent).toContain(copy.residency.cleanup.checking)
    expect(panel().querySelector('[role="progressbar"]')!.getAttribute('aria-valuenow')).toBe('50')
    assetCleanupProgress.set({ ...reading, stage: 'deleting', completedItems: 1, totalItems: 4, evictedItems: 1, evictedBytes: 4096 }); await settle()
    expect(panel().querySelector('details')?.open).toBe(false)
    await unmount(component); component = mount(ServerSyncSettings, { target: host }); await settle()
    expect(panel().textContent).toContain(copy.residency.cleanup.deleting)
    expect(findButton(copy.residency.clean)?.disabled).toBe(true)
    f.cancel.mockImplementation(() => assetCleanupProgress.update(value => value && { ...value, state: 'cancelling' }))
    click(copy.residency.cancel); await settle()
    expect(panel().textContent).toContain(copy.residency.cleanup.cancelling)
    expect(findButton(copy.residency.cancel)?.disabled).toBe(true)
    f.status.mockClear()
    assetCleanupProgress.update(value => value && { ...value, state: 'cancelled' }); await settle()
    expect(panel().textContent).toContain(copy.residency.cleanup.cancelled)
    expect(panel().querySelector('.lucide-loader-circle')).toBeNull()
    expect(f.status).toHaveBeenCalledOnce()
    expect(findButton(copy.residency.clean)?.disabled).toBe(false)
    finish(); await settle()
})
it.each([false, true])('refreshes capacity after a partial cleanup failure and preserves its error (refresh fails: %s)', async refreshFails => {
    const copy = languageEnglish.risuNest.serverSync
    f.view = { status: { configured: true, bound: true, assetPolicy: 'remote' }, running: false, paused: false }
    const status = assetResidencyStatus({ policy: 'remote', localBytes: 8192 })
    f.status.mockResolvedValue(status)
    component = mount(ServerSyncSettings, { target: host }); await settle()
    f.status.mockClear()
    f.evict.mockImplementation(async () => {
        assetCleanupProgress.set({ operationId: 'synthetic', state: 'failed', stage: 'deleting', sequence: 2, completedItems: 1, totalItems: 2, evictedItems: 1, evictedBytes: 4096 })
        if (refreshFails) f.status.mockRejectedValue({ code: 'writer-collision' })
        else f.status.mockResolvedValue({ ...status, localBytes: 4096, remoteBytes: 4096, remoteObjects: 1, serverBytes: 4096, serverObjects: 1 })
        throw { code: 'storage-io' }
    })
    click(copy.residency.clean); await settle()
    expect(f.status).toHaveBeenCalledOnce()
    expect(host.textContent).toContain(copy.residency.cleanup.failed)
    expect(host.textContent).toContain(copy.errorHelp)
    expect(host.textContent).not.toContain(languageEnglish.lwwSync.writerCollision)
    if (!refreshFails) {
        expect(host.textContent).not.toContain('8.0 KiB')
        expect(host.textContent).toContain('4.0 KiB')
    }
    expect(findButton(copy.residency.clean)?.disabled).toBe(false)
})
it.each(['finalizing', 'complete', 'failed'] as const)('renders cleanup %s without confusing processed files with removed files', async state => {
    const copy = languageEnglish.risuNest.serverSync
    f.view = { status: { configured: true, bound: true, assetPolicy: 'remote' }, running: false, paused: false }
    assetCleanupProgress.set({ operationId: 'synthetic', state: state === 'finalizing' ? 'running' : state, stage: 'finalizing', sequence: 2, completedItems: 4, totalItems: null, evictedItems: 3, evictedBytes: 4096 })
    component = mount(ServerSyncSettings, { target: host }); await settle()
    const panel = host.querySelector('[data-asset-cleanup-progress]')!
    expect(panel.textContent).toContain(copy.residency.cleanup[state])
    expect(panel.querySelector('[role="progressbar"]')!.getAttribute('aria-valuenow')).toBe(state === 'complete' ? '100' : null)
    if (state === 'complete') expect(panel.textContent).toContain(copy.count.replace('{0}', '3'))
    expect(findButton(copy.residency.clean)?.disabled).toBe(state === 'finalizing')
    expect(!!findButton(copy.residency.cancel)).toBe(state === 'finalizing')
})
it.each([
    { error: '', newDevice: false },
    { error: 'writer-collision', newDevice: false },
    { error: 'writer-collision', newDevice: true },
    { error: 'equal-stamp-integrity', newDevice: true },
])('uses the supplied onboarding action once with newDevice=$newDevice in state "$error"', async ({ error, newDevice }) => {
    f.view = { status: { configured: true, bound: !!error }, paused: !!error, error }
    const connectTarget = vi.fn(async () => {})
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
    await registration()
    click(newDevice ? languageEnglish.lwwSync.newDeviceAction : languageEnglish.risuNest.serverSync.connect)
    await settle()
    expect(connectTarget).toHaveBeenCalledExactlyOnceWith({ endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId: 'registration', token: 'synthetic' }, newDevice, error ? undefined : 'full')
    expect(f.connect).not.toHaveBeenCalled()
})
it.each(['', 'server-unreachable', 'clock-skew', 'unauthorized'])('offers no new-device connection outside the duplicate-device recovery state ("%s")', async error => {
    f.view = { status: { configured: true, bound: !!error }, paused: !!error, error }
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await registration()
    expect(findButton(languageEnglish.risuNest.serverSync.connect)).toBeDefined()
    expect(findButton(languageEnglish.lwwSync.newDeviceAction)).toBeUndefined()
})
describe('a registration code the server refuses', () => {
    it.each([false, true])('describes a writer collision for the current connection state (bound=%s)', async bound => {
        f.view = { status: { configured: true, bound }, paused: true, error: 'writer-collision' }
        component = mount(ServerSyncSettings, { target: host })
        await settle()
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(bound
            ? languageEnglish.lwwSync.writerCollision : languageEnglish.lwwSync.writerCollisionNotConnected)
        expect(f.connect).not.toHaveBeenCalled()
    })
    const usedCodes = ['registration-used', 'registration-integrity', 'registration-not-new']
    it.each(usedCodes.flatMap(error => [{ error, tone: 'settings' as const }, { error, tone: 'onboarding' as const }]))('asks for a new registration code for "$error" in $tone', async ({ error, tone }) => {
        f.view = { status: { configured: true, bound: false }, paused: true, error }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn(), tone } })
        await settle()
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(tone === 'onboarding' ? languageEnglish.lwwSync.registrationUsedOnboarding : languageEnglish.lwwSync.registrationUsed)
        expect(host.querySelector('#server-registration')).not.toBeNull()
    })
    it('names the new registration code a duplicate device needs', async () => {
        f.view = { status: { configured: true, bound: true }, paused: true, error: 'writer-collision', blockedCode: 'writer-collision' }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.writerCollision)
    })
    it.each(usedCodes.flatMap(error => [{ error, newDevice: false }, { error, newDevice: true }]))('returns to the code input when $error refuses a connection with newDevice=$newDevice', async ({ error, newDevice }) => {
        const stopped = { status: { configured: true, bound: true }, paused: true, error: 'writer-collision', blockedCode: 'writer-collision' }
        f.view = stopped
        const connectTarget = vi.fn(async (): Promise<void> => {
            publish({ ...stopped, error })
            throw Object.assign(new Error(error), { code: error, status: 409, retryable: false })
        })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await registration()
        click(newDevice ? languageEnglish.lwwSync.newDeviceAction : languageEnglish.risuNest.serverSync.connect)
        await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, newDevice, undefined)
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationUsed)
        expect(host.querySelector('#server-registration')).not.toBeNull()
        expect(findButton(languageEnglish.risuNest.serverSync.connect)).toBeUndefined()
        // Sync is still stopped by the duplicate device, so a new code can connect as a new device.
        await registration()
        expect(findButton(languageEnglish.lwwSync.newDeviceAction)).toBeDefined()
        connectTarget.mockImplementationOnce(async () => { publish({ status: { configured: true, bound: true }, paused: false, error: '' }) })
        click(languageEnglish.lwwSync.newDeviceAction)
        await settle()
        expect(connectTarget).toHaveBeenLastCalledWith(registered, true, undefined)
        expect(host.querySelector('#server-registration')).toBeNull()
    })
    it('returns to the code input when a first connection is refused', async () => {
        const connectTarget = vi.fn(async () => { throw Object.assign(new Error('registration-used'), { code: 'registration-used', status: 409, retryable: false }) })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await registration()
        click(languageEnglish.risuNest.serverSync.connect)
        await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationUsed)
        expect(host.querySelector('#server-registration')).not.toBeNull()
    })
    it('keeps the read code when the connection fails for another reason', async () => {
        const connectTarget = vi.fn(async () => { throw Object.assign(new Error('server-unreachable'), { code: 'server-unreachable', status: 503, retryable: true }) })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await registration()
        click(languageEnglish.risuNest.serverSync.connect)
        await settle()
        expect(host.querySelector('[role="alert"]')).not.toBeNull()
        expect(host.querySelector('#server-registration')).toBeNull()
        expect(findButton(languageEnglish.risuNest.serverSync.connect)).toBeDefined()
    })
    it('keeps onboarding on the code input when the code is refused', async () => {
        f.connect.mockRejectedValue(Object.assign(new Error('registration-used'), { code: 'registration-used', status: 409, retryable: false }))
        await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
        expect(f.connect).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
        expect(heading()).toBe(onboarding.hub.title)
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationUsedOnboarding)
        expect(host.querySelector('#server-registration')).not.toBeNull()
        expect(f.dismiss).not.toHaveBeenCalled()
    })
    it('drops the refusal once onboarding leaves the server screen without connecting', async () => {
        f.connect.mockRejectedValue(Object.assign(new Error('registration-used'), { code: 'registration-used', status: 409, retryable: false }))
        await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
        expect(f.dismiss).not.toHaveBeenCalled()
        click(onboarding.back); await settle()
        expect(heading()).not.toBe(onboarding.hub.title)
        expect(f.dismiss).toHaveBeenCalledOnce()
    })
    describe('reading another code after a refusal', () => {
        const refusal = (error: string) => vi.fn(async (): Promise<void> => {
            publish({ status: { configured: true, bound: false }, paused: true, error })
            throw Object.assign(new Error(error), { code: error, status: 409, retryable: false })
        })
        // Stands in for production, which drops the failure of an attempt that left the device unconnected.
        beforeEach(() => { f.dismiss.mockImplementation(() => publish({ ...f.view, error: '' })) })
        afterEach(() => { f.dismiss.mockReset() })
        it.each(usedCodes.flatMap(error => [{ error, tone: 'settings' as const }, { error, tone: 'onboarding' as const }]))('drops the "$error" refusal once a new code is read in $tone', async ({ error, tone }) => {
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: refusal(error), tone } })
            await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
            expect(host.querySelector('[role="alert"]')?.textContent).toBe(tone === 'onboarding' ? languageEnglish.lwwSync.registrationUsedOnboarding : languageEnglish.lwwSync.registrationUsed)
            await registration(); await settle()
            expect(host.textContent).toContain(languageEnglish.risuNest.serverSync.reviewTitle)
            expect(host.querySelector('[role="alert"]')).toBeNull()
            expect(f.dismiss).toHaveBeenCalledOnce()
        })
        it('drops the refusal once a new code is scanned', async () => {
            f.scan = true
            f.scanner.scan.mockResolvedValue(registered)
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: refusal('registration-used') } })
            await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
            expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationUsed)
            click(languageEnglish.risuNest.serverSync.scanRegistration); await settle()
            expect(host.textContent).toContain(languageEnglish.risuNest.serverSync.reviewTitle)
            expect(host.querySelector('[role="alert"]')).toBeNull()
            expect(f.dismiss).toHaveBeenCalledOnce()
        })
        it('drops the refusal once a registration link delivers a new code', async () => {
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: refusal('registration-used') } })
            await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
            expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationUsed)
            serverRegistrationInbox.stage('synthetic-new-registration-link')
            await settle()
            expect(host.textContent).toContain(languageEnglish.risuNest.serverSync.reviewTitle)
            expect(host.querySelector('[role="alert"]')).toBeNull()
        })
        it.each(['server-unreachable', 'device-credential-unavailable', 'clock-skew'])('keeps the "%s" connection failure when a new code is read', async error => {
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: refusal(error) } })
            await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
            const shown = host.querySelector('[role="alert"]')?.textContent
            expect(shown).toBeTruthy()
            click(languageEnglish.risuNest.serverSync.discardRegistration); await settle()
            await registration(); await settle()
            expect(host.querySelector('[role="alert"]')?.textContent).toBe(shown)
            expect(f.dismiss).not.toHaveBeenCalled()
        })
    })
})
it('shows an unfinished connection with Connect, which finishes it with the saved registration', async () => {
    f.view = { status: { configured: false, bound: true }, paused: true, error: '', bindingIncomplete: true }
    f.complete.mockResolvedValue(undefined)
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    expect(host.querySelector('[role="status"]')?.textContent).toBe(languageEnglish.lwwSync.bindingIncomplete)
    expect(findButton(languageEnglish.risuNest.serverSync.syncNow)).toBeUndefined()
    expect(findButton(languageEnglish.risuNest.serverSync.disconnect)).toBeDefined()
    click(languageEnglish.risuNest.serverSync.connect)
    await settle()
    expect(f.complete).toHaveBeenCalledOnce()
    expect(f.connect).not.toHaveBeenCalled(); expect(f.bind).not.toHaveBeenCalled()
})
it('reports why Connect could not finish an unfinished connection', async () => {
    f.view = { status: { configured: false, bound: true }, paused: true, error: '', bindingIncomplete: true }
    f.complete.mockRejectedValue({ code: 'unauthorized' })
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    click(languageEnglish.risuNest.serverSync.connect)
    await settle()
    expect(host.querySelector('[role="alert"]')?.textContent).toBe(languageEnglish.lwwSync.registrationRevoked)
    expect(host.querySelector('[role="status"]')?.textContent).toBe(languageEnglish.lwwSync.bindingIncomplete)
})
it('shows no unfinished connection outside that state', async () => {
    f.view = { status: { configured: true, bound: true }, paused: false, error: '' }
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    expect(host.textContent).not.toContain(languageEnglish.lwwSync.bindingIncomplete)
    expect(findButton(languageEnglish.risuNest.serverSync.connect)).toBeUndefined()
    expect(findButton(languageEnglish.risuNest.serverSync.syncNow)).toBeDefined()
})
it('explains an item too large to send instead of the generic sync error', async () => {
    f.view = { status: { configured: true, bound: true }, paused: true, error: 'unit-too-large' }
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    const alert = host.querySelector('[role="alert"]')
    expect(alert?.textContent).toBe(languageEnglish.lwwSync.unitTooLarge)
    expect(alert?.textContent).not.toBe(languageEnglish.risuNest.serverSync.errorHelp)
})
it.each([
    { error: 'unauthorized', text: languageEnglish.lwwSync.registrationRevoked },
    { error: 'invalid-device-token', text: languageEnglish.lwwSync.registrationRevoked },
    { error: 'device-credential-unavailable', text: languageEnglish.risuNest.serverSync.credentialUnavailable },
    { error: 'previous-files-download-failed', text: languageEnglish.lwwSync.downloadFailedNotConnected },
    { error: 'previous-storage-unavailable', text: languageEnglish.lwwSync.previousStorageUnavailable },
    { error: 'library-format-switch-required', text: languageEnglish.lwwSync.formatSwitchRequiredHelp },
    { error: 'app-update-required', text: languageEnglish.lwwSync.appUpdateRequiredHelp },
    { error: 'sync-server-update-required', text: languageEnglish.lwwSync.serverUpdateRequiredHelp },
])('explains "$error" instead of the generic sync error', async ({ error, text }) => {
    f.view = { status: { configured: true, bound: true }, paused: true, error }
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    expect(host.querySelector('[role="alert"]')?.textContent).toBe(text)
})
describe('a library below this app data format', () => {
    const lww = languageEnglish.lwwSync
    const below = { status: { configured: true, bound: true }, paused: true, error: 'library-format-switch-required', blockedCode: 'library-format-switch-required' }
    async function open(view: Record<string, unknown> = below) {
        f.view = view
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
    }
    // A first publication that stops at the format level stops sync outside the scheduler, with no blocked code.
    it.each(['library-format-switch-required', ''])('names the stop and switches after the acknowledgement, then resumes sync (blocked code "%s")', async blockedCode => {
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        await open({ ...below, blockedCode })
        expect(host.textContent).toContain(lww.formatSwitchRequired)
        click(lww.formatSwitch); await settle()
        expect(f.checkbox).toHaveBeenCalledWith(expect.objectContaining({ title: lww.formatSwitchTitle, description: lww.formatSwitchDescription, checkboxLabel: lww.formatSwitchAcknowledge, actionLabel: lww.formatSwitch, requireChecked: true }))
        expect(f.switchFormat).toHaveBeenCalledOnce()
        expect(f.retry).toHaveBeenCalledOnce()
    })
    it('switches nothing when the question is cancelled', async () => {
        f.checkbox.mockResolvedValue({ confirmed: false, checked: false })
        await open()
        click(lww.formatSwitch); await settle()
        expect(f.switchFormat).not.toHaveBeenCalled(); expect(f.retry).not.toHaveBeenCalled()
    })
    it('explains a failed switch and stays paused', async () => {
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.switchFormat.mockRejectedValue({ code: 'server-unreachable', retryable: true })
        await open()
        click(lww.formatSwitch); await settle()
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(lww.formatSwitchFailed)
        expect(f.retry).not.toHaveBeenCalled()
        // A library already above this app is left to the sync attempt, which reports it.
        f.switchFormat.mockRejectedValue({ code: 'app-update-required', retryable: false })
        click(lww.formatSwitch); await settle()
        expect(host.textContent).not.toContain(lww.formatSwitchFailed)
        expect(f.retry).toHaveBeenCalledOnce()
    })
    it.each(['app-update-required', 'sync-server-update-required'])('offers no switch while %s stops sync', async code => {
        await open({ ...below, error: code, blockedCode: code })
        expect(findButton(lww.formatSwitch)).toBeUndefined()
        expect(host.textContent).toContain(code === 'app-update-required' ? lww.appUpdateRequired : lww.serverUpdateRequired)
    })
})
it.each(['linux', 'windows', 'macos', 'android', 'ios'])('names the Secret Service for an unreadable credential only on Linux (%s)', async os => {
    f.os = os
    f.view = { status: { configured: true, bound: true }, paused: true, error: 'device-credential-unavailable' }
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    const text = host.querySelector('[role="alert"]')?.textContent
    expect(text).toBe(os === 'linux' ? languageEnglish.risuNest.serverSync.credentialUnavailableLinux : languageEnglish.risuNest.serverSync.credentialUnavailable)
    expect(text?.includes('Secret Service')).toBe(os === 'linux')
})
it.each([false, true])('mentions the QR code in the connection help only where it can be scanned (%s)', async scan => {
    f.scan = scan
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    expect(host.textContent).toContain(scan ? languageEnglish.risuNest.serverSync.connectRowHelpScan : languageEnglish.risuNest.serverSync.connectRowHelp)
    expect(host.textContent?.includes('QR')).toBe(scan)
})
describe('scanning a registration QR code', () => {
    const sync = languageEnglish.risuNest.serverSync
    beforeEach(() => { f.scan = true })
    it('scans in the settings wording, and the scanned registration is reviewed before connecting', async () => {
        f.scanner.scan.mockResolvedValue({ endpoint: 'https://scanned.invalid', libraryId: 'scanned-library', deviceId: 'device', token: 'synthetic' })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        click(sync.scanRegistration)
        await settle()
        expect(f.scanner.scan).toHaveBeenCalledExactlyOnceWith('settings')
        expect(host.textContent).toContain(sync.reviewTitle)
        expect(host.textContent).toContain('https://scanned.invalid')
        expect(host.querySelector('[role="alert"]')).toBeNull()
    })
    it('marks the scan button busy while the camera runs and offers no second cancel below the camera screen', async () => {
        f.scanner.scan.mockReturnValue(new Promise(() => {}))
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        click(sync.scanRegistration)
        await settle()
        const buttons = [...host.querySelectorAll('button')]
        expect(buttons).toHaveLength(2)
        expect(buttons[1].textContent).toContain(sync.scanRegistration)
        expect(buttons[1].getAttribute('aria-busy')).toBe('true')
        expect(host.textContent).not.toContain(languageEnglish.cancel)
    })
    it('returns quietly from a cancelled scan and explains a failed one', async () => {
        f.scanner.scan.mockRejectedValueOnce(new QrScanError('qr-scan-cancelled')).mockRejectedValueOnce(new QrScanError('qr-scan-timeout'))
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        click(sync.scanRegistration)
        await settle()
        expect(host.querySelector('[role="alert"]')).toBeNull()
        expect(findButton(sync.scanRegistration)?.disabled).toBe(false)
        click(sync.scanRegistration)
        await settle()
        expect(host.querySelector('[role="alert"]')?.textContent).toBe(sync.cameraUnavailable)
    })
    it('ends a running scan when the screen closes', async () => {
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        await unmount(component); component = undefined
        expect(f.scanner.cancel).toHaveBeenCalledOnce()
    })
    it('scans in the onboarding wording from the onboarding screen', async () => {
        f.scanner.scan.mockReturnValue(new Promise(() => {}))
        await openOnboardingServer()
        click(sync.scanRegistration)
        await settle()
        expect(f.scanner.scan).toHaveBeenCalledExactlyOnceWith('onboarding')
    })
})
it('keeps settings on the existing production action by default', async () => {
    component = mount(ServerSyncSettings, { target: host })
    await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
    expect(f.connect).toHaveBeenCalledExactlyOnceWith({ endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId: 'registration', token: 'synthetic' }, false, 'full')
})
it('dismisses a registration without connecting', async () => {
    const connectTarget = vi.fn()
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
    await registration(); click(languageEnglish.risuNest.serverSync.discardRegistration); await settle()
    expect(connectTarget).not.toHaveBeenCalled(); expect(f.connect).not.toHaveBeenCalled()
})
it('hides the native server connection UI on web', async () => {
    f.native = false; component = mount(ServerSyncSettings, { target: host }); await domSettled()
    expect(host.querySelector('textarea')).toBeNull(); expect(host.querySelector('button')).toBeNull()
})

const sync = languageEnglish.risuNest.serverSync
const residencyStatus = (overrides: Partial<AssetResidencyStatus> = {}) => assetResidencyStatus({ policy: 'remote', remoteBytes: 4096, remoteObjects: 2, serverBytes: 4096, serverObjects: 2, ...overrides })
const externalOnly = { serverObjects: 0, serverBytes: 0, externalObjects: [{ connectionId: 'external', objects: 2 }] }
async function mountBound(status: AssetResidencyStatus = residencyStatus()) {
    f.view = { status: { configured: true, bound: true }, paused: false, error: '' }
    f.status.mockResolvedValue(status)
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
}
const alertText = () => host.querySelector('[role="alert"]')?.textContent
describe('disconnecting with files kept only on the server', () => {
    it('asks before disconnecting when no file is kept only on the server', async () => {
        await mountBound(residencyStatus({ remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        f.action.mockResolvedValue(true)
        click(sync.disconnect); await settle()
        expect(f.action).toHaveBeenCalledExactlyOnceWith({
            title: sync.disconnectTitle, description: sync.disconnectDescription, actionLabel: sync.disconnect, cancelLabel: languageEnglish.cancel,
        })
        expect(f.checkbox).not.toHaveBeenCalled(); expect(f.policy).not.toHaveBeenCalled()
        expect(f.disconnect).toHaveBeenCalledOnce()
    })
    it('keeps the connection when the question is cancelled', async () => {
        await mountBound(residencyStatus({ remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        f.action.mockResolvedValue(false)
        click(sync.disconnect); await settle()
        expect(f.action).toHaveBeenCalledOnce()
        expect(f.disconnect).not.toHaveBeenCalled()
    })
    it('shows the disconnect as running while it checks the files', async () => {
        await mountBound()
        f.status.mockReturnValue(new Promise(() => {}))
        click(sync.disconnect); await settle()
        const button = host.querySelector<HTMLButtonElement>('button[aria-busy="true"]')
        expect(button?.textContent).toContain(sync.disconnect)
        expect(button?.disabled).toBe(true)
        expect(f.checkbox).not.toHaveBeenCalled(); expect(f.action).not.toHaveBeenCalled()
        expect(f.disconnect).not.toHaveBeenCalled()
    })
    it('asks with the unknown wording once the file check takes more than a few seconds', async () => {
        await mountBound()
        vi.useFakeTimers({ toFake: ['setTimeout', 'clearTimeout'] })
        try {
            f.status.mockReturnValue(new Promise(() => {}))
            f.checkbox.mockResolvedValue({ confirmed: false, checked: false })
            click(sync.disconnect); await settle()
            await vi.advanceTimersByTimeAsync(2_900); await settle()
            expect(f.checkbox).not.toHaveBeenCalled()
            await vi.advanceTimersByTimeAsync(100); await settle()
            expect(f.checkbox).toHaveBeenCalledExactlyOnceWith({
                title: sync.disconnectTitle, description: sync.disconnectRemoteOnlyUnknown, checkboxLabel: sync.downloadThenDisconnect,
                actionLabel: sync.disconnect, cancelLabel: languageEnglish.cancel, requireChecked: false,
            })
            expect(host.querySelector('button[aria-busy="true"]')).toBeNull()
            expect(f.disconnect).not.toHaveBeenCalled()
        } finally {
            vi.useRealTimers()
        }
    })
    it('reads the current status when disconnecting and asks once with an unchecked download option', async () => {
        await mountBound(residencyStatus({ remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        f.status.mockResolvedValue(residencyStatus())
        f.checkbox.mockResolvedValue({ confirmed: false, checked: false })
        click(sync.disconnect); await settle()
        expect(f.checkbox).toHaveBeenCalledExactlyOnceWith({
            title: sync.disconnectTitle, description: sync.disconnectRemoteOnly, checkboxLabel: sync.downloadThenDisconnect,
            actionLabel: sync.disconnect, cancelLabel: languageEnglish.cancel, requireChecked: false,
        })
        expect(f.disconnect).not.toHaveBeenCalled(); expect(f.policy).not.toHaveBeenCalled(); expect(f.hold).not.toHaveBeenCalled()
    })
    it('disconnects without downloading when the download option stays unchecked', async () => {
        await mountBound()
        f.checkbox.mockResolvedValue({ confirmed: true, checked: false })
        click(sync.disconnect); await settle()
        expect(f.disconnect).toHaveBeenCalledOnce(); expect(f.policy).not.toHaveBeenCalled(); expect(f.hold).not.toHaveBeenCalled()
    })
    it('stops sync, downloads every file, then disconnects', async () => {
        await mountBound()
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockResolvedValue(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        click(sync.disconnect); await settle()
        expect(f.policy).toHaveBeenCalledExactlyOnceWith('full')
        expect(f.disconnect).toHaveBeenCalledOnce(); expect(f.release).toHaveBeenCalledOnce()
        const order = [f.hold, f.policy, f.disconnect, f.release].map(mock => mock.mock.invocationCallOrder[0])
        expect(order).toEqual([...order].sort((a, b) => a - b))
        expect(alertText()).toBeUndefined()
    })
    it('shows the busy text and the cancel button while the download runs', async () => {
        await mountBound()
        let finish!: () => void
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockImplementation(() => new Promise(resolve => { finish = () => resolve(residencyStatus({ policy: 'full', remoteObjects: 0, serverObjects: 0 })) }))
        click(sync.disconnect); await settle()
        expect(host.querySelector('[role="status"]')?.textContent).toBe(sync.residency.working)
        click(sync.residency.cancel); await settle()
        expect(f.cancel).toHaveBeenCalledOnce()
        finish(); await settle()
        expect(host.querySelector('[role="status"]')).toBeNull()
    })
    it('keeps the connection and explains it when the download fails', async () => {
        await mountBound()
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'server-unreachable', retryable: true })
        click(sync.disconnect); await settle()
        expect(f.disconnect).not.toHaveBeenCalled(); expect(f.release).toHaveBeenCalledOnce()
        expect(alertText()).toBe(sync.downloadFailedKeptConnection)
    })
    it('disconnects when the failed download leaves no file only on the server', async () => {
        await mountBound()
        f.status.mockResolvedValueOnce(residencyStatus()).mockResolvedValueOnce(residencyStatus({ remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0, unavailableObjects: 1 }))
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'required-asset-unavailable', retryable: false })
        click(sync.disconnect); await settle()
        expect(f.disconnect).toHaveBeenCalledOnce(); expect(f.release).toHaveBeenCalledOnce()
        const order = [f.hold, f.policy, f.disconnect, f.release].map(mock => mock.mock.invocationCallOrder[0])
        expect(order).toEqual([...order].sort((a, b) => a - b))
        expect(alertText()).toBeUndefined()
    })
    it.each([
        { name: 'still lists files only on the server', reread: () => f.status.mockResolvedValueOnce(residencyStatus()).mockResolvedValueOnce(residencyStatus({ remoteObjects: 1, serverObjects: 1, unavailableObjects: 1 })) },
        { name: 'cannot be read', reread: () => f.status.mockResolvedValueOnce(residencyStatus()).mockRejectedValueOnce(new Error('status-unavailable')) },
    ])('keeps the connection when the status after a failed download $name', async ({ reread }) => {
        await mountBound()
        reread()
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'required-asset-unavailable', retryable: false })
        click(sync.disconnect); await settle()
        expect(f.disconnect).not.toHaveBeenCalled(); expect(f.release).toHaveBeenCalledOnce()
        expect(alertText()).toBe(sync.downloadFailedKeptConnection)
    })
    it('keeps the connection without a message when the download is cancelled', async () => {
        await mountBound()
        f.status.mockResolvedValueOnce(residencyStatus()).mockResolvedValue(residencyStatus({ remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'cancelled', retryable: true })
        click(sync.disconnect); await settle()
        expect(f.disconnect).not.toHaveBeenCalled(); expect(f.release).toHaveBeenCalledOnce()
        expect(alertText()).toBeUndefined()
    })
    it('asks without the download option when the remaining files are kept only in external storage', async () => {
        await mountBound(residencyStatus(externalOnly))
        f.action.mockResolvedValue(true)
        click(sync.disconnect); await settle()
        expect(f.action).toHaveBeenCalledOnce()
        expect(f.checkbox).not.toHaveBeenCalled(); expect(f.policy).not.toHaveBeenCalled()
        expect(f.disconnect).toHaveBeenCalledOnce()
    })
    it('disconnects when the failed download leaves files only in external storage', async () => {
        await mountBound()
        f.status.mockResolvedValueOnce(residencyStatus()).mockResolvedValueOnce(residencyStatus({ ...externalOnly, remoteObjects: 1 }))
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'required-asset-unavailable', retryable: false })
        click(sync.disconnect); await settle()
        expect(f.disconnect).toHaveBeenCalledOnce()
        expect(alertText()).toBeUndefined()
    })
    it('keeps the download failure when the status refresh afterwards fails', async () => {
        await mountBound()
        f.status.mockResolvedValueOnce(residencyStatus()).mockResolvedValueOnce(residencyStatus()).mockRejectedValueOnce({ code: 'server-unreachable' })
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'server-unreachable', retryable: true })
        click(sync.disconnect); await settle()
        expect(f.status).toHaveBeenCalledTimes(4)
        expect(f.disconnect).not.toHaveBeenCalled()
        expect(alertText()).toBe(sync.downloadFailedKeptConnection)
    })
    it('keeps the download failure when the first status read fails after it', async () => {
        f.view = { status: { configured: true, bound: true }, paused: false, error: '' }
        let failMount!: (error: unknown) => void
        f.status.mockResolvedValue(residencyStatus()).mockImplementationOnce(() => new Promise((_, reject) => { failMount = reject }))
        f.checkbox.mockResolvedValue({ confirmed: true, checked: true })
        f.policy.mockRejectedValue({ code: 'server-unreachable', retryable: true })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        click(sync.disconnect); await settle()
        expect(alertText()).toBe(sync.downloadFailedKeptConnection)
        failMount({ code: 'server-unreachable' }); await settle()
        expect(alertText()).toBe(sync.downloadFailedKeptConnection)
    })
    it('asks with the unknown wording when the status cannot be read', async () => {
        await mountBound()
        f.status.mockRejectedValueOnce(new Error('status-unavailable'))
        f.checkbox.mockResolvedValue({ confirmed: true, checked: false })
        click(sync.disconnect); await settle()
        expect(f.checkbox).toHaveBeenCalledExactlyOnceWith(expect.objectContaining({ description: sync.disconnectRemoteOnlyUnknown, requireChecked: false }))
        expect(f.action).not.toHaveBeenCalled(); expect(f.policy).not.toHaveBeenCalled()
        expect(f.disconnect).toHaveBeenCalledOnce()
    })
    it('keeps the connection when the unknown question is cancelled', async () => {
        await mountBound()
        f.status.mockRejectedValueOnce(new Error('status-unavailable'))
        f.checkbox.mockResolvedValue({ confirmed: false, checked: false })
        click(sync.disconnect); await settle()
        expect(f.checkbox).toHaveBeenCalledOnce()
        expect(f.disconnect).not.toHaveBeenCalled()
    })
})
it('removes the asset storage controls once the server is disconnected', async () => {
    await mountBound(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
    expect(host.textContent).toContain(sync.residency.title)
    f.disconnect.mockImplementation(async () => { publish({ status: { configured: true, bound: false }, paused: true, error: '' }) })
    f.action.mockResolvedValue(true)
    click(sync.disconnect); await settle()
    expect(f.disconnect).toHaveBeenCalledOnce()
    expect(host.textContent).not.toContain(sync.residency.title)
    for (const label of [sync.residency.clean, sync.residency.download]) expect(findButton(label)).toBeUndefined()
    expect(host.querySelector('[role="radiogroup"]')).toBeNull()
})
it('changes the asset storage choice from its radio options', async () => {
    await mountBound(residencyStatus({ policy: 'full' }))
    const options = [...host.querySelectorAll<HTMLInputElement>('[role="radiogroup"] input[type="radio"]')]
    expect(options.map(option => [option.closest('label')?.textContent?.trim(), option.checked])).toEqual([[sync.residency.full, true], [sync.residency.remote, false]])
    f.policy.mockResolvedValue(residencyStatus({ policy: 'remote' })); f.status.mockResolvedValue(residencyStatus({ policy: 'remote' }))
    options[1].click(); await settle()
    expect(f.policy).toHaveBeenCalledExactlyOnceWith('remote')
    expect(options[1].checked).toBe(true)
})
it('shows no asset storage controls for a stored server registration that is not connected', async () => {
    f.view = { status: { configured: true, bound: false }, paused: true, error: '' }
    f.status.mockResolvedValue(residencyStatus({ policy: 'full' }))
    component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
    await settle()
    expect(host.textContent).not.toContain(sync.residency.title)
    expect(findButton(sync.residency.download)).toBeUndefined()
})
it('counts only files the server holds as kept only on the server', async () => {
    await mountBound(residencyStatus({ remoteBytes: 8 * 1024 * 1024, serverBytes: 4 * 1024 * 1024 }))
    const row = [...host.querySelectorAll('*')].find(node => node.children.length === 0 && node.textContent === sync.residency.remoteOnly)?.parentElement?.parentElement
    expect(row?.textContent).toContain('4.0 MiB')
    expect(host.textContent).not.toContain('8.0 MiB')
})
it('prints an empty server share in the same unit as external storage sizes', async () => {
    await mountBound(residencyStatus({ remoteBytes: 0, remoteObjects: 0, serverBytes: 0, serverObjects: 0 }))
    const row = [...host.querySelectorAll('*')].find(node => node.children.length === 0 && node.textContent === sync.residency.remoteOnly)?.parentElement?.parentElement
    expect(row?.textContent).toContain('0 B')
    expect(host.textContent).not.toContain('bytes')
})
describe('asset storage chosen before connecting', () => {
    const choices = () => [...host.querySelectorAll<HTMLInputElement>('[role="radiogroup"] input[type="radio"]')]
    const shownChoices = () => choices().map(option => [option.closest('label')?.textContent?.trim(), option.checked])
    it.each(['settings', 'onboarding'] as const)('offers the choice with the registration in the %s screen, keeping every asset on this device by default', async tone => {
        const connectTarget = vi.fn(async () => {})
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget, tone } })
        await registration()
        expect(host.querySelector('[role="radiogroup"]')?.getAttribute('aria-label')).toBe(sync.residency.title)
        expect(host.textContent).toContain(sync.residency.description)
        expect(shownChoices()).toEqual([[sync.residency.full, true], [sync.residency.remote, false]])
        choices()[1].click(); await domSettled()
        expect(shownChoices()).toEqual([[sync.residency.full, false], [sync.residency.remote, true]])
        expect(f.policy).not.toHaveBeenCalled()
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'remote')
        expect(f.policy).not.toHaveBeenCalled()
    })
    it('starts from the asset storage this device already keeps', async () => {
        f.status.mockResolvedValue(residencyStatus({ policy: 'remote' }))
        const connectTarget = vi.fn(async () => {})
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await settle(); await registration()
        expect(shownChoices()).toEqual([[sync.residency.full, false], [sync.residency.remote, true]])
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'remote')
    })
    it('downloads every asset with sync stopped once a device that kept them on the server connects keeping them here', async () => {
        f.status.mockResolvedValue(residencyStatus({ policy: 'remote' }))
        f.policy.mockResolvedValue(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        const connectTarget = vi.fn(async () => { publish({ status: { configured: true, bound: true }, paused: false, error: '' }) })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await settle(); await registration()
        choices()[0].click(); await domSettled()
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
        expect(f.policy).toHaveBeenCalledExactlyOnceWith('full')
        const order = [connectTarget, f.hold, f.policy, f.release].map(mock => mock.mock.invocationCallOrder[0])
        expect(order).toEqual([...order].sort((a, b) => a - b))
    })
    it('downloads nothing when that connection is cancelled', async () => {
        f.status.mockResolvedValue(residencyStatus({ policy: 'remote' }))
        const connectTarget = vi.fn(async () => ({ kind: 'cancelled' }))
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await settle(); await registration()
        choices()[0].click(); await domSettled()
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
        expect(f.policy).not.toHaveBeenCalled(); expect(f.hold).not.toHaveBeenCalled()
    })
    it('leaves a connected device on its own asset storage group when it reads a new registration', async () => {
        f.view = { status: { configured: true, bound: true }, paused: false, error: '' }
        f.status.mockResolvedValue(residencyStatus({ policy: 'full' }))
        const connectTarget = vi.fn(async () => {})
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await settle(); click(sync.enterCode); await domSettled(); await registration()
        expect(host.querySelectorAll('[role="radiogroup"]')).toHaveLength(1)
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, undefined)
    })
    it('mounted onboarding connects with the asset storage chosen on its screen', async () => {
        f.connect.mockResolvedValue({ kind: 'bound' })
        await openOnboardingServer(); await registration()
        choices()[1].click(); await domSettled()
        click(sync.connect); await settle()
        expect(f.connect).toHaveBeenCalledExactlyOnceWith(registered, false, 'remote')
        expect(heading()).toBe(onboarding.done.title)
    })
})
describe('downloading files kept only on the server', () => {
    it.each([
        { name: 'files only on the server', policy: 'full', status: {}, shown: true },
        { name: 'files only in external storage', policy: 'full', status: { ...externalOnly, remoteBytes: 4096 }, shown: true },
        { name: 'no remote files', policy: 'full', status: { remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }, shown: false },
        { name: 'files only on the server', policy: 'remote', status: {}, shown: false },
    ] as const)('offers the download for policy $policy with $name: $shown', async ({ policy, status, shown }) => {
        await mountBound(residencyStatus({ policy, ...status }))
        expect(!!findButton(sync.residency.download)).toBe(shown)
    })
    it('shows the files kept only in external storage as their own row', async () => {
        await mountBound(residencyStatus({ policy: 'full', ...externalOnly, remoteBytes: 3 * 1024 * 1024 }))
        const row = [...host.querySelectorAll('*')].find(node => node.children.length === 0 && node.textContent === sync.residency.externalOnly)?.parentElement?.parentElement
        expect(row?.textContent).toContain('3.0 MiB')
        expect(findButton(sync.residency.download)).toBeDefined()
    })
    it('shows no external storage row when every remote file is on the server', async () => {
        await mountBound(residencyStatus({ policy: 'full' }))
        expect(host.textContent).not.toContain(sync.residency.externalOnly)
    })
    it('downloads with sync stopped and stays connected', async () => {
        await mountBound(residencyStatus({ policy: 'full' }))
        f.policy.mockResolvedValue(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        f.status.mockResolvedValue(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        click(sync.residency.download); await settle()
        expect(f.policy).toHaveBeenCalledExactlyOnceWith('full'); expect(f.disconnect).not.toHaveBeenCalled()
        const order = [f.hold, f.policy, f.release].map(mock => mock.mock.invocationCallOrder[0])
        expect(order).toEqual([...order].sort((a, b) => a - b))
        expect(findButton(sync.residency.download)).toBeUndefined()
    })
})

describe('asset storage before its breakdown loads', () => {
    const connectedView = (assetPolicy: string, fields: Record<string, unknown> = {}) => ({ status: { configured: true, bound: true, assetPolicy }, paused: false, error: '', ...fields })
    function deferred<T>() { let resolve!: (value: T) => void; let reject!: (error: unknown) => void; const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail }); return { promise, resolve, reject } }
    const choices = () => [...host.querySelectorAll<HTMLInputElement>('[role="radiogroup"] input[type="radio"]')]
    const shownChoices = () => choices().map(option => [option.closest('label')?.textContent?.trim(), option.checked, option.disabled])
    const loading = () => host.querySelector('[data-residency-loading]')
    const rowValue = (label: string) => [...host.querySelectorAll('.place')].find(row => row.querySelector('.place-label')?.textContent === label)?.querySelector('.value')?.textContent
    async function mountLoading(assetPolicy: string) {
        f.view = connectedView(assetPolicy)
        const read = deferred<ReturnType<typeof residencyStatus>>()
        f.status.mockReturnValueOnce(read.promise)
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        return read
    }
    it('shows the choices with the stored policy and a loading state in place of the breakdown', async () => {
        const read = await mountLoading('remote')
        expect(host.textContent).toContain(sync.residency.title)
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
        expect(loading()?.getAttribute('role')).toBe('status')
        expect(loading()?.querySelector('.checking')?.textContent?.trim()).toBe(sync.residency.checking)
        expect(loading()?.querySelector('.places')?.getAttribute('aria-hidden')).toBe('true')
        expect([...loading()!.querySelectorAll('.place-label')].map(label => label.textContent)).toEqual([sync.residency.local, sync.residency.remoteOnly, sync.residency.unavailable])
        expect(host.querySelectorAll('.place .value')).toHaveLength(0)
        expect(host.querySelector('.distribution')).toBeNull()
        expect(findButton(sync.residency.download)).toBeUndefined()
        expect(findButton(sync.residency.clean)?.disabled).toBe(false)
        read.resolve(residencyStatus({ localBytes: 2048 })); await settle()
        expect(loading()).toBeNull()
        expect(rowValue(sync.residency.local)).toBe('2.0 KiB')
        expect(rowValue(sync.residency.remoteOnly)).toBe('4.0 KiB')
        expect(rowValue(sync.residency.unavailable)).toBe(sync.count.replace('{0}', '0'))
        expect(host.querySelector('.distribution')).not.toBeNull()
        expect(findButton(sync.residency.download)).toBeUndefined()
    })
    it('offers the download only once the breakdown lists files kept elsewhere', async () => {
        const read = await mountLoading('full')
        expect(shownChoices()).toEqual([[sync.residency.full, true, false], [sync.residency.remote, false, false]])
        expect(findButton(sync.residency.download)).toBeUndefined()
        read.resolve(residencyStatus({ policy: 'full' })); await settle()
        expect(findButton(sync.residency.download)?.disabled).toBe(false)
    })
    it('keeps the blocks of the group while the breakdown arrives', async () => {
        const read = await mountLoading('full')
        const blocks = () => [...host.querySelectorAll('section')].find(section => section.textContent?.includes(sync.residency.title))?.querySelectorAll('.sync-block').length
        expect(blocks()).toBe(3)
        read.resolve(residencyStatus({ policy: 'full' })); await settle()
        expect(blocks()).toBe(3)
    })
    it('changes the choice while the breakdown loads and keeps the newer breakdown over the first one', async () => {
        const first = await mountLoading('full')
        f.ensure.mockImplementation(async () => { publish(connectedView('remote')) })
        f.policy.mockResolvedValue(residencyStatus({ policy: 'remote', localBytes: 1024 }))
        choices()[1].click(); await settle()
        expect(f.policy).toHaveBeenCalledExactlyOnceWith('remote')
        expect(f.status).toHaveBeenCalledOnce()
        expect(loading()).toBeNull()
        expect(rowValue(sync.residency.local)).toBe('1.0 KiB')
        first.resolve(residencyStatus({ policy: 'full', localBytes: 8 * 1024 * 1024 })); await settle()
        expect(rowValue(sync.residency.local)).toBe('1.0 KiB')
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
        expect(findButton(sync.residency.download)).toBeUndefined()
    })
    it('disables the choices while the change runs and returns them to the stored policy when it is refused', async () => {
        await mountLoading('remote')
        const change = deferred<never>()
        f.policy.mockReturnValue(change.promise)
        choices()[0].click(); await settle()
        expect(shownChoices()).toEqual([[sync.residency.full, true, true], [sync.residency.remote, false, true]])
        expect(findButton(sync.residency.cancel)).toBeDefined()
        change.reject({ code: 'server-unreachable', retryable: true }); await settle()
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
        expect(alertText()).toBe(sync.errorHelp)
    })
    it('drops the loading state when the breakdown cannot be read', async () => {
        const read = await mountLoading('remote')
        read.reject({ code: 'local-store-unavailable' }); await settle()
        expect(loading()).toBeNull()
        expect(host.querySelectorAll('.place')).toHaveLength(0)
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
        expect(findButton(sync.residency.clean)).toBeDefined()
        expect(alertText()).toBe(sync.errorHelp)
    })
    it('shows the sync storage without waiting for the breakdown', async () => {
        const { getServerSyncCacheUsage } = await import('src/ts/storage/sync/serverSyncProduction')
        vi.mocked(getServerSyncCacheUsage).mockResolvedValue({ totalBytes: 2, cacheBytes: 1, protectedBytes: 0, reclaimableBytes: 1, ledgerBytes: 1, databaseBytes: 0, blockedReason: null })
        await mountLoading('full')
        expect(loading()).not.toBeNull()
        expect(host.textContent).toContain(sync.management.title)
    })
    it('connects keeping every asset here and downloads them when the stored policy kept them on the server', async () => {
        f.view = { status: { configured: false, assetPolicy: 'remote' }, paused: false, error: '' }
        f.status.mockReturnValue(new Promise(() => {}))
        f.policy.mockResolvedValue(residencyStatus({ policy: 'full', remoteObjects: 0, remoteBytes: 0, serverObjects: 0, serverBytes: 0 }))
        const connectTarget = vi.fn(async () => { publish(connectedView('remote')) })
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget } })
        await settle(); await registration()
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
        choices()[0].click(); await domSettled()
        click(sync.connect); await settle()
        expect(connectTarget).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
        expect(f.policy).toHaveBeenCalledExactlyOnceWith('full')
        const order = [connectTarget, f.hold, f.policy, f.release].map(mock => mock.mock.invocationCallOrder[0])
        expect(order).toEqual([...order].sort((a, b) => a - b))
    })
    it.each([
        { name: 'a policy change', returns: 'policy', policy: 'remote', act: () => choices()[1].click() },
        { name: 'downloading the files kept elsewhere', returns: 'policy', policy: 'full', act: () => click(sync.residency.download) },
        { name: 'a cleanup', returns: 'evict', policy: 'full', act: () => click(sync.residency.clean) },
    ] as const)('shows the breakdown $name returns without reading it again', async ({ returns, policy, act }) => {
        f.view = connectedView('full')
        f.status.mockResolvedValue(residencyStatus({ policy: 'full', localBytes: 8 * 1024 * 1024 }))
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(rowValue(sync.residency.local)).toBe('8.0 MiB')
        f[returns].mockResolvedValue(residencyStatus({ policy, localBytes: 3 * 1024 }))
        act(); await settle()
        expect(f[returns]).toHaveBeenCalledOnce()
        expect(f.status).toHaveBeenCalledOnce()
        expect(rowValue(sync.residency.local)).toBe('3.0 KiB')
        expect(alertText()).toBeUndefined()
    })
    it('reads the breakdown again after an operation that returns none', async () => {
        f.view = connectedView('full')
        f.status.mockResolvedValueOnce(residencyStatus({ policy: 'full', localBytes: 8 * 1024 * 1024 })).mockResolvedValue(residencyStatus({ policy: 'full', localBytes: 1024 }))
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        click(sync.syncNow); await settle()
        expect(f.status).toHaveBeenCalledTimes(2)
        expect(rowValue(sync.residency.local)).toBe('1.0 KiB')
    })
    it('keeps a choice made before connecting when the connection status is published again', async () => {
        f.view = { status: { configured: false, assetPolicy: 'full' }, paused: false, error: '' }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle(); await registration()
        choices()[1].click(); await domSettled()
        publish({ status: { configured: false, assetPolicy: 'full' }, paused: false, error: '' }); await settle()
        expect(shownChoices()).toEqual([[sync.residency.full, false, false], [sync.residency.remote, true, false]])
    })
})

const onboarding = languageEnglish.risuNest.onboarding
const heading = () => host.querySelector('h1')?.textContent
function pick(title: string) { const entry = [...host.querySelectorAll('button')].find(button => button.querySelector('b')?.textContent === title); expect(entry).toBeDefined(); entry!.click() }
async function openOnboardingServer(expectConnectionScreen = true) {
    component = mount(Onboarding, { target: host }); await domSettled()
    pick(onboarding.home.syncTitle); await domSettled()
    pick(onboarding.sync.hubTitle); await domSettled()
    if (expectConnectionScreen) expect(heading()).toBe(onboarding.hub.title)
}
const registered = { endpoint: 'https://synthetic.invalid', libraryId: 'library', deviceId: 'registration', token: 'synthetic' }
it.each(['bound', 'cancelled'])('mounted onboarding advances only after %s shared binding outcome', async kind => {
    f.connect.mockResolvedValue({ kind })
    await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
    expect(f.connect).toHaveBeenCalledExactlyOnceWith(registered, false, 'full')
    expect(f.configure).not.toHaveBeenCalled(); expect(f.bind).not.toHaveBeenCalled()
    expect(heading()).toBe(kind === 'bound' ? onboarding.done.title : onboarding.hub.title)
})
it('mounted onboarding connects as a new device through the same action', async () => {
    f.view = { status: { configured: true, bound: false }, paused: true, error: 'writer-collision' }
    f.connect.mockResolvedValue({ kind: 'bound' })
    await openOnboardingServer(); await registration(); click(languageEnglish.lwwSync.newDeviceAction); await settle()
    expect(f.connect).toHaveBeenCalledExactlyOnceWith(registered, true, 'full')
    expect(heading()).toBe(onboarding.done.title)
})
it.each([false, true])('mounted onboarding mentions the QR code only where it can be scanned (%s)', async scan => {
    f.scan = scan
    await openOnboardingServer()
    expect(host.textContent).toContain(scan ? onboarding.hub.leadScan : onboarding.hub.lead)
    expect(host.textContent?.includes('QR')).toBe(scan)
})
describe('onboarding on a device already connected to the server', () => {
    const connected = { status: { configured: true, bound: true }, paused: false, error: '' }
    it('finishes without connecting again', async () => {
        f.view = connected
        await openOnboardingServer(false); await settle()
        expect(heading()).toBe(onboarding.done.title)
        expect(host.textContent).toContain(onboarding.done.data)
        expect(f.connect).not.toHaveBeenCalled(); expect(f.bind).not.toHaveBeenCalled()
    })
    it('stays on the connection screen while the connection is unfinished, and finishes once it completes', async () => {
        f.view = { status: { configured: false, bound: true }, paused: true, error: '', bindingIncomplete: true }
        await openOnboardingServer(); await settle()
        expect(heading()).toBe(onboarding.hub.title)
        expect(host.querySelector('[role="status"]')?.textContent).toBe(languageEnglish.lwwSync.bindingIncomplete)
        publish(connected); await settle()
        expect(heading()).toBe(onboarding.done.title)
    })
    it('keeps a failed connection that left the device connected on screen until sync runs', async () => {
        f.connect.mockImplementation(async () => {
            publish({ status: { configured: true, bound: true }, paused: true, error: 'server-unreachable' })
            throw Object.assign(new Error('server-unreachable'), { code: 'server-unreachable' })
        })
        await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
        expect(heading()).toBe(onboarding.hub.title)
        expect(host.querySelector('[role="alert"]')).not.toBeNull()
        publish(connected); await settle()
        expect(heading()).toBe(onboarding.done.title)
    })
})
describe('starting fresh from onboarding', () => {
    const startingValues = ['preset', 'textTheme', 'maxContext', 'maxResponse', 'claudeCachingExperimental']
    it('applies the starting settings to an empty library that is not connected', async () => {
        component = mount(Onboarding, { target: host }); await domSettled()
        pick(onboarding.home.freshTitle); await settle()
        expect(f.preset).toHaveBeenCalledOnce()
        expect(f.state.db).toMatchObject({ preset: 'starting', textTheme: 'highcontrast', maxContext: 16000, maxResponse: 1000, claudeCachingExperimental: true })
        expect(heading()).toBe(onboarding.done.title)
        expect(host.textContent).toContain(onboarding.done.fresh)
    })
    it.each([
        { name: 'connected to a sync server', binding: { target: { kind: 'server', connectionId: 'server' } }, characters: [], summary: onboarding.done.data },
        { name: 'connected to external storage', binding: { target: { kind: 'external', connectionId: 'external' } }, characters: [], summary: onboarding.done.data },
        { name: 'holding characters', binding: { target: { kind: 'none' } }, characters: [{ chaId: 'synthetic' }], summary: onboarding.done.data },
    ])('keeps the shared settings of a library $name', async ({ binding, characters, summary }) => {
        f.bindingState.mockResolvedValue(binding)
        f.state.db.characters = characters
        component = mount(Onboarding, { target: host }); await domSettled()
        pick(onboarding.home.freshTitle); await settle()
        expect(f.preset).not.toHaveBeenCalled()
        for (const key of startingValues) expect(f.state.db).not.toHaveProperty(key)
        expect(heading()).toBe(onboarding.done.title)
        expect(host.textContent).toContain(summary)
    })
    it('keeps the shared settings when the connection state cannot be read', async () => {
        f.bindingState.mockRejectedValue(new Error('binding-state-unavailable'))
        component = mount(Onboarding, { target: host }); await domSettled()
        pick(onboarding.home.freshTitle); await settle()
        expect(f.preset).not.toHaveBeenCalled()
        for (const key of startingValues) expect(f.state.db).not.toHaveProperty(key)
        expect(heading()).toBe(onboarding.done.title)
        expect(host.textContent).toContain(onboarding.done.data)
        expect(host.textContent).not.toContain(onboarding.done.import)
    })
})
it('mounted web onboarding offers no native server entry', async () => {
    f.native = false; component = mount(Onboarding, { target: host }); await domSettled()
    expect(host.textContent).not.toContain(languageEnglish.risuNest.onboarding.sync.hubTitle)
    expect(host.querySelector('#server-registration')).toBeNull()
})
it('mounted onboarding discards the registration without advancing', async () => {
    await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.discardRegistration); await settle()
    expect(f.connect).not.toHaveBeenCalled()
    expect(host.querySelector('h1')?.textContent).toBe(languageEnglish.risuNest.onboarding.hub.title)
})

it('mounted onboarding keeps binding failures on the connection screen', async () => {
    f.connect.mockRejectedValue(new Error('refresh-failed'))
    await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
    expect(host.querySelector('h1')?.textContent).toBe(languageEnglish.risuNest.onboarding.hub.title)
    expect(host.querySelector('[role="alert"]')).not.toBeNull()
})
it('a late bound response does not advance a dismissed server screen', async () => {
    let finish!: (value: { kind: string }) => void
    f.connect.mockImplementation(() => new Promise(resolve => { finish = resolve }))
    await openOnboardingServer(); await registration(); click(languageEnglish.risuNest.serverSync.connect); await settle()
    click(languageEnglish.risuNest.onboarding.back); await domSettled()
    finish({ kind: 'bound' }); await settle()
    expect(host.querySelector('h1')?.textContent).toBe(languageEnglish.risuNest.onboarding.sync.title)
})

describe('layout', () => {
    const statusText = () => host.querySelector('[data-tone]')?.textContent?.trim()
    it('folds the registration input on a connected device until it is asked for', async () => {
        f.view = { status: { configured: true, bound: true }, paused: false, error: '' }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(host.querySelector('textarea')).toBeNull()
        click(sync.enterCode); await domSettled()
        expect(host.querySelector('textarea')).not.toBeNull()
    })
    it.each([
        { name: 'not connected', view: { status: { configured: false }, paused: false, error: '' }, shown: true },
        { name: 'left unfinished', view: { status: { configured: true, bound: false }, paused: false, error: '', bindingIncomplete: true }, shown: true },
        { name: 'connected', view: { status: { configured: true, bound: true }, paused: false, error: '' }, shown: false },
        { name: 'connected and asked for a new code', view: { status: { configured: true, bound: true }, paused: true, error: 'unauthorized' }, shown: false },
    ])('warns about concurrent edits only before the device connects, when $name', async ({ view, shown }) => {
        f.view = view
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(host.textContent?.includes(languageEnglish.lwwSync.concurrentEditNotice)).toBe(shown)
    })
    it.each([
        { name: 'not connected', view: { status: { configured: false }, paused: false, error: '' }, label: sync.disconnected, tone: 'idle' },
        { name: 'connected', view: { status: { configured: true, bound: true }, paused: false, error: '' }, label: sync.ready, tone: 'connected' },
        { name: 'running', view: { status: { configured: true, bound: true }, paused: false, running: true, error: '' }, label: sync.running, tone: 'working' },
        { name: 'paused', view: { status: { configured: true, bound: true }, paused: true, error: '' }, label: sync.paused, tone: 'paused' },
        { name: 'failing and retrying', view: { status: { configured: true, bound: true }, paused: false, error: 'server-unreachable' }, label: sync.failed, tone: 'failed', notice: 'info' },
        { name: 'stopped by an error', view: { status: { configured: true, bound: true }, paused: true, error: 'unit-too-large' }, label: sync.blocked, tone: 'attention', notice: 'danger' },
        { name: 'revoked', view: { status: { configured: true, bound: true }, paused: true, error: 'unauthorized' }, label: sync.registrationRequired, tone: 'attention' },
    ])('labels the connection state when $name', async ({ view, label, tone, notice }) => {
        f.view = view
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(statusText()).toBe(label)
        expect(host.querySelector('[data-tone]')?.getAttribute('data-tone')).toBe(tone)
        if (notice) expect(host.querySelector('[data-notice][role]')?.getAttribute('data-notice')).toBe(notice)
    })
    it('leaves the section heading and the sync storage to the settings page', async () => {
        const cache = { totalBytes: 2, cacheBytes: 1, protectedBytes: 0, reclaimableBytes: 1, ledgerBytes: 1, databaseBytes: 0, blockedReason: null }
        const { getServerSyncCacheUsage } = await import('src/ts/storage/sync/serverSyncProduction')
        vi.mocked(getServerSyncCacheUsage).mockResolvedValue(cache)
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn(), tone: 'onboarding' } })
        await settle()
        expect([...host.querySelectorAll('h2')].map(node => node.textContent)).toEqual([])
        expect(host.textContent).not.toContain(sync.management.title)
        expect(host.querySelector('textarea')).not.toBeNull()
        await unmount(component); component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect([...host.querySelectorAll('h2')].map(node => node.textContent)).toEqual([sync.title, sync.management.title])
    })
})
describe('progress', () => {
    const running = (startedAt: number) => ({
        status: { configured: true, bound: true }, paused: false, running: true, error: '',
        progress: { mode: 'full', startedAt, stages: ['downloading', 'publishing'], active: ['publishing'], current: 'publishing', plannedSend: 4, lanes: [lane('send', { active: true, step: 'uploading', itemsDone: 1, itemsTotal: 2, filesDone: 1, filesTotal: 2, bytesDone: 1024, bytesTotal: 4096, sentBytes: 3000 }), lane('receive', { receivedBytes: 500 })] },
    })
    const panel = () => host.querySelector('[data-sync-progress]')
    it.each(['settings', 'onboarding'] as const)('shows upload and download speed in %s and hides them for local work, waiting and completion', async tone => {
        vi.useFakeTimers({ toFake: ['performance'] })
        try {
            const base = running(Date.now() - 5000)
            const network = { id: 'attempt', atMs: 0, sentBytes: '0', receivedBytes: '0', sending: true, receiving: true }
            const mode = tone === 'settings' ? 'routine' : 'full'
            const progress = { ...base.progress, mode, network }
            f.view = { ...base, progress }
            component = mount(ServerSyncSettings, { target: host, props: { tone, connectTarget: vi.fn() } })
            await settle()
            const sampled = { ...progress, network: { ...network, atMs: 1000, sentBytes: '2097152', receivedBytes: '1048576' } }
            publish({ ...base, progress: sampled }); await settle()
            expect(panel()?.querySelector('[data-transfer-speed]')?.textContent).toContain(`${languageEnglish.risuNest.serverSync.uploadSpeed} 2.0 MiB/s`)
            expect(panel()?.querySelector('[data-transfer-speed]')?.textContent).toContain(`${languageEnglish.risuNest.serverSync.downloadSpeed} 1.0 MiB/s`)
            expect(panel()?.querySelector('[data-transfer-speed]')?.closest('details')).toBeNull()
            expect(panel()?.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
            for (const update of [{ pausedAt: Date.now() }, { active: ['applying'], current: 'applying' }, { active: ['refreshing'], current: 'refreshing' }]) {
                publish({ ...base, progress: { ...sampled, ...update } }); await settle()
                expect(panel()?.querySelector('[data-transfer-speed]')).toBeNull()
            }
            publish({ ...base, progress: undefined, finished: { ...sampled, active: [], lanes: [], endedAt: Date.now() } }); await settle()
            expect(host.querySelector('[data-transfer-speed]')).toBeNull()
        } finally { vi.useRealTimers() }
    })
    it('shows the running step, the stages and the transfer counts of a sync', async () => {
        f.view = running(Date.now() - 5000)
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        expect(panel()?.querySelector('[role="status"]')?.textContent).toContain(sync.activity.uploading)
        expect(panel()?.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
        expect([...panel()!.querySelectorAll('li')].map(item => [item.textContent?.trim(), item.getAttribute('aria-current')])).toEqual([[sync.stage.downloading, null], [sync.stage.publishing, 'step']])
        expect([...panel()!.querySelectorAll('dt')].map(term => term.textContent)).toEqual([sync.verifiedBytes, sync.progressItems, sync.progressFiles, sync.elapsed])
        expect(host.querySelector('[data-tone]')?.textContent?.trim()).toBe(sync.running)
    })
    it('opens the panel only once a sync has run for a moment', async () => {
        vi.useFakeTimers()
        try {
            f.view = running(Date.now())
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
            await settle()
            expect(panel()).toBeNull()
            await vi.advanceTimersByTimeAsync(500); await settle()
            expect(panel()).toBeNull()
            await vi.advanceTimersByTimeAsync(500); await settle()
            expect(panel()).not.toBeNull()
        } finally { vi.useRealTimers() }
    })
    it('shows the changes to upload and the last successful sync while idle', async () => {
        f.pending.mockResolvedValue(3)
        f.view = { status: { configured: true, bound: true }, paused: false, error: '', lastSuccessAt: 0 }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await settle()
        const rows = Object.fromEntries([...host.querySelectorAll('dt')].map(term => [term.textContent, term.nextElementSibling?.textContent]))
        expect(rows[sync.pendingChanges]).toBe(sync.count.replace('{0}', '3'))
        expect(rows[sync.lastSuccess]).toBe(new Date(0).toLocaleString())
        expect(panel()).toBeNull()
    })
    it('shows a first connection under the server check in place of its hint', async () => {
        f.view = { status: { configured: false }, paused: false, error: '' }
        component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
        await registration()
        expect(host.textContent).toContain(sync.connectHint)
        publish({ status: { configured: false }, paused: false, error: '', progress: { mode: 'full', startedAt: Date.now() - 1000, stages: ['preparing'], active: ['preparing'], current: 'preparing' } })
        await settle()
        expect(panel()?.closest('.sync-block')?.textContent).toContain(sync.reviewTitle)
        expect(host.textContent).not.toContain(sync.connectHint)
        expect(host.querySelector('[data-tone]')?.textContent?.trim()).toBe(sync.running)
    })
    describe('automatic sync', () => {
        const routine = (lanes: unknown[], fields: Record<string, unknown> = {}) => ({ mode: 'routine', startedAt: Date.now(), stages: ['downloading', 'publishing'], active: ['publishing'], current: 'publishing', lanes, ...fields })
        const connected = (fields: Record<string, unknown>) => ({ status: { configured: true, bound: true, libraryId: 'library' }, paused: false, error: '', lastSuccessAt: 0, ...fields })
        const lastSync = () => [...host.querySelectorAll('dt')].some(term => term.textContent === sync.lastSuccess)
        it('shows one bar as soon as a change moves, with its steps folded under Details', async () => {
            f.view = connected({ running: true, progress: routine([lane('send', { active: true, step: 'confirming', itemsDone: 1, itemsTotal: 4, sentBytes: 900 })], { plannedSend: 4 }) })
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
            await settle()
            expect(panel()?.getAttribute('data-mode')).toBe('routine')
            expect(panel()?.querySelector('[role="status"]')?.textContent).toContain(sync.running)
            expect(panel()?.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
            expect(lastSync()).toBe(false)
            const details = panel()!.querySelector('details')!
            expect(details.open).toBe(false)
            expect(details.querySelector('summary')?.textContent?.trim()).toBe(sync.details)
            details.querySelector('summary')!.click(); await settle()
            expect(details.open).toBe(true)
            expect(details.textContent).toContain(`${sync.activity.confirming} · 1 / 4`)
            expect([...details.querySelectorAll('li')].map(item => item.textContent?.trim())).toEqual([sync.stage.downloading, sync.stage.publishing])
            expect([...details.querySelectorAll('dt')].map(term => term.textContent)).toEqual([sync.verifiedBytes, sync.progressItems, sync.elapsed])
        })
        it('shows nothing while automatic sync has no change to move', async () => {
            vi.useFakeTimers()
            try {
                f.view = connected({ running: true, progress: routine([lane('send', { active: true, step: 'preparing' }), lane('receive')]) })
                component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
                await vi.advanceTimersByTimeAsync(2000); await settle()
                expect(panel()).toBeNull()
                expect(lastSync()).toBe(true)
            } finally { vi.useRealTimers() }
        })
        it('keeps a finished bar for a moment, then shows the last sync again', async () => {
            const finished = routine([lane('send', { itemsDone: 3, itemsTotal: 3 })], { active: [], endedAt: Date.now() })
            f.view = connected({ finished })
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
            await settle()
            expect(panel()?.querySelector('[role="status"]')?.textContent).toContain(sync.complete)
            expect(panel()?.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('100')
            expect(panel()?.querySelector('[role="status"] .lucide-loader-circle')).toBeNull()
            expect(host.querySelector('[data-tone]')?.textContent?.trim()).toBe(sync.ready)
            expect(lastSync()).toBe(false)
            publish(connected({}))
            await settle()
            expect(panel()).toBeNull()
            expect(lastSync()).toBe(true)
        })
        it('keeps Details as it was left for the next sync', async () => {
            f.view = connected({ running: true, progress: routine([lane('send', { active: true, step: 'confirming', itemsDone: 1, itemsTotal: 2 })], { plannedSend: 2 }) })
            component = mount(ServerSyncSettings, { target: host, props: { connectTarget: vi.fn() } })
            await settle()
            panel()!.querySelector('summary')!.click(); await settle()
            publish(connected({}))
            await settle()
            publish(connected({ running: true, progress: routine([lane('receive', { active: true, step: 'downloading', backlogDone: 10, backlogLeft: 30 })], { stages: ['downloading'], active: ['downloading'], current: 'downloading' }) }))
            await settle()
            expect(panel()!.querySelector('details')!.open).toBe(true)
            expect(panel()?.querySelector('[role="progressbar"]')?.getAttribute('aria-valuenow')).toBe('25')
        })
    })
})

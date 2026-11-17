vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
import 'src/ts/stores.svelte'
import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'
import { language } from 'src/lang'
import { DBState, settingsOpen, SettingsMenuIndex } from 'src/ts/stores.svelte'
import { activeRisuNestSettingsTab, risuNestSettingsTabRequest } from 'src/ts/setting/risuNestSettingsTabs'
import { externalRenewalOpen, externalRenewalRequest } from 'src/ts/setting/externalRenewalRequest'
import { pluginStartupState } from 'src/ts/plugins/pluginStartupGate'
import { externalConnectionTitle, externalStorageStrings } from 'src/lib/Setting/ExternalStorage/strings'
import { syncStatus, type SyncStatusSource } from 'src/ts/storage/sync/syncStatus'
import type { ServerSyncView } from 'src/ts/storage/sync/syncStatusView'
import type { ExternalConnectionSummary } from 'src/ts/storage/sync/external/types'
import SyncStoppedNotice from './SyncStoppedNotice.svelte'

let component: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (component) await unmount(component)
    component = undefined
    document.body.replaceChildren()
    syncStatus.set(null)
    pluginStartupState.set({ phase: 'released', since: 0 })
    settingsOpen.set(false)
    activeRisuNestSettingsTab.set(null)
    risuNestSettingsTabRequest.set(null)
    externalRenewalRequest.set(null)
    externalRenewalOpen.set(false)
    DBState.db.risunestSyncStatusUi = true
})
const server = (view: Partial<ServerSyncView>): SyncStatusSource => ({
    target: 'server',
    view: { status: { configured: true, bound: true }, running: false, paused: false, error: '', ...view } as ServerSyncView,
    pendingChanges: async () => 0,
    syncNow: async () => {},
})
const stopped = (error: string, lastSuccessAt?: number) => server({ paused: true, error, lastSuccessAt })
async function show(source: SyncStatusSource | null) {
    syncStatus.set(source)
    await tick()
}
const notice = () => document.querySelector('[data-sync-stopped]')
const close = () => notice()!.querySelector<HTMLButtonElement>(`button[aria-label="${language.close}"]`)!.click()
const en = externalStorageStrings('en')
// Another storage connection an asset download read through.
const drive = { id: 'drive-2', providerId: 'google_drive', endpoint: { accountHint: 'synthetic@example.invalid' } } as Partial<ExternalConnectionSummary> as ExternalConnectionSummary
const external = (failure: unknown, remedyConnection?: ExternalConnectionSummary, state: Partial<ExternalConnectionSummary> = { status: 'ready' }): SyncStatusSource => ({
    target: 'external', connection: { id: 'webdav-1', purpose: 'sync', ...state } as Partial<ExternalConnectionSummary> as ExternalConnectionSummary,
    failure, remedyConnection, syncStopped: false, refresh: () => {}, syncNow: async () => {},
})

it('shows a stop once until its cause changes or a sync succeeds', async () => {
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(server({ error: 'server-unreachable' }))
    expect(notice()).toBeNull()

    await show(stopped('clock-skew'))
    expect(notice()?.getAttribute('role')).toBe('alert')
    expect(notice()?.querySelector('p')?.textContent).toBe(language.risuNest.serverSync.blocked)
    expect(notice()?.textContent).toContain(language.lwwSync.clockBlocked)
    close(); await tick()
    expect(notice()).toBeNull()

    // A retry clears the error before its attempt and fails with the same one.
    await show(server({}))
    await show(server({ running: true }))
    await show(stopped('clock-skew'))
    expect(notice()).toBeNull()

    await show(stopped('writer-collision'))
    expect(notice()).not.toBeNull()
    close(); await tick()

    await show(server({ lastSuccessAt: 1 }))
    await show(stopped('writer-collision', 1))
    expect(notice()).not.toBeNull()
})

it('leaves the stop to the plugin notice while plugins wait and omits the open settings tab', async () => {
    pluginStartupState.set({ phase: 'waiting', since: Date.now(), target: 'server' })
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(server({ error: 'unauthorized' }))
    expect(notice()).toBeNull()
    pluginStartupState.set({ phase: 'released', since: 0 }); await tick()
    expect(notice()?.querySelector('p')?.textContent).toBe(language.risuNest.serverSync.registrationRequired)

    const settings = [...notice()!.querySelectorAll('button')].find(button => button.textContent?.trim() === language.risuNest.tabs.sync)!
    settings.click(); await tick()
    expect(get(settingsOpen)).toBe(true)
    expect(get(SettingsMenuIndex)).toBe(17)
    activeRisuNestSettingsTab.set('sync'); await tick()
    expect([...notice()!.querySelectorAll('button')].map(button => button.getAttribute('aria-label'))).toEqual([language.close])
})

it.each([
    ['is marked', { status: 'reauth-required', lastError: { code: 'unauthorized', message: '', retryable: false, action: 'reauthenticate' } }, undefined, undefined, en.reauthenticate],
    ['failed a sync', { status: 'ready' }, { kind: 'reauthRequired' }, undefined, en.reauthenticate],
    // An asset download reads every storage connection that holds a body and names the one that refused.
    ['failed an asset download through another storage', { status: 'ready' }, { code: 'external-reauth-required', status: 502, retryable: false, connectionId: 'drive-2' }, drive,
        en.reauthenticateOtherAssets.replace('{0}', externalConnectionTitle(en, drive))],
] as const)('offers to sign in again when an external connection %s for it', async (_, state, failure, remedyConnection, text) => {
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(external(failure, remedyConnection, state))
    expect(notice()?.querySelector('p')?.textContent).toBe(en.statusReauth)
    expect(notice()?.querySelectorAll('p')[1]?.textContent).toBe(text)
    const buttons = [...notice()!.querySelectorAll('button')]
    expect(buttons.map(button => button.getAttribute('aria-label') ?? button.textContent?.trim())).toEqual([language.close, en.renew])

    buttons[1].click(); await tick()
    expect(get(externalRenewalRequest)).toBe(remedyConnection?.id ?? 'webdav-1')
    expect([get(settingsOpen), get(SettingsMenuIndex), get(risuNestSettingsTabRequest)]).toEqual([true, 17, 'sync'])

    // The form takes the place the notice covers; closing it with sync still stopped brings the notice back.
    externalRenewalOpen.set(true); await tick()
    expect(notice()).toBeNull()
    externalRenewalOpen.set(false); await tick()
    expect(notice()).not.toBeNull()
})

it.each([
    ['its own recovery key', { kind: 'repositoryKeyUnavailable' }, undefined, en.unlockKey],
    // An asset download reads every storage connection that holds a body and names the one without its recovery key.
    ['the recovery key of another storage', { code: 'external-key-locked', status: 502, retryable: false, connectionId: 'drive-2' }, drive,
        en.unlockKeyOtherAssets.replace('{0}', externalConnectionTitle(en, drive))],
] as const)('reports a sync stopped on %s and opens the sync settings for it', async (_, failure, remedyConnection, text) => {
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(external(failure, remedyConnection))
    expect(notice()?.querySelector('p')?.textContent).toBe(en.statusLocked)
    expect(notice()?.querySelectorAll('p')[1]?.textContent).toBe(text)
    const buttons = [...notice()!.querySelectorAll('button')]
    expect(buttons.map(button => button.getAttribute('aria-label') ?? button.textContent?.trim())).toEqual([language.close, language.risuNest.tabs.sync])
})

it('shows a closed external stop again once it names another storage or asks for the recovery key', async () => {
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(external({ kind: 'reauthRequired' }))
    close(); await tick()
    // A retry refused the same way keeps it closed.
    await show(external({ kind: 'unauthorized' }))
    expect(notice()).toBeNull()

    await show(external({ code: 'external-reauth-required', status: 502, retryable: false, connectionId: 'drive-2' }, drive))
    expect(notice()).not.toBeNull()
    close(); await tick()
    await show(external({ code: 'external-key-locked', status: 502, retryable: false, connectionId: 'drive-2' }, drive))
    expect(notice()).not.toBeNull()
})

it('stays hidden while the sync UI is turned off', async () => {
    DBState.db.risunestSyncStatusUi = false
    component = mount(SyncStoppedNotice, { target: document.body })
    await show(stopped('unauthorized'))
    expect(notice()).toBeNull()
    DBState.db.risunestSyncStatusUi = true; await tick()
    expect(notice()).not.toBeNull()
})

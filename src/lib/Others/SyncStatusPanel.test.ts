vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
vi.mock('src/ts/storage/persistentDataRuntime.svelte', async importOriginal => ({
    ...await importOriginal<typeof import('src/ts/storage/persistentDataRuntime.svelte')>(),
    persistentLocalSaveFailure: (await import('svelte/store')).writable<unknown | null>(null),
}))
vi.mock('src/ts/globalApi.svelte', () => ({ saving: { state: false } }))
import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'
import { language } from 'src/lang'
import { externalStorageStrings } from 'src/lib/Setting/ExternalStorage/strings'
import { syncStatusPanel } from 'src/ts/gui/syncStatusPanel'
import { DBState, settingsOpen, SettingsMenuIndex } from 'src/ts/stores.svelte'
import { externalRenewalRequest } from 'src/ts/setting/externalRenewalRequest'
import { syncStatus, type SyncStatusSource } from 'src/ts/storage/sync/syncStatus'
import type { ServerSyncView } from 'src/ts/storage/sync/syncStatusView'
import type { ExternalConnectionSummary } from 'src/ts/storage/sync/external/types'
import SyncStatusPanel from './SyncStatusPanel.svelte'

let component: ReturnType<typeof mount> | undefined
afterEach(async () => {
    syncStatusPanel.set(null)
    if (component) await unmount(component)
    component = undefined
    document.body.replaceChildren()
    syncStatus.set(null)
    settingsOpen.set(false)
    externalRenewalRequest.set(null)
    DBState.db.risunestSyncStatusUi = true
})
async function open(source: SyncStatusSource) {
    const anchor = document.body.appendChild(document.createElement('button'))
    syncStatus.set(source)
    component = mount(SyncStatusPanel, { target: document.body })
    await tick()
    syncStatusPanel.set({ anchor, placement: 'side' })
    await tick()
}
const lastSuccess = new Date(0).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' })
const rows = () => [...document.querySelectorAll('[data-sync-status-panel] dt')].map(term => [term.textContent, term.nextElementSibling?.textContent])

it('lists server rows it knows and reads pending changes when an attempt ends, not on each report', async () => {
    const pendingChanges = vi.fn(async () => 3)
    const server = (view: Partial<ServerSyncView>): SyncStatusSource => ({
        target: 'server',
        view: { status: { configured: true, bound: true }, running: false, paused: false, error: '', ...view } as ServerSyncView,
        pendingChanges,
        syncNow: async () => { throw { code: 'clock-skew' } },
    })
    const copy = language.risuNest.serverSync
    await open(server({}))
    await vi.waitFor(() => expect(rows()).toEqual([
        [copy.pendingChanges, copy.count.replace('{0}', '3')],
        [externalStorageStrings('en').thisDevice, language.risuNest.syncStatus.saved],
    ]))

    syncStatus.set(server({ running: true })); await tick()
    for (let report = 0; report < 3; report++) { syncStatus.set(server({ running: true })); await tick() }
    pendingChanges.mockResolvedValue(0)
    syncStatus.set(server({ lastSuccessAt: 0 })); await tick()
    await vi.waitFor(() => expect(rows().slice(0, 2)).toEqual([
        [copy.lastSuccess, lastSuccess],
        [copy.pendingChanges, copy.count.replace('{0}', '0')],
    ]))
    expect(pendingChanges).toHaveBeenCalledTimes(3)

    const syncNow = [...document.querySelectorAll('[data-sync-status-panel] button')].find(button => button.textContent?.trim() === copy.syncNow) as HTMLButtonElement
    syncNow.click()
    await vi.waitFor(() => expect(document.querySelector('[data-sync-status-panel]')?.textContent).toContain(language.lwwSync.clockBlocked))

    document.body.dispatchEvent(new PointerEvent('pointerdown', { bubbles: true })); await tick()
    expect(get(syncStatusPanel)).toBeNull()
    expect(document.querySelector('[data-sync-status-panel]')).toBeNull()

    syncStatusPanel.set({ anchor: document.body.querySelector('button')!, placement: 'side' }); await tick()
    expect(document.querySelector('[data-sync-status-panel]')).not.toBeNull()
    DBState.db.risunestSyncStatusUi = false; await tick()
    expect(get(syncStatusPanel)).toBeNull()
    expect(document.querySelector('[data-sync-status-panel]')).toBeNull()
})

it('reads external storage again when it opens and shows its last sync without pending changes', async () => {
    const refresh = vi.fn()
    const connection = { id: 'webdav-1', purpose: 'sync', status: 'ready', lastSyncAtMs: '0' } as Partial<ExternalConnectionSummary> as ExternalConnectionSummary
    const source: SyncStatusSource = { target: 'external', connection, syncStopped: false, refresh, syncNow: async () => {} }
    await open(source)
    syncStatus.set({ ...source }); await tick()
    expect(refresh).toHaveBeenCalledTimes(1)
    expect(document.querySelector('[data-sync-status-panel] h2')?.textContent).toBe(externalStorageStrings('en').title)
    expect(rows()).toEqual([
        [language.risuNest.serverSync.lastSuccess, lastSuccess],
        [externalStorageStrings('en').thisDevice, language.risuNest.syncStatus.saved],
    ])
})

it('offers to sign in again in place of Sync Now while an external sync stops on its sign-in', async () => {
    const strings = externalStorageStrings('en')
    const connection = { id: 'drive-1', purpose: 'sync', status: 'ready' } as Partial<ExternalConnectionSummary> as ExternalConnectionSummary
    await open({ target: 'external', connection, failure: { kind: 'reauthRequired' }, syncStopped: false, refresh: () => {}, syncNow: async () => {} })
    const buttons = () => [...document.querySelectorAll('[data-sync-status-panel] .border-t button')].map(button => button.textContent?.trim())
    expect(buttons()).toEqual([strings.renew, language.risuNest.tabs.sync])

    ;[...document.querySelectorAll<HTMLButtonElement>('[data-sync-status-panel] button')].find(button => button.textContent?.trim() === strings.renew)!.click()
    await tick()
    expect(get(syncStatusPanel)).toBeNull()
    expect(get(externalRenewalRequest)).toBe('drive-1')
    expect([get(settingsOpen), get(SettingsMenuIndex)]).toEqual([true, 17])

    syncStatus.set({ target: 'external', connection, syncStopped: false, refresh: () => {}, syncNow: async () => {} })
    syncStatusPanel.set({ anchor: document.body.querySelector('button')!, placement: 'side' }); await tick()
    expect(buttons()[0]).toBe(language.risuNest.serverSync.syncNow)
})

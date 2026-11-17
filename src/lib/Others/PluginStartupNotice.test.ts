vi.mock('src/ts/parser/parser.svelte', async () => (await import('src/ts/parser/parser.testSupport')).parserStubModule())
import 'src/ts/stores.svelte'
import { afterEach, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
import { get } from 'svelte/store'
import { changeLanguage, language } from 'src/lang'
import { languageKorean } from 'src/lang/ko'
import { DBState, settingsOpen, SettingsMenuIndex } from 'src/ts/stores.svelte'
import { activeRisuNestSettingsTab, risuNestSettingsTabRequest } from 'src/ts/setting/risuNestSettingsTabs'
import { externalRenewalOpen, externalRenewalRequest } from 'src/ts/setting/externalRenewalRequest'
import { configurePluginStartup, pluginStartupState } from 'src/ts/plugins/pluginStartupGate'
import PluginStartupNotice from './PluginStartupNotice.svelte'
import { syncStatus, type SyncStatusSource } from 'src/ts/storage/sync/syncStatus'
import { syncStatusDisplay, type ServerSyncView } from 'src/ts/storage/sync/syncStatusView'
import { externalStorageStrings } from '../Setting/ExternalStorage/strings'
import type { ExternalConnectionSummary } from 'src/ts/storage/sync/external/types'

let component: ReturnType<typeof mount> | undefined
let inlineComponent: ReturnType<typeof mount> | undefined
afterEach(async () => {
    if (component) await unmount(component)
    if (inlineComponent) await unmount(inlineComponent)
    component = inlineComponent = undefined
    document.body.replaceChildren()
    pluginStartupState.set({ phase: 'released', since: 0 })
    syncStatus.set(null)
    configurePluginStartup(() => false, async () => {})
    settingsOpen.set(false)
    activeRisuNestSettingsTab.set(null)
    risuNestSettingsTabRequest.set(null)
    externalRenewalRequest.set(null)
    externalRenewalOpen.set(false)
    DBState.db.risunestSyncStatusUi = true
    vi.useRealTimers()
    changeLanguage('en')
})
async function render() {
    component = mount(PluginStartupNotice, { target: document.body })
    await tick()
}
const labels = () => [...document.querySelectorAll('aside button')].map(button => button.getAttribute('aria-label') ?? button.textContent?.trim())
const close = () => document.querySelector<HTMLButtonElement>(`aside button[aria-label="${language.close}"]`)!.click()

it('waits one second, closes until the next wait, stop or failure, and hides after release', async () => {
    vi.useFakeTimers()
    const since = Date.now()
    pluginStartupState.set({ phase: 'waiting', since })
    await render()
    expect(document.querySelector('aside')).toBeNull()
    await vi.advanceTimersByTimeAsync(999)
    expect(document.querySelector('aside')).toBeNull()
    await vi.advanceTimersByTimeAsync(1); await tick()
    expect(document.querySelector('[role="status"]')?.textContent).toBe(language.risuNest.pluginStartup.waiting)
    close(); await tick()
    expect(document.querySelector('aside')).toBeNull()

    // A stop while plugins still wait shows again; closing it holds even after it clears.
    syncStatus.set({ target: 'server', syncNow: async () => {}, pendingChanges: async () => 0,
        view: { status: { configured: true, bound: true }, running: false, paused: true, error: 'clock-skew' } as ServerSyncView }); await tick()
    expect(document.querySelector('[role="alert"]')).not.toBeNull()
    close(); await tick()
    syncStatus.set(null); await tick()
    expect(document.querySelector('aside')).toBeNull()

    // A failure keeps the wait's start time and still shows.
    pluginStartupState.set({ phase: 'failed', since, error: { code: 'unauthorized' }, target: 'server' }); await tick()
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(language.risuNest.pluginStartup.syncUnfinished)
    close(); await tick()
    expect(document.querySelector('aside')).toBeNull()

    pluginStartupState.set({ phase: 'waiting', since: Date.now() })
    await vi.advanceTimersByTimeAsync(1000); await tick()
    expect(document.querySelector('[role="status"]')).not.toBeNull()
    pluginStartupState.set({ phase: 'released', since: 0 }); await tick()
    expect(document.querySelector('aside')).toBeNull()
})

it.each(['unauthorized', 'invalid-device-token', 'rate-limited'])('reports the stopped sync reason while the sync UI is turned off (%s)', async code => {
    vi.useFakeTimers()
    DBState.db.risunestSyncStatusUi = false
    pluginStartupState.set({ phase: 'waiting', since: Date.now() })
    await render()
    await vi.advanceTimersByTimeAsync(1000); await tick()
    expect(document.querySelector('aside')).toBeNull()
    const source: SyncStatusSource = {
        target: 'server', syncNow: async () => {}, pendingChanges: async () => 0,
        view: { status: { configured: true, bound: true }, running: false, paused: true, error: code } as ServerSyncView,
    }
    syncStatus.set(source); await tick()
    const display = syncStatusDisplay(source, externalStorageStrings(DBState.db.language), false)
    // The stop shows while plugins still wait.
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.label)
    pluginStartupState.set({ phase: 'failed', since: Date.now(), error: new Error('Sync binding is unavailable'), target: 'server' }); await tick()
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(language.risuNest.pluginStartup.syncUnfinished)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.label)
    if (code !== 'rate-limited') expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.error)
    expect(labels()).toContain(language.retry)
})

it('uses the language loaded after the app mounted', async () => {
    vi.useFakeTimers()
    pluginStartupState.set({ phase: 'waiting', since: Date.now() })
    await render()
    changeLanguage('ko')
    await vi.advanceTimersByTimeAsync(1000); await tick()
    expect(document.querySelector('[role="status"]')?.textContent).toBe(languageKorean.risuNest.pluginStartup.waiting)
    expect(labels()).toEqual([languageKorean.close, languageKorean.risuNest.pluginStartup.loadNow])
})

it('shows failure actions immediately and opens the sync tab', async () => {
    pluginStartupState.set({ phase: 'failed', since: Date.now(), error: { code: 'unauthorized' }, target: 'server' })
    await render()
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(language.risuNest.pluginStartup.syncUnfinished)
    expect(labels()).toEqual([language.risuNest.tabs.sync, language.close, language.retry, language.risuNest.pluginStartup.loadNow])
    const settings = [...document.querySelectorAll('button')].find(button => button.textContent === language.risuNest.tabs.sync)!
    settings.click(); await tick()
    expect(get(settingsOpen)).toBe(true)
    expect(get(SettingsMenuIndex)).toBe(17)
    activeRisuNestSettingsTab.set('sync'); await tick()
    expect([...document.querySelectorAll('button')].some(button => button.textContent === language.risuNest.tabs.sync)).toBe(false)
})

it('leaves the plugin settings page to its inline notice', async () => {
    pluginStartupState.set({ phase: 'failed', since: Date.now(), error: { code: 'unauthorized' }, target: 'server' })
    await render()
    inlineComponent = mount(PluginStartupNotice, { target: document.body, props: { inline: true } })
    await tick()
    expect(document.querySelectorAll('aside')).toHaveLength(1)
    await unmount(inlineComponent); inlineComponent = undefined; await tick()
    expect(document.querySelectorAll('aside')).toHaveLength(1)
})

it.each(['waiting', 'failed'] as const)('carries the stopped sync label and shared error while %s', async phase => {
    const source: SyncStatusSource = {
        target: 'server', syncNow: async () => {}, pendingChanges: async () => 0,
        view: { status: { configured: true, bound: true }, running: false, paused: true, error: 'clock-skew' } as ServerSyncView,
    }
    syncStatus.set(source)
    pluginStartupState.set({ phase, since: Date.now() - 1000, target: 'server' })
    await render()
    await vi.waitFor(() => expect(document.querySelector('[role="alert"]')).not.toBeNull())
    const display = syncStatusDisplay(source, externalStorageStrings(DBState.db.language), false)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.label)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.error)
    expect(labels()).toContain(language.retry)
})

it('uses shared external unavailable text without printing the internal error', async () => {
    pluginStartupState.set({ phase: 'failed', since: Date.now(), target: 'external', error: new Error('Sync is unavailable') })
    await render()
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(externalStorageStrings(DBState.db.language).errorGeneric)
    expect(document.querySelector('[role="alert"]')?.textContent).not.toContain('Sync is unavailable')
})

it('carries the external stop label and action text while its own notice is hidden', async () => {
    const source: SyncStatusSource = {
        target: 'external', syncNow: async () => {}, refresh: () => {}, syncStopped: false,
        connection: {id:'synthetic',status:'reauth-required',lastError:{code:'unauthorized',message:'',retryable:false,action:'reauthenticate'}} as ExternalConnectionSummary,
    }
    syncStatus.set(source)
    pluginStartupState.set({phase:'waiting',since:Date.now()-1000,target:'external'})
    await render()
    await vi.waitFor(() => expect(document.querySelector('[role="alert"]')).not.toBeNull())
    const display=syncStatusDisplay(source,externalStorageStrings(DBState.db.language),false)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.label)
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(display.error)
    expect(labels()).toEqual([language.close, externalStorageStrings(DBState.db.language).renew, language.risuNest.pluginStartup.loadNow])
})

it('signs in again instead of retrying an external startup that failed on its sign-in, and steps aside while the form is open', async () => {
    // Only the gate's error names the sign-in failure here.
    syncStatus.set({ target: 'external', syncNow: async () => {}, refresh: () => {}, syncStopped: false,
        connection: { id: 'synthetic', status: 'ready' } as ExternalConnectionSummary })
    pluginStartupState.set({ phase: 'failed', since: Date.now(), target: 'external', error: { kind: 'reauthRequired' } })
    await render()
    const renew = externalStorageStrings(DBState.db.language).renew
    expect(labels()).toEqual([language.close, renew, language.risuNest.pluginStartup.loadNow])
    ;[...document.querySelectorAll<HTMLButtonElement>('aside button')].find(button => button.textContent?.trim() === renew)!.click()
    expect(get(externalRenewalRequest)).toBe('synthetic')
    expect([get(settingsOpen), get(SettingsMenuIndex), get(risuNestSettingsTabRequest)]).toEqual([true, 17, 'sync'])

    // The form takes the top of the settings page; closing it with plugins still held brings the notice back.
    externalRenewalOpen.set(true); await tick()
    expect(document.querySelector('aside')).toBeNull()
    externalRenewalOpen.set(false); await tick()
    expect(document.querySelector('aside')).not.toBeNull()
})

it('suppresses the generic server error help under the unfinished notice', async () => {
    pluginStartupState.set({ phase: 'failed', since: Date.now(), target: 'server', error: new Error('Sync is unavailable') })
    await render()
    expect(document.querySelector('[role="alert"]')?.textContent).toContain(language.risuNest.pluginStartup.syncUnfinished)
    expect(document.querySelector('[role="alert"]')?.textContent).not.toContain(language.risuNest.serverSync.errorHelp)
})

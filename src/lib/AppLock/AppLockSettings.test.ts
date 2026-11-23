import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { mount, unmount, tick } from 'svelte'
import { language } from 'src/lang'
import { appLock } from 'src/ts/appLock/appLock.svelte'
import { defaultAppLockRecord, createPinRecord } from 'src/ts/appLock/record'
import type { AppLockCapabilities, OsAuthOutcome } from 'src/ts/appLock/native'
import RisuNestAppLockSettings from '../Setting/Pages/RisuNestAppLockSettings.svelte'

const boundary = vi.hoisted(() => ({
    capabilities: { auth: { status: 'unsupported' }, previewSupported: false } as AppLockCapabilities,
    outcome: 'cancelled' as OsAuthOutcome,
}))
vi.mock('@tauri-apps/api/core', async (importOriginal) => ({
    ...await importOriginal<typeof import('@tauri-apps/api/core')>(),
    invoke: async (command: string) => {
        if (command === 'app_lock_check') return boundary.capabilities
        if (command === 'app_lock_authenticate') return boundary.outcome
        if (command === 'app_lock_set_policy') return { active: true, epoch: 0, atMs: Date.now(), departedAtMs: null }
        if (command === 'app_lock_ready') return true
        throw new Error(`Unexpected native command: ${command}`)
    },
}))
vi.mock('@tauri-apps/api/event', async (importOriginal) => ({
    ...await importOriginal<typeof import('@tauri-apps/api/event')>(),
    listen: async () => () => {},
}))
let host: HTMLDivElement
let mounted: ReturnType<typeof mount> | undefined
let saved: string | null = null
let failWrite = false
const syntheticPin = await createPinRecord('123456')

beforeEach(async () => {
    boundary.capabilities = { auth: { status: 'unsupported' }, previewSupported: false }
    boundary.outcome = 'cancelled'
    saved = null
    failWrite = false
    await appLock.initialize({ read: () => saved, commit: async value => {
        if (failWrite) throw new Error('Synthetic write failure')
        saved = value
    } })
    await appLock.retryRead()
    host = document.createElement('div')
    document.body.append(host)
})
afterEach(async () => {
    if (mounted) await unmount(mounted)
    mounted = undefined
    appLock.cancelFlow()
    host.remove()
})
async function render(enabled = false) {
    if (enabled) {
        saved = JSON.stringify({ ...defaultAppLockRecord(), enabled: true, pin: syntheticPin })
        await appLock.retryRead()
        for (const digit of "123456") await appLock.digit(digit)
    }
    mounted = mount(RisuNestAppLockSettings, { target: host })
    await tick()
    await vi.waitFor(() => expect(host.querySelector('input')).not.toBeNull())
}
function toggle(label: string): HTMLInputElement {
    const found = [...host.querySelectorAll('label')].find(item => item.textContent?.trim() === label)?.querySelector('input')
    expect(found).toBeDefined()
    return found!
}

describe('app lock settings', () => {
    it('warns before setup, omits unsupported rows, and keeps enable unchecked until PIN saving', async () => {
        await render()
        expect(host.textContent).toContain(language.risuNest.appLock.noRecovery)
        expect(host.textContent).not.toContain(language.risuNest.appLock.native)
        expect(host.textContent).not.toContain(language.risuNest.appLock.preview)
        toggle(language.risuNest.appLock.enable).click()
        await vi.waitFor(() => expect(appLock.state.flow).toBe('setup'))
        await tick()
        await vi.waitFor(() => expect(toggle(language.risuNest.appLock.enable).checked).toBe(false))
        expect(saved).toBeNull()
        appLock.cancelFlow()
        await tick()
        await vi.waitFor(() => expect(toggle(language.risuNest.appLock.enable).checked).toBe(false))
    })

    it('keeps app lock on and places a failed enable-row write before the recovery warning', async () => {
        await render(true)
        failWrite = true
        toggle(language.risuNest.appLock.enable).click()
        await vi.waitFor(() => expect(host.querySelector('[role="alert"]')?.textContent).toBe(language.risuNest.localData.applyFailed))
        expect(toggle(language.risuNest.appLock.enable).checked).toBe(true)
        expect([...host.querySelectorAll('[data-notice]')].map(notice => notice.textContent)).toEqual([
            language.risuNest.localData.applyFailed,
            language.risuNest.appLock.noRecovery,
        ])
        expect(JSON.parse(saved!).enabled).toBe(true)
    })

    it('keeps preview independent and restores its checkbox when a write fails', async () => {
        boundary.capabilities.previewSupported = true
        await appLock.refreshCapabilities()
        await render()
        failWrite = true
        toggle(language.risuNest.appLock.preview).click()
        await vi.waitFor(() => expect(host.textContent).toContain(language.risuNest.localData.applyFailed))
        expect(toggle(language.risuNest.appLock.preview).checked).toBe(false)
        failWrite = false
        toggle(language.risuNest.appLock.preview).click()
        await vi.waitFor(() => expect(appLock.state.record.hideRecentsPreview).toBe(true))
        expect(JSON.parse(saved!).hideRecentsPreview).toBe(true)
        expect(JSON.parse(saved!).enabled).toBe(false)
        expect(host.textContent).not.toContain(language.risuNest.localData.applyFailed)
    })

    it('offers exactly six timing choices and reverts to the persisted choice on failure', async () => {
        await render(true)
        const select = host.querySelector('select')!
        expect([...select.options].map(option => option.value)).toEqual(['launch', 'immediate', 'after-1m', 'after-5m', 'after-15m', 'after-30m'])
        failWrite = true
        select.value = 'after-15m'
        select.dispatchEvent(new Event('change', { bubbles: true }))
        await vi.waitFor(() => expect(host.textContent).toContain(language.risuNest.localData.applyFailed))
        await vi.waitFor(() => expect(select.value).toBe('launch'))
        expect(host.textContent).toContain(language.risuNest.appLock.timingLaunchHelp)
    })

    it('keeps native unlock off on cancellation and saves it only after OS success', async () => {
        boundary.capabilities.auth = { status: 'available', method: 'faceId' }
        await render(true)
        expect(host.textContent).toContain(language.risuNest.appLock.nativeHelpFaceId)
        expect(host.textContent).not.toContain(language.risuNest.appLock.noRecovery)
        toggle(language.risuNest.appLock.native).click()
        await tick()
        await vi.waitFor(() => {
            expect(toggle(language.risuNest.appLock.native).disabled).toBe(false)
            expect(toggle(language.risuNest.appLock.native).checked).toBe(false)
        })
        expect(host.textContent).not.toContain(language.risuNest.appLock.nativeUnavailable)
        boundary.outcome = 'success'
        toggle(language.risuNest.appLock.native).click()
        await tick()
        await vi.waitFor(() => expect(JSON.parse(saved!).nativeUnlock).toBe(true))
        expect(toggle(language.risuNest.appLock.native).checked).toBe(true)
    })
})
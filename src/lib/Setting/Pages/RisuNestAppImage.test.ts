import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'
const mocks = vi.hoisted(() => ({ invoke: vi.fn(), confirm: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }))
vi.mock('src/ts/alert', () => ({ alertConfirm: mocks.confirm }))
vi.mock('src/lang', () => ({ language: { risuNest: { platform: {
    title: 'Platform', appImageLinks: 'AppImage links', appImageLinksHelp: 'Register again after moving the file.',
    appImageRegister: 'Register', appImageRegistered: 'Registered', appImageReplace: 'Replace existing handler?',
} } } }))
import Component from './RisuNestAppImage.svelte'
let component: ReturnType<typeof mount> | undefined
afterEach(async () => { if (component) await unmount(component); component = undefined; document.body.replaceChildren(); vi.resetAllMocks() })
async function setup(available = true, replacesExisting = false) {
    const current = { available, replacesExisting, registered: false, token: 'observed-handler' }
    mocks.invoke.mockImplementation(async command => command === 'appimage_integration_state' ? current : undefined)
    const target = document.createElement('div')
    document.body.append(target)
    component = mount(Component, { target })
    await tick()
    await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalled())
    await tick()
    return target
}
describe('AppImage URL integration action', () => {
    it('does not render an unsupported control', async () => {
        const target = await setup(false)
        expect(target.querySelector('button')).toBeNull()
    })
    it('only replaces a handler after an explicit confirmed action', async () => {
        const target = await setup(true, true)
        expect(mocks.invoke.mock.calls.map(([command]) => command)).not.toContain('appimage_integration_register')
        mocks.confirm.mockResolvedValueOnce(false).mockResolvedValueOnce(true)
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(mocks.confirm).toHaveBeenCalledOnce())
        await tick()
        expect(mocks.invoke.mock.calls.map(([command]) => command)).not.toContain('appimage_integration_register')
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('appimage_integration_register', {
            token: 'observed-handler', replaceExisting: true,
        }))
    })
    it('shows registration failure without losing the action', async () => {
        const target = await setup()
        mocks.invoke.mockImplementation(async command => {
            if (command === 'appimage_integration_register') throw new Error('synthetic desktop failure')
            return { available: true, token: 'state', replacesExisting: false }
        })
        target.querySelector('button')!.click()
        await vi.waitFor(() => expect(target.querySelector('[role=status]')?.textContent).toContain('synthetic desktop failure'))
        expect(target.querySelector('button')!.disabled).toBe(false)
    })
})

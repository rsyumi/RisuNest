// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, unmount } from 'svelte'

const native = vi.hoisted(() => {
    const state = { status: 0, afterRequest: 0 }
    return {
        state,
        invoke: vi.fn(async (command: string) => {
            if (command === 'plugin:ios-native|state') {
                return { notifications: state.status === 2, notificationStatus: state.status }
            }
            if (command === 'plugin:ios-native|request_notifications') {
                state.status = state.afterRequest
                return { granted: state.status === 2 }
            }
            if (command === 'plugin:ios-native|open_settings') return { opened: true }
            throw new Error(`Unexpected command ${command}`)
        }),
    }
})

vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('src/ts/alert', () => ({ alertError: vi.fn() }) satisfies Partial<typeof import('src/ts/alert')>)
vi.mock('src/ts/platform', async () =>
    (await import('src/ts/platform.testSupport')).platformModule({ runtime: 'tauri', os: 'ios' }))

import { language } from 'src/lang'
import RisuNestIOSPlatform from './RisuNestIOSPlatform.svelte'

const commands = () => native.invoke.mock.calls.map(([command]) => command)
const notificationButton = (target: HTMLElement) =>
    [...target.querySelectorAll('button')].find(button =>
        [language.risuNest.platform.allowNotifications, language.risuNest.platform.openSettings]
            .includes(button.textContent?.trim() ?? ''))!

describe('RisuNest iOS platform settings', () => {
    let mounted: ReturnType<typeof mount> | null = null

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = null
        native.invoke.mockClear()
        document.body.replaceChildren()
    })

    async function mountPlatform(status: number, afterRequest = status): Promise<HTMLDivElement> {
        native.state.status = status
        native.state.afterRequest = afterRequest
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestIOSPlatform, { target })
        await vi.waitFor(() => expect(target.querySelector('[role="status"]')).not.toBeNull())
        return target
    }

    it('asks iOS for permission before any request, then offers Settings once iOS has an answer', async () => {
        const target = await mountPlatform(0, 1)
        expect(target.querySelector('[role="status"]')?.textContent).toBe(language.risuNest.platform.notificationsOff)
        expect(notificationButton(target).textContent?.trim()).toBe(language.risuNest.platform.allowNotifications)

        notificationButton(target).click()
        await vi.waitFor(() =>
            expect(notificationButton(target).textContent?.trim()).toBe(language.risuNest.platform.openSettings))
        expect(commands()).toContain('plugin:ios-native|request_notifications')
        expect(commands()).not.toContain('plugin:ios-native|open_settings')
    })

    it('shows a granted request as allowed', async () => {
        const target = await mountPlatform(0, 2)
        notificationButton(target).click()
        await vi.waitFor(() =>
            expect(target.querySelector('[role="status"]')?.textContent).toBe(language.risuNest.platform.notificationsOn))
        expect(notificationButton(target).textContent?.trim()).toBe(language.risuNest.platform.openSettings)
    })

    it('opens Settings without asking again after the user has declined', async () => {
        const target = await mountPlatform(1)
        expect(notificationButton(target).textContent?.trim()).toBe(language.risuNest.platform.openSettings)

        notificationButton(target).click()
        await vi.waitFor(() => expect(commands()).toContain('plugin:ios-native|open_settings'))
        expect(commands()).not.toContain('plugin:ios-native|request_notifications')
    })
})

// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const mocks = vi.hoisted(() => {
    let listener: ((settings: { androidKeepAliveDuringGeneration: boolean }) => void) | undefined
    const settings = { androidKeepAliveDuringGeneration: false }
    return {
        notificationStatus: null as boolean | null,
        getDetailedOSLabel: vi.fn(async () => 'Android 16'),
        openNotificationSettings: vi.fn(),
        requestAndroidGenerationNotifications: vi.fn(async () => {}),
        getDeviceSettings: vi.fn(() => ({ ...settings })),
        updateDeviceSettings: vi.fn((partial: Partial<typeof settings>) => {
            Object.assign(settings, partial)
            listener?.({ ...settings })
        }),
        subscribeDeviceSettings: vi.fn((nextListener) => {
            listener = nextListener
            return () => { listener = undefined }
        }),
        androidGenerationNotificationsEnabled: vi.fn(async () => mocks.notificationStatus),
    }
})

vi.mock('src/ts/platform', () => ({ getDetailedOSLabel: mocks.getDetailedOSLabel }))
vi.mock('src/ts/storage/deviceSettings', () => ({
    getDeviceSettings: mocks.getDeviceSettings,
    updateDeviceSettings: mocks.updateDeviceSettings,
    subscribeDeviceSettings: mocks.subscribeDeviceSettings,
}))
vi.mock('src/ts/androidGenerationKeepAlive', () => ({
    androidGenerationNotificationsEnabled: mocks.androidGenerationNotificationsEnabled,
    requestAndroidGenerationNotifications: mocks.requestAndroidGenerationNotifications,
}))

import RisuNestAndroidPlatform from './RisuNestAndroidPlatform.svelte'

describe('RisuNest Android platform settings', () => {
    let mounted: ReturnType<typeof mount> | null = null

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = null
        mocks.notificationStatus = null
        mocks.updateDeviceSettings({ androidKeepAliveDuringGeneration: false })
        vi.clearAllMocks()
        document.body.replaceChildren()
        delete window.RisuGenerationKeepAlive
    })

    async function mountPlatform(status: boolean | null): Promise<HTMLDivElement> {
        mocks.notificationStatus = status
        window.RisuGenerationKeepAlive = {
            begin: () => false,
            end: () => undefined,
            notificationsEnabled: () => status === true,
            requestNotifications: vi.fn(),
            openNotificationSettings: mocks.openNotificationSettings,
            webViewVersion: async () => '140.0.1',
        }
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestAndroidPlatform, { target })
        await vi.waitFor(() => expect(target.textContent).toContain('140.0.1'))
        return target
    }

    it('keeps platform controls and diagnostics visible when notification status is unavailable', async () => {
        const target = await mountPlatform(null)
        await tick()
        await Promise.resolve()
        await tick()

        expect(target.textContent).toContain('Platform')
        expect(target.textContent).toContain('Open notification settings')
        expect(target.textContent).toContain('Keep app alive while generating')
        expect(target.textContent).toContain('Android 16')
        expect(target.textContent).toContain('140.0.1')
        expect(target.querySelector('[role="status"]')).toBeNull()
        expect(target.querySelector('[role="alert"]')).toBeNull()
    })

    it('renders notification state, diagnostics, settings action, and persisted keep-alive toggle', async () => {
        const target = await mountPlatform(true)
        await tick()
        await Promise.resolve()
        await tick()

        expect(target.textContent).toContain('Allowed')
        expect(target.textContent).toContain('Android 16')
        expect(target.textContent).toContain('140.0.1')
        const notificationBadge = target.querySelector('[role="status"][aria-live="polite"]')
        expect(notificationBadge?.textContent).toBe('Allowed')
        expect(notificationBadge?.classList.contains('rounded-full')).toBe(true)
        expect(notificationBadge?.classList.contains('border-success-500')).toBe(true)
        expect(notificationBadge?.classList.contains('text-textcolor')).toBe(true)
        expect(target.querySelector('[role="status"] button')).toBeNull()
        const toggle = target.querySelector('input[type="checkbox"]')!
        const action = target.querySelector('button')!
        expect(action.textContent?.trim()).toBe('Open notification settings')
        // The notification status row leads, then the keep-alive toggle, then the device facts.
        expect(Boolean(action.compareDocumentPosition(toggle) & Node.DOCUMENT_POSITION_FOLLOWING)).toBe(true)
        expect(Boolean(toggle.compareDocumentPosition(target.querySelector('[data-platform-info]')!) & Node.DOCUMENT_POSITION_FOLLOWING)).toBe(true)
        target.querySelector('button')?.click()
        expect(mocks.openNotificationSettings).toHaveBeenCalledOnce()
        expect(mocks.updateDeviceSettings).not.toHaveBeenCalled()
        ;(target.querySelector('input[type="checkbox"]') as HTMLInputElement).click()
        await tick()
        expect(mocks.updateDeviceSettings).toHaveBeenCalledWith({ androidKeepAliveDuringGeneration: true })
    })

    it('warns while notifications are off and refreshes when focus returns from settings', async () => {
        const target = await mountPlatform(false)
        await tick()
        expect(target.textContent).toContain("This feature doesn't work while notifications are off.")
        expect(target.querySelector('[role="alert"]')).not.toBeNull()
        const notificationBadge = target.querySelector('[role="status"]')
        expect(notificationBadge?.textContent).toBe('Off')
        expect(notificationBadge?.classList.contains('rounded-full')).toBe(true)
        expect(notificationBadge?.classList.contains('border-draculared')).toBe(true)
        expect(notificationBadge?.classList.contains('text-textcolor')).toBe(true)

        mocks.notificationStatus = true
        window.dispatchEvent(new Event('focus'))
        await vi.waitFor(() => expect(target.textContent).toContain('Allowed'))
        expect(notificationBadge?.classList.contains('border-success-500')).toBe(true)
        expect(notificationBadge?.classList.contains('border-draculared')).toBe(false)
        expect(target.textContent).not.toContain("This feature doesn't work while notifications are off.")
    })

    it('requests permission when enabling without treating the request as a grant', async () => {
        const target = await mountPlatform(false)
        expect(mocks.requestAndroidGenerationNotifications).not.toHaveBeenCalled()
        const toggle = target.querySelector('input[type="checkbox"]') as HTMLInputElement
        toggle.click()
        await vi.waitFor(() => expect(mocks.requestAndroidGenerationNotifications).toHaveBeenCalledOnce())
        expect(target.querySelector('[role="status"]')?.textContent).toBe('Off')
        expect(target.querySelector('[role="alert"]')).not.toBeNull()
        toggle.click()
        await tick()
        expect(mocks.requestAndroidGenerationNotifications).toHaveBeenCalledOnce()
    })

    it('refreshes permission on native resume without browser focus or visibility events', async () => {
        const target = await mountPlatform(false)
        await tick()
        expect(target.querySelector('[role="alert"]')).not.toBeNull()
        target.querySelector('button')?.click()

        mocks.notificationStatus = true
        window.dispatchEvent(new Event('risunest-android-notifications-changed'))
        await vi.waitFor(() => expect(target.querySelector('[role="status"]')?.textContent).toBe('Allowed'))
        expect(target.querySelector('[role="alert"]')).toBeNull()

        mocks.notificationStatus = false
        window.dispatchEvent(new Event('risunest-android-notifications-changed'))
        await vi.waitFor(() => expect(target.querySelector('[role="status"]')?.textContent).toBe('Off'))
        expect(target.querySelector('[role="alert"]')).not.toBeNull()

        await unmount(mounted!)
        mounted = null
        mocks.androidGenerationNotificationsEnabled.mockClear()
        window.dispatchEvent(new Event('risunest-android-notifications-changed'))
        expect(mocks.androidGenerationNotificationsEnabled).not.toHaveBeenCalled()
    })
})

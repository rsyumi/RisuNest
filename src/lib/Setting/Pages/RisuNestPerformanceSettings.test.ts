// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const deviceSettings = vi.hoisted(() => ({
    getDeviceSettings: vi.fn(() => ({ performanceProfile: 'normal' as const })),
    subscribeDeviceSettings: vi.fn(() => () => undefined),
    updateDeviceSettings: vi.fn(),
}))

vi.mock('src/ts/storage/deviceSettings', () => deviceSettings)
vi.mock('src/lang', () => ({
    language: {
        risuNest: {
            perf: {
                title: 'Performance',
                profile: 'Performance profile',
                profileNormal: 'Standard',
                profileLowSpec: 'Low-spec',
                profileHelp: 'Help',
            },
        },
    },
}))

import RisuNestPerformanceSettings from './RisuNestPerformanceSettings.svelte'

describe('RisuNestPerformanceSettings', () => {
    let mounted: ReturnType<typeof mount> | undefined

    afterEach(async () => {
        if (mounted) await unmount(mounted)
        mounted = undefined
        document.body.replaceChildren()
        vi.clearAllMocks()
    })

    it('exposes the selected profile as a pressed button and updates it on activation', async () => {
        const target = document.createElement('div')
        document.body.append(target)
        mounted = mount(RisuNestPerformanceSettings, { target })
        await tick()

        const group = target.querySelector('[role="group"][aria-label="Performance profile"]')
        const standard = [...target.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Standard')
        const lowSpec = [...target.querySelectorAll<HTMLButtonElement>('button')].find((button) => button.textContent === 'Low-spec')
        expect(group).not.toBeNull()
        expect(standard?.getAttribute('aria-pressed')).toBe('true')
        expect(lowSpec?.getAttribute('aria-pressed')).toBe('false')
        expect(deviceSettings.updateDeviceSettings).not.toHaveBeenCalled()

        lowSpec?.click()
        await tick()

        expect(standard?.getAttribute('aria-pressed')).toBe('false')
        expect(lowSpec?.getAttribute('aria-pressed')).toBe('true')
        expect(deviceSettings.updateDeviceSettings).toHaveBeenCalledOnce()
        expect(deviceSettings.updateDeviceSettings).toHaveBeenCalledWith({ performanceProfile: 'low-spec' })
    })
})

// @vitest-environment happy-dom

import { afterEach, describe, expect, it, vi } from 'vitest'
import { mount, tick, unmount } from 'svelte'

const values = vi.hoisted(() => new Map<string, unknown>())

vi.mock('src/ts/setting/utils', () => ({
    UNINITIALIZED: Symbol('uninitialized'),
    getLabel: (item: { fallbackLabel?: string }) => item.fallbackLabel ?? '',
    getSettingValue: (item: { id: string }) => values.get(item.id),
    resolveLanguagePath: () => undefined,
    setSettingValue: (item: { id: string }, value: unknown) => values.set(item.id, value),
}))
vi.mock('src/ts/alert', () => ({ alertMd: vi.fn() }))

import SettingNumber from '../Wrappers/SettingNumber.svelte'
import SettingSelect from '../Wrappers/SettingSelect.svelte'
import SettingSlider from '../Wrappers/SettingSlider.svelte'
import type { SettingContext, SettingItem } from 'src/ts/setting/types'

describe('RisuNest inlay controls', () => {
    const mounted: ReturnType<typeof mount>[] = []
    const ctx = {} as SettingContext

    afterEach(async () => {
        for (const component of mounted.splice(0)) await unmount(component)
        values.clear()
        document.body.replaceChildren()
    })

    it('gives the format, quality, and maximum resolution controls their translated labels', async () => {
        const items: SettingItem[] = [
            {
                id: 'risunest.inlay.format', type: 'select', fallbackLabel: 'Storage format',
                options: { selectOptions: [{ value: 'webp', label: 'WebP' }] },
            },
            {
                id: 'risunest.inlay.quality', type: 'slider', fallbackLabel: 'WebP quality',
                options: { min: 1, max: 100, step: 1 },
            },
            {
                id: 'risunest.inlay.maxDimension', type: 'number', fallbackLabel: 'Maximum resolution (px)',
                options: { min: 0, step: 1 },
            },
        ]
        values.set(items[0].id, 'webp')
        values.set(items[1].id, 85)
        values.set(items[2].id, 0)
        const target = document.createElement('div')
        document.body.append(target)

        mounted.push(mount(SettingSelect, { target, props: { item: items[0], ctx } }))
        mounted.push(mount(SettingSlider, { target, props: { item: items[1], ctx } }))
        mounted.push(mount(SettingNumber, { target, props: { item: items[2], ctx } }))
        await tick()

        expect(target.querySelector('select')?.getAttribute('aria-label')).toBe('Storage format')
        expect(target.querySelector('[role="slider"]')?.getAttribute('aria-label')).toBe('WebP quality')
        expect(target.querySelector('input[type="number"]')?.getAttribute('aria-label')).toBe('Maximum resolution (px)')
    })
})

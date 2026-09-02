import { describe, expect, it } from 'vitest'
import { risuNestSettingsItems } from './risuNestSettingsData'
import type { Database } from '../storage/database.svelte'

describe('RisuNest inlay settings data', () => {
    it('bounds maximum dimension to the native u32 range', () => {
        const item = risuNestSettingsItems.find(({ id }) => id === 'risunest.inlay.maxDimension')

        expect(item?.options).toMatchObject({ min: 0, max: 4_294_967_295, step: 1 })
    })

    it.each([
        [-1, 0],
        [12.6, 13],
        [4_294_967_296, 4_294_967_295],
    ])('normalizes manually entered maximum dimension %s before writing live settings', (input, expected) => {
        const item = risuNestSettingsItems.find(({ id }) => id === 'risunest.inlay.maxDimension')
        const database = { risunestInlayMaxDimension: 0 } as Database

        item?.setValue?.(database, input)

        expect(database.risunestInlayMaxDimension).toBe(expected)
    })
})

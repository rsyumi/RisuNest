import { describe, expect, it } from 'vitest'
import { risuNestSettingsItems } from './risuNestSettingsData'

describe('RisuNest inlay settings data', () => {
    it('bounds maximum dimension to the native u32 range', () => {
        const item = risuNestSettingsItems.find(({ id }) => id === 'risunest.inlay.maxDimension')

        expect(item?.options).toMatchObject({ min: 0, max: 4_294_967_295, step: 1 })
    })
})

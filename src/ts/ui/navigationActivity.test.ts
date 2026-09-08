import { describe, expect, it } from 'vitest'
import { get } from 'svelte/store'

import {
    beginNavigationActivity,
    navigationActivity,
} from './navigationActivity'

describe('navigation activity', () => {
    it('keeps a newer navigation visible when an older navigation finishes', () => {
        const older = beginNavigationActivity('character')
        const newer = beginNavigationActivity('conversation')

        expect(older.isCurrent()).toBe(false)
        expect(newer.isCurrent()).toBe(true)
        expect(get(navigationActivity)).toEqual({
            token: 2,
            kind: 'conversation',
        })

        older.finish()
        expect(get(navigationActivity)).toEqual({
            token: 2,
            kind: 'conversation',
        })

        newer.finish()
        expect(get(navigationActivity)).toBeNull()
    })
})

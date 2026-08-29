import { describe, expect, test } from 'vitest'

import { resolveHypaOrphanState } from './hypaOrphanState'

describe('Hypa orphan state', () => {
    test('keeps orphan state fail closed when evidence lookup rejects', async () => {
        await expect(resolveHypaOrphanState(
            () => Promise.reject(new Error('revision changed')),
        )).resolves.toBe(true)
    })

    test('publishes the valid evidence result after a later successful lookup', async () => {
        await expect(resolveHypaOrphanState(() => Promise.resolve(false))).resolves.toBe(false)
    })
})

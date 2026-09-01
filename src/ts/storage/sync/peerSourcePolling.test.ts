import { afterEach, describe, expect, it, vi } from 'vitest'

import { createPeerSourcePolling } from './peerSourcePolling'

afterEach(() => vi.useRealTimers())

describe('peer source polling', () => {
    it('uses one interval, never overlaps a tick, and stops future ticks', async () => {
        vi.useFakeTimers()
        let finish!: () => void
        const poll = vi.fn(() => new Promise<void>((resolve) => { finish = resolve }))
        const polling = createPeerSourcePolling({ intervalMilliseconds: 10, poll })

        polling.start()
        polling.start()
        await vi.advanceTimersByTimeAsync(10)
        await vi.advanceTimersByTimeAsync(20)
        expect(poll).toHaveBeenCalledTimes(1)

        polling.stop()
        finish()
        await Promise.resolve()
        await vi.advanceTimersByTimeAsync(30)
        expect(poll).toHaveBeenCalledTimes(1)

        polling.start()
        await vi.advanceTimersByTimeAsync(10)
        expect(poll).toHaveBeenCalledTimes(2)
        polling.stop()
    })
})

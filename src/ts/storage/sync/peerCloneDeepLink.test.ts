import { describe, expect, it, vi } from 'vitest'

import {
    consumePendingPeerCloneUri,
    publishPeerCloneUri,
    subscribePeerCloneUri,
} from './peerCloneDeepLink'

describe('peer clone deep link bridge', () => {
    it('keeps only the latest unconsumed URI and delivers later links to subscribers', () => {
        publishPeerCloneUri('first')
        publishPeerCloneUri('second')
        expect(consumePendingPeerCloneUri()).toBe('second')
        expect(consumePendingPeerCloneUri()).toBeNull()

        const listener = vi.fn()
        const unsubscribe = subscribePeerCloneUri(listener)
        publishPeerCloneUri('third')
        unsubscribe()
        publishPeerCloneUri('fourth')

        expect(listener).toHaveBeenCalledOnce()
        expect(listener).toHaveBeenCalledWith('third')
        expect(consumePendingPeerCloneUri()).toBe('fourth')
    })
})

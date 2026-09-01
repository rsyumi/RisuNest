import { describe, expect, it, vi } from 'vitest'
import { dispatchRisuLocalUrl } from '../../deepLinkDispatcher'

import {
    consumePendingPeerCloneUri,
    consumePendingDeviceSyncUri,
    publishPeerCloneUri,
    publishDeviceSyncUri,
    receiveDeviceSyncUri,
    subscribeDeviceSyncUri,
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

describe('device sync registration bridge', () => {
    it('carries a canonical Tauri v2 URL from the dispatcher into pending controller state', () => {
        consumePendingDeviceSyncUri()
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2'
        const openSettings = vi.fn()

        expect(dispatchRisuLocalUrl(uri, {
            onRealm: vi.fn(),
            onPeerClone: vi.fn(),
            onDeviceSync: (value) => receiveDeviceSyncUri(value, openSettings),
        })).toBe(true)
        expect(consumePendingDeviceSyncUri()).toBe(uri)
        expect(openSettings).toHaveBeenCalledWith(18)
    })

    it('stages a v2 link independently without notifying legacy clone listeners', () => {
        const legacy = vi.fn()
        const unregisterLegacy = subscribePeerCloneUri(legacy)
        const listener = vi.fn()
        const unregister = subscribeDeviceSyncUri(listener)

        publishDeviceSyncUri('risuailocal://peer-clone/v2?endpoint=x')

        expect(listener).toHaveBeenCalledWith('risuailocal://peer-clone/v2?endpoint=x')
        expect(legacy).not.toHaveBeenCalled()
        unregister()
        unregisterLegacy()
        publishDeviceSyncUri('later')
        expect(consumePendingDeviceSyncUri()).toBe('later')
    })
})

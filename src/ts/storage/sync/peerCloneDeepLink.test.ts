import { describe, expect, it, vi } from 'vitest'
import { dispatchRisuLocalUrl } from '../../deepLinkDispatcher'

import {
    consumePendingDeviceSyncUri,
    publishDeviceSyncUri,
    receiveDeviceSyncUri,
    subscribeDeviceSyncUri,
} from './peerCloneDeepLink'

describe('device sync registration bridge', () => {
    it('carries a canonical Tauri v2 URL from the dispatcher into pending controller state', () => {
        consumePendingDeviceSyncUri()
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2'
        const openSettings = vi.fn()

        expect(dispatchRisuLocalUrl(uri, {
            onRealm: vi.fn(),
            onDeviceSync: (value) => receiveDeviceSyncUri(value, openSettings),
        })).toBe(true)
        expect(consumePendingDeviceSyncUri()).toBe(uri)
        expect(openSettings).toHaveBeenCalledWith(18)
    })

    it('keeps only the latest unconsumed URI and delivers later links to subscribers', () => {
        publishDeviceSyncUri('first')
        publishDeviceSyncUri('second')
        expect(consumePendingDeviceSyncUri()).toBe('second')
        expect(consumePendingDeviceSyncUri()).toBeNull()

        const listener = vi.fn()
        const unregister = subscribeDeviceSyncUri(listener)
        publishDeviceSyncUri('third')
        unregister()
        publishDeviceSyncUri('later')

        expect(listener).toHaveBeenCalledOnce()
        expect(listener).toHaveBeenCalledWith('third')
        expect(consumePendingDeviceSyncUri()).toBe('later')
    })
})

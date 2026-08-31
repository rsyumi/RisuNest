import { afterEach, describe, expect, it } from 'vitest'
import { androidPeerSyncNotificationsEnabled } from './peerSyncShared'

describe('androidPeerSyncNotificationsEnabled', () => {
    afterEach(() => {
        delete (window as { RisuPeerCloneBridge?: unknown }).RisuPeerCloneBridge
    })

    it('reports the bridge state when the method is available', () => {
        expect(androidPeerSyncNotificationsEnabled({
            startSource: () => true,
            stopSource: () => true,
            notificationsEnabled: () => false,
        })).toBe(false)
        expect(androidPeerSyncNotificationsEnabled({
            startSource: () => true,
            stopSource: () => true,
            notificationsEnabled: () => true,
        })).toBe(true)
    })

    it('returns null without a bridge or on an old bridge without the method', () => {
        expect(androidPeerSyncNotificationsEnabled()).toBeNull()
        expect(androidPeerSyncNotificationsEnabled({
            startSource: () => true,
            stopSource: () => true,
        })).toBeNull()
    })

    it('treats a throwing bridge as unknown instead of failing the caller', () => {
        expect(androidPeerSyncNotificationsEnabled({
            startSource: () => true,
            stopSource: () => true,
            notificationsEnabled: () => {
                throw new Error('bridge detached')
            },
        })).toBeNull()
    })

    it('falls back to the window bridge when none is passed', () => {
        ;(window as { RisuPeerCloneBridge?: unknown }).RisuPeerCloneBridge = {
            startSource: () => true,
            stopSource: () => true,
            notificationsEnabled: () => false,
        }
        expect(androidPeerSyncNotificationsEnabled()).toBe(false)
    })
})

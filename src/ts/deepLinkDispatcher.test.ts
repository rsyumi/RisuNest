import { describe, expect, it, vi } from 'vitest'

import { dispatchRisuLocalUrl } from './deepLinkDispatcher'

describe('dispatchRisuLocalUrl', () => {
    it('routes peer clone links without changing the realm route', () => {
        const onRealm = vi.fn()
        const onPeerClone = vi.fn()
        const onDeviceSync = vi.fn()
        const handlers = { onRealm, onPeerClone, onDeviceSync }

        expect(dispatchRisuLocalUrl('risuailocal://realm/card-1', handlers)).toBe(true)
        expect(dispatchRisuLocalUrl(
            'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.2%3A1234&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=secret',
            handlers,
        )).toBe(true)

        expect(onRealm).toHaveBeenCalledWith('card-1')
        expect(onPeerClone).toHaveBeenCalledTimes(1)
        expect(onDeviceSync).not.toHaveBeenCalled()
    })

    it('routes v2 registration links separately from legacy clone links', () => {
        const onDeviceSync = vi.fn()
        const handlers = { onRealm: vi.fn(), onPeerClone: vi.fn(), onDeviceSync }
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=secret'

        expect(dispatchRisuLocalUrl(uri, handlers)).toBe(true)
        expect(onDeviceSync).toHaveBeenCalledWith(uri)
        expect(handlers.onPeerClone).not.toHaveBeenCalled()
    })

    it('never falls back to the legacy clone handler for v2 links', () => {
        const handlers = { onRealm: vi.fn(), onPeerClone: vi.fn() }
        const uri = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145'

        expect(dispatchRisuLocalUrl(uri, handlers)).toBe(false)
        expect(handlers.onPeerClone).not.toHaveBeenCalled()
    })

    it('ignores unknown and malformed links', () => {
        const handlers = { onRealm: vi.fn(), onPeerClone: vi.fn(), onDeviceSync: vi.fn() }

        expect(dispatchRisuLocalUrl('https://example.com/realm/card-1', handlers)).toBe(false)
        expect(dispatchRisuLocalUrl('not a url', handlers)).toBe(false)
        expect(dispatchRisuLocalUrl('risuailocal://realm/%E0%A4%A', handlers)).toBe(false)
        expect(handlers.onRealm).not.toHaveBeenCalled()
        expect(handlers.onPeerClone).not.toHaveBeenCalled()
    })
})

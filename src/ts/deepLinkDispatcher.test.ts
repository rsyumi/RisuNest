import { describe, expect, it, vi } from 'vitest'

import { dispatchRisuLocalUrl } from './deepLinkDispatcher'

const registration = 'risuailocal://peer-clone/v2?endpoint=http%3A%2F%2F192.168.1.2%3A32145&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=secret'

describe('dispatchRisuLocalUrl', () => {
    it('routes realm links without touching the device sync handler', () => {
        const onRealm = vi.fn()
        const onDeviceSync = vi.fn()

        expect(dispatchRisuLocalUrl('risuailocal://realm/card-1', { onRealm, onDeviceSync })).toBe(true)
        expect(onRealm).toHaveBeenCalledWith('card-1')
        expect(onDeviceSync).not.toHaveBeenCalled()
    })

    it('routes v2 registration links to the device sync handler', () => {
        const onDeviceSync = vi.fn()

        expect(dispatchRisuLocalUrl(registration, { onRealm: vi.fn(), onDeviceSync })).toBe(true)
        expect(onDeviceSync).toHaveBeenCalledWith(registration)
    })

    it('does not recognize v1 clone links', () => {
        const handlers = { onRealm: vi.fn(), onDeviceSync: vi.fn() }
        const legacy = 'risuailocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.2%3A1234&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=secret'

        expect(dispatchRisuLocalUrl(legacy, handlers)).toBe(false)
        expect(handlers.onRealm).not.toHaveBeenCalled()
        expect(handlers.onDeviceSync).not.toHaveBeenCalled()
    })

    it('ignores unknown and malformed links', () => {
        const handlers = { onRealm: vi.fn(), onDeviceSync: vi.fn() }

        expect(dispatchRisuLocalUrl('https://example.com/realm/card-1', handlers)).toBe(false)
        expect(dispatchRisuLocalUrl('not a url', handlers)).toBe(false)
        expect(dispatchRisuLocalUrl('risuailocal://realm/%E0%A4%A', handlers)).toBe(false)
        expect(handlers.onRealm).not.toHaveBeenCalled()
        expect(handlers.onDeviceSync).not.toHaveBeenCalled()
    })
})

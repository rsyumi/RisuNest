import { describe, expect, it, vi } from 'vitest'

import { dispatchRisuLocalUrl } from './deepLinkDispatcher'

const registration =
    'risunestlocal://sync-server/connect?libraryId=synthetic-library'

describe('dispatchRisuLocalUrl', () => {
    it('routes realm links without touching the server sync handler', () => {
        const onRealm = vi.fn()
        const onServerSync = vi.fn()

        expect(
            dispatchRisuLocalUrl('risunestlocal://realm/card-1', {
                onRealm,
                onServerSync,
            }),
        ).toBe(true)
        expect(onRealm).toHaveBeenCalledWith('card-1')
        expect(onServerSync).not.toHaveBeenCalled()
    })

    it('routes public server navigation to the server sync handler', () => {
        const onServerSync = vi.fn()

        expect(
            dispatchRisuLocalUrl(registration, {
                onRealm: vi.fn(),
                onServerSync,
            }),
        ).toBe(true)
        expect(onServerSync).toHaveBeenCalledWith(registration)
    })

    it('does not recognize v1 clone links', () => {
        const handlers = { onRealm: vi.fn(), onServerSync: vi.fn() }
        const legacy =
            'risunestlocal://peer-clone/v1?endpoint=http%3A%2F%2F192.168.1.2%3A1234&session=123e4567-e89b-12d3-a456-426614174000&manifest=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa#claim=secret'

        expect(dispatchRisuLocalUrl(legacy, handlers)).toBe(false)
        expect(
            dispatchRisuLocalUrl(legacy.replace('/v1?', '/v2?'), handlers),
        ).toBe(false)
        expect(handlers.onRealm).not.toHaveBeenCalled()
        expect(handlers.onServerSync).not.toHaveBeenCalled()
    })

    it('ignores unknown and malformed links', () => {
        const handlers = { onRealm: vi.fn(), onServerSync: vi.fn() }

        expect(
            dispatchRisuLocalUrl('https://example.com/realm/card-1', handlers),
        ).toBe(false)
        expect(dispatchRisuLocalUrl('not a url', handlers)).toBe(false)
        expect(
            dispatchRisuLocalUrl('risunestlocal://realm/%E0%A4%A', handlers),
        ).toBe(false)
        expect(handlers.onRealm).not.toHaveBeenCalled()
        expect(handlers.onServerSync).not.toHaveBeenCalled()
    })
})

it('routes strict private registration separately from public navigation', async () => {
    const vector = (
        await import('../../crates/sync-connect/tests/registration-vector.json')
    ).default
    const handlers = {
        onRealm: vi.fn(),
        onServerSync: vi.fn(),
        onServerRegistration: vi.fn(),
    }
    expect(dispatchRisuLocalUrl(vector.uri, handlers)).toBe(true)
    expect(handlers.onServerRegistration).toHaveBeenCalledExactlyOnceWith(
        vector.uri,
    )
    expect(handlers.onServerSync).not.toHaveBeenCalled()
    expect(handlers.onRealm).not.toHaveBeenCalled()
    expect(dispatchRisuLocalUrl(vector.uri + '=', handlers)).toBe(false)
    expect(handlers.onServerRegistration).toHaveBeenCalledOnce()
})

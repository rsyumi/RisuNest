import { afterEach, describe, expect, it, vi } from 'vitest'
import { fetchRealmResource, isRealmAccessDisabled } from './realmAccess'

afterEach(() => {
    vi.unstubAllGlobals()
})

describe('RisuRealm test access', () => {
    it('disables Realm only when the explicit test flag is true', () => {
        expect(isRealmAccessDisabled()).toBe(true)
        expect(isRealmAccessDisabled({ VITE_DISABLE_REALM: 'true' })).toBe(true)
    })

    it('keeps Realm compatible when the test flag is absent or false', () => {
        expect(isRealmAccessDisabled({})).toBe(false)
        expect(isRealmAccessDisabled({ VITE_DISABLE_REALM: 'false' })).toBe(false)
    })

    it('does not issue Realm resource requests when access is disabled', () => {
        const fetchMock = vi.fn()
        vi.stubGlobal('fetch', fetchMock)

        expect(fetchRealmResource('https://example.invalid/rs/asset.png', undefined, {
            VITE_DISABLE_REALM: 'true',
        })).toBeUndefined()
        expect(fetchMock).not.toHaveBeenCalled()
    })

    it('preserves Realm resource requests when access is enabled', async () => {
        const response = new Response(null, { status: 200 })
        const fetchMock = vi.fn().mockResolvedValue(response)
        vi.stubGlobal('fetch', fetchMock)

        await expect(fetchRealmResource('https://example.invalid/rs/asset.png', undefined, {})).resolves.toBe(response)
        expect(fetchMock).toHaveBeenCalledOnce()
    })
})

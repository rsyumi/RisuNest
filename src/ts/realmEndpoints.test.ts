import { afterEach, describe, expect, it, vi } from 'vitest'
import { isRealmUrl, REALM_BLOCKED_URL_PATTERNS } from '../../scripts/realmBlocklist.mjs'
import { REALM_HUB_URL, REALM_NIGHTLY_HUB_URL, REALM_SITE_URL } from './realmEndpoints'

afterEach(() => {
    vi.unstubAllGlobals()
    vi.restoreAllMocks()
})

describe('isRealmUrl', () => {
    it.each([
        `${REALM_HUB_URL}/realm/search`,
        `${REALM_HUB_URL}/hub/info/some-id`,
        `${REALM_HUB_URL}/hub/realm/upload`,
        `${REALM_HUB_URL}/hub/report`,
        `${REALM_HUB_URL}/hub/remove`,
        `${REALM_HUB_URL}/resource/img.png`,
        `${REALM_HUB_URL}/rs/assets/hash.png`,
        `${REALM_NIGHTLY_HUB_URL}/realm/search`,
        `${REALM_SITE_URL}/upload`,
        `${REALM_SITE_URL}/api/v1/download/dynamic/id?cors=true`,
        `${REALM_SITE_URL}/character/id`,
    ])('treats %s as Realm', (url) => {
        expect(isRealmUrl(url)).toBe(true)
    })

    it.each([
        `${REALM_HUB_URL}/hub/login`,
        `${REALM_HUB_URL}/hub/account/save`,
        `${REALM_HUB_URL}/cryptokey?key=1`,
        `${REALM_HUB_URL}/drive/token?code=x`,
        `${REALM_HUB_URL}/transformers/model.onnx`,
        `${REALM_HUB_URL}/proxy2`,
        `${REALM_HUB_URL}/kei`,
        `${REALM_HUB_URL}/redirect/docs/lua`,
        'https://risuai.xyz/',
        'https://example.com/realm/search',
    ])('keeps %s reachable', (url) => {
        expect(isRealmUrl(url)).toBe(false)
    })

    it('does not treat relative or non-http inputs as Realm', () => {
        expect(isRealmUrl('/realm/search')).toBe(false)
        expect(isRealmUrl('blob:https://example.com/uuid')).toBe(false)
        expect(isRealmUrl('data:text/plain,realm')).toBe(false)
        expect(isRealmUrl('')).toBe(false)
    })

    it('accepts URL objects', () => {
        expect(isRealmUrl(new URL(`${REALM_SITE_URL}/upload`))).toBe(true)
    })
})

describe('REALM_BLOCKED_URL_PATTERNS', () => {
    it('stays consistent with isRealmUrl', () => {
        for (const pattern of REALM_BLOCKED_URL_PATTERNS) {
            expect(pattern).toMatch(/^\*:\/\/.+\*$/)
            const sample = 'https://' + pattern.slice('*://'.length, -1) + 'sample'
            expect(isRealmUrl(sample), pattern).toBe(true)
        }
    })
})

describe('vitest Realm fetch guard', () => {
    it('throws before the request leaves the test', () => {
        expect(() => fetch(`${REALM_HUB_URL}/realm/search`)).toThrow(/RisuRealm/)
    })

    it('inspects URL and Request inputs, not just strings', () => {
        expect(() => fetch(new URL(`${REALM_SITE_URL}/upload`))).toThrow(/RisuRealm/)
        expect(() => fetch(new Request(`${REALM_HUB_URL}/hub/info/id`))).toThrow(/RisuRealm/)
    })

    it('reports the offending URL on the console even when the caller swallows the error', () => {
        const error = vi.spyOn(console, 'error').mockImplementation(() => {})
        expect(() => fetch(`${REALM_HUB_URL}/realm/search`)).toThrow()
        expect(error).toHaveBeenCalledWith(expect.stringContaining(`${REALM_HUB_URL}/realm/search`))
    })

    it('survives a test that stubs and then unstubs fetch', () => {
        vi.stubGlobal('fetch', vi.fn())
        vi.unstubAllGlobals()
        expect(() => fetch(`${REALM_HUB_URL}/realm/search`)).toThrow(/RisuRealm/)
    })

    it('also stops happy-dom iframe navigation, which bypasses globalThis.fetch', async () => {
        const error = vi.spyOn(console, 'error').mockImplementation(() => {})
        // happy-dom reports the failed navigation on the console it captured at
        // environment setup, which is not the object `vi.spyOn(console, ...)` patches.
        // Keep that expected line out of a passing run.
        vi.spyOn(process.stderr, 'write').mockImplementation(() => true)
        const frame = document.createElement('iframe')
        const outcome = new Promise<string>((resolve) => {
            frame.addEventListener('load', () => resolve('load'))
            frame.addEventListener('error', () => resolve('error'))
        })
        frame.src = `${REALM_HUB_URL}/realm/guard-probe`
        document.body.appendChild(frame)
        try {
            await expect(outcome).resolves.toBe('error')
            expect(error).toHaveBeenCalledWith(expect.stringContaining(`${REALM_HUB_URL}/realm/guard-probe`))
        } finally {
            frame.remove()
        }
    })
})

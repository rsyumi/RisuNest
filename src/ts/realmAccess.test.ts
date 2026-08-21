import { describe, expect, it } from 'vitest'
import { isRealmAccessDisabled } from './realmAccess'

describe('RisuRealm test access', () => {
    it('disables Realm only when the explicit test flag is true', () => {
        expect(isRealmAccessDisabled()).toBe(true)
        expect(isRealmAccessDisabled({ VITE_DISABLE_REALM: 'true' })).toBe(true)
    })

    it('keeps Realm compatible when the test flag is absent or false', () => {
        expect(isRealmAccessDisabled({})).toBe(false)
        expect(isRealmAccessDisabled({ VITE_DISABLE_REALM: 'false' })).toBe(false)
    })
})

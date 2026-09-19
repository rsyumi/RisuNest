import { describe, expect, it } from 'vitest'
import {
    isExpectedHubMessage,
    resolveExpectedOfficialAccountMessageUrl,
} from './officialAccountMessage'

describe('official account Hub message provenance', () => {
    const expectedSource = {} as Window

    it('requires the exact Hub origin and expected iframe or window source', () => {
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: expectedSource,
        }, 'https://sv.risuai.xyz/hub/login', expectedSource)).toBe(true)

        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz.attacker.example',
            source: expectedSource,
        }, 'https://sv.risuai.xyz/hub/login', expectedSource)).toBe(false)

        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: {} as Window,
        }, 'https://sv.risuai.xyz/hub/login', expectedSource)).toBe(false)
    })

    it('rejects unbound, null, and closed message sources', () => {
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: expectedSource,
        }, 'https://sv.risuai.xyz/hub/login')).toBe(false)

        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: null,
        }, 'https://sv.risuai.xyz/hub/login', expectedSource)).toBe(false)

        const closedSource = { closed: true } as Window
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: closedSource,
        }, 'https://sv.risuai.xyz/hub/login', closedSource)).toBe(false)
    })

    it('keeps the configured Hub origin for account iframe messages', () => {
        expect(resolveExpectedOfficialAccountMessageUrl(
            'account',
            'https://nightly.risuai.xyz',
            '',
        )).toBe('https://nightly.risuai.xyz/hub/login')
    })
})

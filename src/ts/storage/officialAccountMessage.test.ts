import { describe, expect, it, vi } from 'vitest'
import {
    createHubPopupController,
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

    it('tracks only the Window returned by the Drive popup boundary and clears it on close', () => {
        const popup = { closed: false, close: vi.fn() } as unknown as Window
        const openWindow = vi.fn(() => popup)
        const controller = createHubPopupController(openWindow)

        expect(controller.source).toBeNull()
        expect(controller.open('https://sv.risuai.xyz/drive')).toBe(popup)
        expect(controller.source).toBe(popup)
        expect(controller.open('https://sv.risuai.xyz/drive')).toBe(popup)
        expect(openWindow).toHaveBeenCalledOnce()
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: popup,
        }, 'https://sv.risuai.xyz/drive', controller.source)).toBe(true)

        controller.close()

        expect(popup.close).toHaveBeenCalledOnce()
        expect(controller.source).toBeNull()
    })

    it.each([
        'https://nightly.risuai.xyz',
        'https://risu.example.test',
    ])('accepts only the production Drive callback origin and popup source for hub %s', (hubUrl) => {
        const popup = {} as Window
        const expectedUrl = resolveExpectedOfficialAccountMessageUrl('drive', hubUrl, '')

        expect(expectedUrl).toBe('https://sv.risuai.xyz/drive')
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: popup,
        }, expectedUrl, popup)).toBe(true)
        expect(isExpectedHubMessage({
            origin: new URL(hubUrl).origin,
            source: popup,
        }, expectedUrl, popup)).toBe(false)
        expect(isExpectedHubMessage({
            origin: 'https://sv.risuai.xyz',
            source: {} as Window,
        }, expectedUrl, popup)).toBe(false)
    })

    it('keeps the configured Hub origin for account iframe messages', () => {
        expect(resolveExpectedOfficialAccountMessageUrl(
            'account',
            'https://nightly.risuai.xyz',
            '',
        )).toBe('https://nightly.risuai.xyz/hub/login')
    })
})

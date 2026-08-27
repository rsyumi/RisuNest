import { Buffer } from 'buffer'
import { describe, expect, it, vi } from 'vitest'

import {
    decodePreparedNativePngCardMetadata,
    InvalidPreparedNativePngCardError,
    UnsupportedPreparedNativeCharacterCardError,
} from './nativePngCardAdapter'

function encoded(value: unknown): string {
    return Buffer.from(JSON.stringify(value), 'utf8').toString('base64')
}

function v2(name = 'V2'): Record<string, unknown> {
    return {
        spec: 'chara_card_v2',
        spec_version: '2.0',
        data: {
            name,
            character_version: 7,
            extensions: {},
        },
    }
}

function v3(name = 'V3'): Record<string, unknown> {
    return {
        spec: 'chara_card_v3',
        spec_version: '3.0',
        data: { name, extensions: {} },
    }
}

const unusedRccDependencies = {
    hash: vi.fn(async () => 'unused'),
    decrypt: vi.fn(async () => new Uint8Array()),
    requestPassword: vi.fn(async () => null),
}

describe('prepared native PNG card metadata adapter', () => {
    it('selects ccv3 over stale chara and returns no encoded payload', async () => {
        const result = await decodePreparedNativePngCardMetadata({
            chara: 'not-valid-base64',
            ccv3: encoded(v3()),
        }, unusedRccDependencies)

        expect(result).toEqual(v3())
        expect(JSON.stringify(result)).not.toContain(encoded(v3()))
    })

    it('decodes v2 and normalizes a numeric character_version', async () => {
        const result = await decodePreparedNativePngCardMetadata({
            chara: encoded(v2()),
        }, unusedRccDependencies)

        expect(result).toMatchObject({
            spec: 'chara_card_v2',
            data: { character_version: '7' },
        })
    })

    it('keeps an off-spec Tavern card available to the compatibility mapper', async () => {
        const card = {
            name: 'Tavern',
            description: 'Description',
            first_mes: 'Hello',
        }

        await expect(decodePreparedNativePngCardMetadata({
            chara: encoded(card),
        }, unusedRccDependencies)).resolves.toEqual(card)
    })

    it('verifies and decrypts an RCC card with the requested password', async () => {
        const encrypted = Uint8Array.of(1, 2, 3)
        const metadata = encoded({ usePassword: true })
        const dependencies = {
            hash: vi.fn(async () => 'verified-hash'),
            decrypt: vi.fn(async () => Buffer.from(JSON.stringify(v2('Encrypted')), 'utf8')),
            requestPassword: vi.fn(async () => 'secret'),
        }

        const result = await decodePreparedNativePngCardMetadata({
            chara: `rcc||rccv1||${Buffer.from(encrypted).toString('base64')}||verified-hash||${metadata}`,
        }, dependencies)

        expect(result).toMatchObject({ data: { name: 'Encrypted' } })
        expect(dependencies.hash).toHaveBeenCalledWith(encrypted)
        expect(dependencies.requestPassword).toHaveBeenCalledOnce()
        expect(dependencies.decrypt).toHaveBeenCalledWith(encrypted, 'secret')
    })

    it('uses the legacy RISU_NONE key for an RCC card without a password', async () => {
        const encrypted = Uint8Array.of(4, 5, 6)
        const dependencies = {
            hash: vi.fn(async () => 'verified-hash'),
            decrypt: vi.fn(async () => Buffer.from(JSON.stringify(v3('No Password')), 'utf8')),
            requestPassword: vi.fn(async () => 'must-not-be-used'),
        }

        await decodePreparedNativePngCardMetadata({
            chara: `rcc||rccv1||${Buffer.from(encrypted).toString('base64')}||verified-hash||${encoded({ usePassword: false })}`,
        }, dependencies)

        expect(dependencies.requestPassword).not.toHaveBeenCalled()
        expect(dependencies.decrypt).toHaveBeenCalledWith(encrypted, 'RISU_NONE')
    })

    it('returns null when the password prompt is cancelled', async () => {
        const encrypted = Uint8Array.of(7)
        const dependencies = {
            hash: vi.fn(async () => 'verified-hash'),
            decrypt: vi.fn(async () => Buffer.from('{}')),
            requestPassword: vi.fn(async () => null),
        }

        await expect(decodePreparedNativePngCardMetadata({
            chara: `rcc||rccv1||${Buffer.from(encrypted).toString('base64')}||verified-hash||${encoded({ usePassword: true })}`,
        }, dependencies)).resolves.toBeNull()
        expect(dependencies.decrypt).not.toHaveBeenCalled()
    })

    it('rejects a mismatched RCC ciphertext hash', async () => {
        const encrypted = Uint8Array.of(8)
        const dependencies = {
            hash: vi.fn(async () => 'actual-hash'),
            decrypt: vi.fn(async () => Buffer.from('{}')),
            requestPassword: vi.fn(async () => null),
        }

        await expect(decodePreparedNativePngCardMetadata({
            chara: `rcc||rccv1||${Buffer.from(encrypted).toString('base64')}||expected-hash||${encoded({})}`,
        }, dependencies)).rejects.toBeInstanceOf(InvalidPreparedNativePngCardError)
        expect(dependencies.decrypt).not.toHaveBeenCalled()
    })

    it('classifies inline v3 and v2 payloads for legacy fallback before activation', async () => {
        const inlineV3 = v3() as any
        inlineV3.data.assets = [{ type: 'icon', name: 'main', uri: 'data:image/png;base64,AA==' }]
        await expect(decodePreparedNativePngCardMetadata({
            ccv3: encoded(inlineV3),
        }, unusedRccDependencies)).rejects.toBeInstanceOf(UnsupportedPreparedNativeCharacterCardError)

        const inlineV2 = v2() as any
        inlineV2.data.extensions.risuai = {
            additionalAssets: [['inline', 'AA==', 'png']],
        }
        await expect(decodePreparedNativePngCardMetadata({
            chara: encoded(inlineV2),
        }, unusedRccDependencies)).rejects.toBeInstanceOf(UnsupportedPreparedNativeCharacterCardError)
    })
})

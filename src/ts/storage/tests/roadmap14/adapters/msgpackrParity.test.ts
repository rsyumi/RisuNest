import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { Unpackr } from 'msgpackr/index-no-eval'
import { describe, expect, it, vi } from 'vitest'

import { decodeRisuSave } from '../../../risuSave'
import { canonicalSha256 } from '../canonicalCompatibility'

vi.mock('../../../database.svelte', () => ({ presetTemplate: {} }))
vi.mock('../../../../globalApi.svelte', () => ({ forageStorage: {} }))
vi.mock('src/ts/platform', () => ({ isNodeServer: false, isTauri: false }))

interface ParityFixture {
    msgpackrVersion: string
    expectedCanonicalSha256: string
    payloadBase64: string
    historicalPrefixedBase64: string
    expectedProjection: Record<string, unknown>
    expectedObjectKeys: {
        root: string[]
        unknown: string[]
        ordered: string[]
        pluginStorage: string[]
    }
    unknownExtensionBase64: string
}

function readFixture(): ParityFixture {
    return JSON.parse(readFileSync(resolve(
        'src/ts/storage/tests/roadmap14/adapters/fixtures/legacy/msgpackr-parity-v1.json',
    ), 'utf8')) as ParityFixture
}

function persistedProjection(value: unknown): unknown {
    return JSON.parse(JSON.stringify(value))
}

describe('Roadmap 14 msgpackr cross-language fixture', () => {
    it('freezes number, map-order, extension, undefined, and unknown-field semantics', () => {
        const fixture = readFixture()
        const decoder = new Unpackr({ int64AsType: 'number', useRecords: false })
        const decoded = decoder.decode(Buffer.from(fixture.payloadBase64, 'base64')) as Record<string, any>

        expect(fixture.msgpackrVersion).toBe('1.10.1')
        expect(Object.keys(decoded)).toEqual(fixture.expectedObjectKeys.root)
        expect(Object.keys(decoded.roadmap14Unknown)).toEqual(fixture.expectedObjectKeys.unknown)
        expect(Object.keys(decoded.roadmap14Unknown.ordered)).toEqual(fixture.expectedObjectKeys.ordered)
        expect(Object.keys(decoded.pluginCustomStorage)).toEqual(fixture.expectedObjectKeys.pluginStorage)
        expect(decoded.roadmap14Unknown.persistedDate).toBeInstanceOf(Date)
        expect(decoded.roadmap14Unknown.omitted).toBeUndefined()

        const projection = persistedProjection(decoded)
        expect(projection).toEqual(fixture.expectedProjection)
        expect(canonicalSha256(projection)).toBe(fixture.expectedCanonicalSha256)
    })

    it('freezes msgpackr rejection of unknown extension values', () => {
        const fixture = readFixture()
        const decoder = new Unpackr({ int64AsType: 'number', useRecords: false })

        expect(() => decoder.decode(Buffer.from(fixture.unknownExtensionBase64, 'base64')))
            .toThrow(/Unknown extension(?: type)? 42/)
    })

    it('freezes the exact historical RISU-prefixed fallback', async () => {
        const fixture = readFixture()
        const decoded = await decodeRisuSave(
            Buffer.from(fixture.historicalPrefixedBase64, 'base64'),
        )
        const projection = persistedProjection(decoded)

        expect(projection).toEqual(fixture.expectedProjection)
        expect(canonicalSha256(projection)).toBe(fixture.expectedCanonicalSha256)
    })
})

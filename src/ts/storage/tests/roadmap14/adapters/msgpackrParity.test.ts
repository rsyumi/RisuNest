import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'

import { Packr, Unpackr } from 'msgpackr/index-no-eval'
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
        prototypeCollision: string[]
    }
    dateEncodings: {
        invalid: string
        year10000: string
        maximum: string
        negativeSubMillisecond: string
    }
    edgePayloadBase64: string
    edgeExpectedCanonicalSha256: string
    edgeExpectedDecodedKeys: {
        undefinedSanitizedFirst: string[]
        undefinedSanitizedLast: string[]
    }
    edgeExpectedProjection: Record<string, unknown>
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
        expect(decoded.roadmap14Unknown.invalidDate).toBeInstanceOf(Date)
        expect(Number.isNaN(decoded.roadmap14Unknown.invalidDate.getTime())).toBe(true)
        expect(decoded.roadmap14Unknown.year10000.toISOString())
            .toBe('+010000-01-01T00:00:00.000Z')
        expect(decoded.roadmap14Unknown.maximumDate.toISOString())
            .toBe('+275760-09-13T00:00:00.000Z')
        expect(Object.keys(decoded.roadmap14Unknown.prototypeCollision))
            .toEqual(fixture.expectedObjectKeys.prototypeCollision)
        expect(decoded.roadmap14Unknown.prototypeCollision.__proto_).toBe('literal-last')
        expect(Object.hasOwn(decoded.roadmap14Unknown.prototypeCollision, '__proto__')).toBe(false)
        expect(decoded.roadmap14Unknown.omitted).toBeUndefined()

        const encoder = new Packr({ useRecords: false })
        const year10000 = new Date(0)
        year10000.setUTCFullYear(10000, 0, 1)
        year10000.setUTCHours(0, 0, 0, 0)
        expect(Buffer.from(encoder.encode(new Date(Number.NaN))).toString('hex'))
            .toBe(fixture.dateEncodings.invalid)
        expect(Buffer.from(encoder.encode(year10000)).toString('hex'))
            .toBe(fixture.dateEncodings.year10000)
        expect(Buffer.from(encoder.encode(new Date(8.64e15))).toString('hex'))
            .toBe(fixture.dateEncodings.maximum)

        const projection = persistedProjection(decoded)
        expect(projection).toEqual(fixture.expectedProjection)
        expect(canonicalSha256(projection)).toBe(fixture.expectedCanonicalSha256)
    })

    it('freezes undefined collision state and negative sub-millisecond TimeClip semantics', () => {
        const fixture = readFixture()
        const decoder = new Unpackr({ int64AsType: 'number', useRecords: false })
        const payload = Buffer.from(fixture.edgePayloadBase64, 'base64')
        const decoded = decoder.decode(payload) as Record<string, any>
        const unknown = decoded.roadmap14Unknown

        expect(Object.keys(unknown.undefinedSanitizedFirst))
            .toEqual(fixture.edgeExpectedDecodedKeys.undefinedSanitizedFirst)
        expect(unknown.undefinedSanitizedFirst.__proto_).toBe('literal-last')
        expect(Object.keys(unknown.undefinedSanitizedLast))
            .toEqual(fixture.edgeExpectedDecodedKeys.undefinedSanitizedLast)
        expect(Object.hasOwn(unknown.undefinedSanitizedLast, '__proto_')).toBe(true)
        expect(unknown.undefinedSanitizedLast.__proto_).toBeUndefined()
        expect(unknown.negativeSubMillisecond).toBeInstanceOf(Date)
        expect(unknown.negativeSubMillisecond.getTime()).toBe(-999)
        expect(unknown.negativeSubMillisecond.toISOString())
            .toBe('1969-12-31T23:59:59.001Z')
        expect(payload.toString('hex')).toContain(fixture.dateEncodings.negativeSubMillisecond)

        const projection = persistedProjection(decoded)
        expect(projection).toEqual(fixture.edgeExpectedProjection)
        expect(canonicalSha256(projection)).toBe(fixture.edgeExpectedCanonicalSha256)
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

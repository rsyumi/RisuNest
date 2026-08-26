import { describe, expect, it } from 'vitest'

import {
    decodeLogicalManifest,
    encodeLogicalManifest,
    hashLogicalManifest,
    type LogicalManifest,
} from './logicalManifest'

const recordHash = '1'.repeat(64)
const dependencyHash = '2'.repeat(64)

function fixture(): LogicalManifest {
    return {
        schema: 'risunest.logical-manifest/v1',
        libraryId: 'library-1',
        generation: 'generation-2',
        generationSequence: '2',
        parentGeneration: 'generation-1',
        sourceRevision: 7,
        records: [{
            key: 'r1:root',
            state: 'live',
            objectHash: recordHash,
            dependencies: [dependencyHash],
        }],
        objects: [
            { hash: recordHash, size: 12 },
            { hash: dependencyHash, size: 4 },
        ],
    }
}

describe('logical manifest codec', () => {
    it('encodes one exact canonical byte form and hashes those bytes', async () => {
        const manifest = fixture()
        const expected = '{"schema":"risunest.logical-manifest/v1","libraryId":"library-1","generation":"generation-2","generationSequence":"2","parentGeneration":"generation-1","sourceRevision":7,"records":[{"key":"r1:root","state":"live","objectHash":"1111111111111111111111111111111111111111111111111111111111111111","dependencies":["2222222222222222222222222222222222222222222222222222222222222222"]}],"objects":[{"hash":"1111111111111111111111111111111111111111111111111111111111111111","size":12},{"hash":"2222222222222222222222222222222222222222222222222222222222222222","size":4}]}'

        expect(new TextDecoder().decode(encodeLogicalManifest(manifest))).toBe(expected)
        expect(decodeLogicalManifest(new TextEncoder().encode(expected))).toEqual(manifest)
        expect(await hashLogicalManifest(manifest)).toBe(
            'b9798f9605a7c789020f235ddf11666c5a8cf91fdf9205894667fcf14200c578',
        )
    })

    it('accepts a tombstone without object references', () => {
        const manifest: LogicalManifest = {
            ...fixture(),
            generationSequence: '3',
            records: [{
                key: 'r1:character:WyJjaGFyYWN0ZXItMSJd',
                state: 'tombstone',
                deletedGenerationSequence: '3',
            }],
            objects: [],
        }

        expect(decodeLogicalManifest(encodeLogicalManifest(manifest))).toEqual(manifest)
    })

    it.each([
        ['noncanonical generation sequence', (value: any) => { value.generationSequence = '02' }],
        ['future tombstone sequence', (value: any) => {
            value.generationSequence = '2'
            value.records = [{
                key: 'r1:character:WyJjaGFyYWN0ZXItMSJd',
                state: 'tombstone',
                deletedGenerationSequence: '3',
            }]
            value.objects = []
        }],
        ['invalid logical key', (value: any) => { value.records[0].key = 'root' }],
        ['uppercase object hash', (value: any) => { value.records[0].objectHash = 'A'.repeat(64) }],
        ['unsorted dependencies', (value: any) => {
            value.records[0].dependencies = [dependencyHash, recordHash]
        }],
        ['missing referenced object', (value: any) => { value.objects.pop() }],
        ['unreachable object', (value: any) => {
            value.objects.push({ hash: '3'.repeat(64), size: 1 })
        }],
        ['unsorted records', (value: any) => {
            value.records.push({
                key: 'r1:preset:WyIwIl0', state: 'live', objectHash: recordHash,
                dependencies: [dependencyHash],
            })
        }],
        ['duplicate objects', (value: any) => { value.objects.push(value.objects[1]) }],
        ['unsafe size', (value: any) => { value.objects[0].size = Number.MAX_SAFE_INTEGER + 1 }],
        ['nonempty hash for zero bytes', (value: any) => { value.objects[0].size = 0 }],
        ['extra top-level field', (value: any) => { value.extra = true }],
        ['extra live-record field', (value: any) => { value.records[0].extra = true }],
        ['extra object field', (value: any) => { value.objects[0].extra = true }],
    ] as const)('rejects %s', (_description, mutate) => {
        const value: any = fixture()
        mutate(value)
        expect(() => encodeLogicalManifest(value)).toThrow(TypeError)
    })

    it('rejects valid JSON bytes that are not the canonical encoding', () => {
        const canonical = new TextDecoder().decode(encodeLogicalManifest(fixture()))
        const reordered = canonical.replace(
            '{"schema":"risunest.logical-manifest/v1","libraryId":"library-1"',
            '{"libraryId":"library-1","schema":"risunest.logical-manifest/v1"',
        )

        expect(() => decodeLogicalManifest(new TextEncoder().encode(reordered))).toThrow(
            'canonical',
        )
    })

    it('accepts the SHA-256 identity of an empty object only with size zero', () => {
        const emptyHash = 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855'
        const manifest = fixture()
        manifest.records[0] = {
            key: 'r1:root',
            state: 'live',
            objectHash: emptyHash,
            dependencies: [],
        }
        manifest.objects = [{ hash: emptyHash, size: 0 }]

        expect(decodeLogicalManifest(encodeLogicalManifest(manifest))).toEqual(manifest)
    })
})

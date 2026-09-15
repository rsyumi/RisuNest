import { bench, describe, expect } from 'vitest'
import {
    decodeHypaReadFrame,
    encodeHypaWriteFrame,
    toVectorBuffer,
    type HypaEmbeddingEntry,
} from './hypaEmbeddingCache'

const ENTRIES = 1000
const DIMENSIONS = 1536

function deterministicVector(seed: number): Float32Array {
    let state = (seed * 2654435761) >>> 0
    const vector = new Float32Array(DIMENSIONS)
    for (let index = 0; index < DIMENSIONS; index++) {
        state ^= state << 13
        state ^= state >>> 17
        state ^= state << 5
        state >>>= 0
        vector[index] = state / 4294967296 - 0.5
    }
    return vector
}

const vectors = Array.from({ length: ENTRIES }, (_, index) => deterministicVector(index + 1))
const entries: HypaEmbeddingEntry[] = vectors.map((vector, index) => ({
    key: index.toString(16).padStart(64, '0'),
    producer: 'hypa-v2',
    model: 'bench-model',
    endpoint: null,
    preprocessVersion: 1,
    dimensions: DIMENSIONS,
    vector: toVectorBuffer(vector),
}))

/** What the renderer used to hand the store: plain JSON number arrays. */
const numberArrayPayload = entries.map((entry, index) => ({
    key: entry.key,
    embedding: Array.from(vectors[index]),
}))

const frame = encodeHypaWriteFrame(entries)
const readFrame = (() => {
    const header = {
        entries: entries.map((entry) => ({
            key: entry.key,
            dimensions: entry.dimensions,
            byteLength: entry.vector.byteLength,
        })),
    }
    const encoded = new TextEncoder().encode(JSON.stringify(header))
    const bytes = new Uint8Array(
        4 + encoded.length + entries.reduce((sum, entry) => sum + entry.vector.byteLength, 0),
    )
    new DataView(bytes.buffer).setUint32(0, encoded.length, true)
    bytes.set(encoded, 4)
    let offset = 4 + encoded.length
    for (const entry of entries) {
        bytes.set(new Uint8Array(entry.vector), offset)
        offset += entry.vector.byteLength
    }
    return bytes
})()
const numberArrayJson = JSON.stringify(numberArrayPayload)

console.log(
    `hypa-embedding-transport ${JSON.stringify({
        entries: ENTRIES,
        dimensions: DIMENSIONS,
        frameBytes: frame.byteLength,
        jsonNumberArrayBytes: Buffer.byteLength(numberArrayJson),
    })}`,
)

describe('embedding cache transport', () => {
    bench('encodes one round as a Float32 frame', () => {
        expect(encodeHypaWriteFrame(entries).byteLength).toBe(frame.byteLength)
    })

    bench('serializes the same round as JSON number arrays', () => {
        expect(JSON.stringify(numberArrayPayload).length).toBe(numberArrayJson.length)
    })

    bench('decodes one round from a Float32 frame', () => {
        expect(decodeHypaReadFrame(readFrame).size).toBe(ENTRIES)
    })

    bench('parses the same round from JSON number arrays', () => {
        expect(JSON.parse(numberArrayJson)).toHaveLength(ENTRIES)
    })
})

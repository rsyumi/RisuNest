import localforage from 'localforage'
import { invoke } from '@tauri-apps/api/core'
import { isTauri, isTauriAndroid } from '../platform'

/** Vectors travel as Float32 little endian, never as JSON number arrays. */
export const HYPA_VECTOR_ELEMENT_BYTES = 4
export const HYPA_CACHE_BATCH_SIZE = 1024

export interface HypaCachedEmbedding {
    vector: Float32Array
    dimensions: number
}

export interface HypaEmbeddingEntry {
    key: string
    producer: string
    model: string
    endpoint: string | null
    preprocessVersion: number
    dimensions: number
    vector: ArrayBuffer
    metadata?: string | null
}

export interface HypaEmbeddingCache {
    read(keys: string[]): Promise<Map<string, HypaCachedEmbedding>>
    write(entries: HypaEmbeddingEntry[]): Promise<void>
}

type InvokeCommand = <T>(command: string, args?: unknown) => Promise<T>

interface WriteHeaderEntry {
    key: string
    producer: string
    model: string
    endpoint: string | null
    preprocessVersion: number
    dimensions: number
    byteLength: number
    metadata: string | null
}

interface ReadHeaderEntry {
    key: string
    dimensions: number
    byteLength: number
}

const HEADER_LENGTH_BYTES = 4

function chunk<T>(items: T[], size: number): T[][] {
    const chunks: T[][] = []
    for (let offset = 0; offset < items.length; offset += size) {
        chunks.push(items.slice(offset, offset + size))
    }
    return chunks
}

export function encodeHypaWriteFrame(entries: HypaEmbeddingEntry[]): Uint8Array {
    const header: { entries: WriteHeaderEntry[] } = {
        entries: entries.map((entry) => {
            if (
                !Number.isSafeInteger(entry.dimensions) ||
                entry.dimensions <= 0 ||
                entry.vector.byteLength !== entry.dimensions * HYPA_VECTOR_ELEMENT_BYTES
            ) {
                throw new RangeError('Embedding vector length does not match its dimensions')
            }
            return {
                key: entry.key,
                producer: entry.producer,
                model: entry.model,
                endpoint: entry.endpoint,
                preprocessVersion: entry.preprocessVersion,
                dimensions: entry.dimensions,
                byteLength: entry.vector.byteLength,
                metadata: entry.metadata ?? null,
            }
        }),
    }
    const encodedHeader = new TextEncoder().encode(JSON.stringify(header))
    const bodyBytes = entries.reduce((sum, entry) => sum + entry.vector.byteLength, 0)
    const frame = new Uint8Array(HEADER_LENGTH_BYTES + encodedHeader.length + bodyBytes)
    new DataView(frame.buffer).setUint32(0, encodedHeader.length, true)
    frame.set(encodedHeader, HEADER_LENGTH_BYTES)
    let offset = HEADER_LENGTH_BYTES + encodedHeader.length
    for (const entry of entries) {
        frame.set(new Uint8Array(entry.vector), offset)
        offset += entry.vector.byteLength
    }
    return frame
}

export function decodeHypaReadFrame(frame: Uint8Array): Map<string, HypaCachedEmbedding> {
    if (frame.byteLength < HEADER_LENGTH_BYTES) {
        throw new TypeError('Embedding cache returned a malformed frame')
    }
    const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength)
    const headerLength = view.getUint32(0, true)
    const bodyStart = HEADER_LENGTH_BYTES + headerLength
    if (bodyStart > frame.byteLength) {
        throw new TypeError('Embedding cache returned a malformed frame')
    }
    const header = JSON.parse(
        new TextDecoder().decode(frame.subarray(HEADER_LENGTH_BYTES, bodyStart)),
    ) as { entries?: ReadHeaderEntry[] }
    const results = new Map<string, HypaCachedEmbedding>()
    let offset = bodyStart
    for (const entry of header.entries ?? []) {
        const end = offset + entry.byteLength
        if (end > frame.byteLength) {
            throw new TypeError('Embedding cache returned a truncated frame')
        }
        if (entry.dimensions > 0 && entry.byteLength > 0) {
            // Copy out: the frame is not aligned for a Float32Array view.
            const vector = new Float32Array(entry.dimensions)
            new Uint8Array(vector.buffer).set(frame.subarray(offset, end))
            results.set(entry.key, { vector, dimensions: entry.dimensions })
        }
        offset = end
    }
    return results
}

/** The read response is an ArrayBuffer wherever raw IPC responses land. */
function toBytes(value: unknown): Uint8Array {
    if (value instanceof ArrayBuffer) return new Uint8Array(value)
    if (ArrayBuffer.isView(value)) {
        const view = value as ArrayBufferView
        return new Uint8Array(view.buffer, view.byteOffset, view.byteLength)
    }
    if (Array.isArray(value)) return Uint8Array.from(value as number[])
    throw new TypeError('Embedding cache returned an unsupported response')
}

export function createNativeHypaEmbeddingCache(
    invokeCommand: InvokeCommand = invoke as InvokeCommand,
    android: boolean = isTauriAndroid,
): HypaEmbeddingCache {
    return {
        async read(keys: string[]): Promise<Map<string, HypaCachedEmbedding>> {
            const results = new Map<string, HypaCachedEmbedding>()
            for (const batch of chunk(keys, HYPA_CACHE_BATCH_SIZE)) {
                const response = await invokeCommand<unknown>('pds_read_hypa_embeddings', {
                    keys: batch,
                })
                for (const [key, value] of decodeHypaReadFrame(toBytes(response))) {
                    results.set(key, value)
                }
            }
            return results
        },
        async write(entries: HypaEmbeddingEntry[]): Promise<void> {
            for (const batch of chunk(entries, HYPA_CACHE_BATCH_SIZE)) {
                const frame = encodeHypaWriteFrame(batch)
                // Android cannot deliver a raw request body, so it carries the
                // same frame as base64 instead of a JSON number array.
                if (android) {
                    await invokeCommand<void>('pds_write_hypa_embeddings', {
                        payload: Buffer.from(
                            frame.buffer,
                            frame.byteOffset,
                            frame.byteLength,
                        ).toString('base64'),
                    })
                } else {
                    await invokeCommand<void>('pds_write_hypa_embeddings', frame)
                }
            }
        },
    }
}

interface StoredEmbedding {
    dimensions: number
    vector: ArrayBuffer
}

/** The web build has no device tier, so it keeps the browser cache and only
 *  adopts the normalized keys. */
export function createLocalHypaEmbeddingCache(
    forage: LocalForage = localforage.createInstance({ name: 'hypaVector' }),
): HypaEmbeddingCache {
    return {
        async read(keys: string[]): Promise<Map<string, HypaCachedEmbedding>> {
            const results = new Map<string, HypaCachedEmbedding>()
            const loaded = await Promise.all(
                keys.map(async (key) => {
                    try {
                        return [key, await forage.getItem<StoredEmbedding>(key)] as const
                    } catch {
                        return [key, null] as const
                    }
                }),
            )
            for (const [key, stored] of loaded) {
                if (!stored || !stored.vector || !(stored.dimensions > 0)) continue
                results.set(key, {
                    vector: new Float32Array(stored.vector),
                    dimensions: stored.dimensions,
                })
            }
            return results
        },
        async write(entries: HypaEmbeddingEntry[]): Promise<void> {
            for (const entry of entries) {
                await forage.setItem<StoredEmbedding>(entry.key, {
                    dimensions: entry.dimensions,
                    vector: entry.vector,
                })
            }
        },
    }
}

/** Every miss is recoverable by recomputing, so an unavailable store must slow
 *  an embedding round down rather than fail it. */
export function resilientHypaEmbeddingCache(inner: HypaEmbeddingCache): HypaEmbeddingCache {
    return {
        async read(keys: string[]): Promise<Map<string, HypaCachedEmbedding>> {
            try {
                return await inner.read(keys)
            } catch (error) {
                console.warn('Embedding cache read failed', error)
                return new Map()
            }
        },
        async write(entries: HypaEmbeddingEntry[]): Promise<void> {
            try {
                await inner.write(entries)
            } catch (error) {
                console.warn('Embedding cache write failed', error)
            }
        },
    }
}

let cache: HypaEmbeddingCache | null = null

export function getHypaEmbeddingCache(): HypaEmbeddingCache {
    cache ??= resilientHypaEmbeddingCache(
        isTauri ? createNativeHypaEmbeddingCache() : createLocalHypaEmbeddingCache(),
    )
    return cache
}

export function toVectorBuffer(vector: number[] | Float32Array): ArrayBuffer {
    const floats = vector instanceof Float32Array ? vector : Float32Array.from(vector)
    if (floats.byteOffset === 0 && floats.buffer.byteLength === floats.byteLength) {
        return floats.buffer as ArrayBuffer
    }
    return floats.slice().buffer as ArrayBuffer
}

/** A batch must describe one embedding width. Mixed widths mean the model
 *  behind the identity changed, so every cached vector is recomputed. */
export function consistentEmbeddings(
    hits: Map<string, HypaCachedEmbedding>,
): Map<string, HypaCachedEmbedding> {
    let dimensions = 0
    for (const hit of hits.values()) {
        if (dimensions === 0) {
            dimensions = hit.dimensions
        } else if (hit.dimensions !== dimensions) {
            return new Map()
        }
    }
    return hits
}

/** Cached vectors that disagree with freshly computed ones are rejected. */
export function staleEmbeddingKeys(
    hits: Map<string, HypaCachedEmbedding>,
    dimensions: number,
): string[] {
    if (!(dimensions > 0)) return []
    const stale: string[] = []
    for (const [key, hit] of hits) {
        if (hit.dimensions !== dimensions) stale.push(key)
    }
    return stale
}

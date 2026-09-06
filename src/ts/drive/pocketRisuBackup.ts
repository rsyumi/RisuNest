import type { BlobWriteMetadata, InlayBlobType } from '../storage/blobStore'
import { inferBlobMime, normalizeBlobExtension } from '../storage/blobStore'

export type PocketRisuEntry =
    | { kind: 'inlay-data', id: string, ext: string }
    | { kind: 'inlay-legacy', id: string }
    | { kind: 'inlay-sidecar', id: string }
    | { kind: 'inlay-info', id: string }
    | { kind: 'skip' }

export interface PocketRisuInlaySidecar {
    ext: string
    name: string
    type: InlayBlobType
    width?: number
    height?: number
}

const INLAY_TYPES = new Set<InlayBlobType>(['image', 'video', 'audio', 'signature'])

const inlayTypeByExtension: Record<string, InlayBlobType> = {
    avif: 'image', gif: 'image', jpeg: 'image', jpg: 'image', png: 'image', webp: 'image',
    flac: 'audio', mp3: 'audio', ogg: 'audio', wav: 'audio',
    mkv: 'video', mp4: 'video', webm: 'video',
}

function singleSegment(name: string, prefix: string): string | null {
    const rest = name.slice(prefix.length)
    if (rest === '' || rest.includes('/') || rest.includes('\\')) return null
    return rest
}

/**
 * Maps a PocketRisu backup entry name to its handler, or null when the entry
 * uses the shared RisuNest namespace (database, cold storage, flat assets).
 */
export function classifyPocketRisuEntry(name: string): PocketRisuEntry | null {
    if (name.startsWith('inlay/')) {
        const rest = singleSegment(name, 'inlay/')
        if (!rest) return { kind: 'skip' }
        const dot = rest.lastIndexOf('.')
        if (dot <= 0 || dot === rest.length - 1) return { kind: 'inlay-legacy', id: rest }
        return { kind: 'inlay-data', id: rest.slice(0, dot), ext: rest.slice(dot + 1) }
    }
    if (name.startsWith('inlay_sidecar/')) {
        const rest = singleSegment(name, 'inlay_sidecar/')
        return rest ? { kind: 'inlay-sidecar', id: rest } : { kind: 'skip' }
    }
    if (name.startsWith('inlay_info/')) {
        const rest = singleSegment(name, 'inlay_info/')
        return rest ? { kind: 'inlay-info', id: rest } : { kind: 'skip' }
    }
    if (name.startsWith('inlay_meta/') || name.startsWith('inlay_thumb/')) {
        return { kind: 'skip' }
    }
    return null
}

function parseJson(data: Uint8Array): unknown | null {
    try {
        return JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(data))
    } catch {
        return null
    }
}

function toSidecar(value: unknown): PocketRisuInlaySidecar | null {
    if (!value || typeof value !== 'object' || Array.isArray(value)) return null
    const record = value as Record<string, unknown>
    if (typeof record.ext !== 'string' || typeof record.name !== 'string') return null
    if (!INLAY_TYPES.has(record.type as InlayBlobType)) return null
    const sidecar: PocketRisuInlaySidecar = {
        ext: record.ext,
        name: record.name,
        type: record.type as InlayBlobType,
    }
    if (typeof record.width === 'number') sidecar.width = record.width
    if (typeof record.height === 'number') sidecar.height = record.height
    return sidecar
}

export function decodePocketRisuInlaySidecar(data: Uint8Array): PocketRisuInlaySidecar | null {
    return toSidecar(parseJson(data))
}

function decodeLegacyPayloadData(data: string, type: InlayBlobType): Uint8Array | null {
    if (type === 'signature') return new TextEncoder().encode(data)
    const base64 = data.startsWith('data:') ? data.split(',')[1] : data
    if (!base64) return null
    try {
        return new Uint8Array(Buffer.from(base64, 'base64'))
    } catch {
        return null
    }
}

function buildMetadata(sidecar: PocketRisuInlaySidecar): BlobWriteMetadata {
    return {
        kind: 'inlay',
        inlayType: sidecar.type,
        mime: sidecar.type === 'signature'
            ? 'application/json'
            : inferBlobMime(undefined, sidecar.ext),
        name: sidecar.name,
        ext: sidecar.ext,
        ...(sidecar.width === undefined ? {} : { width: sidecar.width }),
        ...(sidecar.height === undefined ? {} : { height: sidecar.height }),
    }
}

type InlayPut = (id: string, data: Uint8Array, metadata: BlobWriteMetadata) => Promise<void>

/**
 * Pairs PocketRisu inlay payloads with their sidecar metadata. Entries can
 * arrive in any order, so both halves are buffered until the pair completes.
 */
export class PocketRisuInlayImporter {
    private readonly pendingData = new Map<string, { data: Uint8Array, ext: string | null }>()
    private readonly pendingSidecars = new Map<string, PocketRisuInlaySidecar>()
    readonly failedIds: string[] = []

    constructor(private readonly put: InlayPut) {}

    async add(entry: Exclude<PocketRisuEntry, { kind: 'skip' }>, data: Uint8Array): Promise<void> {
        switch (entry.kind) {
            case 'inlay-data':
                this.pendingData.set(entry.id, { data, ext: entry.ext })
                break
            case 'inlay-legacy': {
                const parsed = data[0] === 0x7b ? parseJson(data) : null
                const record = parsed && typeof parsed === 'object' && !Array.isArray(parsed)
                    ? parsed as Record<string, unknown>
                    : null
                if (record && typeof record.data === 'string') {
                    const sidecar = toSidecar(record)
                    const bytes = sidecar ? decodeLegacyPayloadData(record.data, sidecar.type) : null
                    if (!sidecar || !bytes) {
                        this.failedIds.push(entry.id)
                        return
                    }
                    await this.store(entry.id, bytes, sidecar)
                    return
                }
                this.pendingData.set(entry.id, { data, ext: null })
                break
            }
            case 'inlay-sidecar':
            case 'inlay-info': {
                const sidecar = decodePocketRisuInlaySidecar(data)
                if (!sidecar) {
                    this.failedIds.push(entry.id)
                    return
                }
                this.pendingSidecars.set(entry.id, sidecar)
                break
            }
        }
        await this.flushPaired()
    }

    async finish(): Promise<void> {
        await this.flushPaired()
        for (const [id, pending] of this.pendingData) {
            const ext = pending.ext === null ? '' : normalizeBlobExtension(pending.ext)
            const type = inlayTypeByExtension[ext]
            if (!type) {
                this.failedIds.push(id)
                continue
            }
            await this.store(id, pending.data, { ext, name: `${id}.${ext}`, type })
        }
        this.pendingData.clear()
        this.pendingSidecars.clear()
    }

    private async flushPaired(): Promise<void> {
        for (const [id, pending] of this.pendingData) {
            const sidecar = this.pendingSidecars.get(id)
            if (!sidecar) continue
            this.pendingData.delete(id)
            this.pendingSidecars.delete(id)
            await this.store(id, pending.data, sidecar)
        }
    }

    private async store(id: string, data: Uint8Array, sidecar: PocketRisuInlaySidecar): Promise<void> {
        try {
            await this.put(id, data, buildMetadata(sidecar))
        } catch {
            this.failedIds.push(id)
        }
    }
}

import { normalizeBlobExtension, type InlayBlobMetadata, type InlayBlobType } from "../../storage/blobStore";

/** Number of stored inlay assets in a group and the bytes they occupy. */
export interface InlayInventoryTotals {
    count: number
    bytes: number
}

/** One row of the inventory table. `ext` is empty when the asset has no extension. */
export interface InlayInventoryEntry extends InlayInventoryTotals {
    ext: string
}

export interface InlayInventory {
    total: InlayInventoryTotals
    byType: Record<InlayBlobType, InlayInventoryTotals>
    /** Image extensions, most numerous first, ties broken by extension. */
    images: InlayInventoryEntry[]
    /** Audio, video, and signature extensions in the same order. */
    others: InlayInventoryEntry[]
}

const inlayTypes: readonly InlayBlobType[] = ['image', 'video', 'audio', 'signature']

function emptyTotals(): InlayInventoryTotals {
    return { count: 0, bytes: 0 }
}

function sorted(entries: Map<string, InlayInventoryEntry>): InlayInventoryEntry[] {
    return [...entries.values()].sort((left, right) => (
        right.count - left.count || (left.ext < right.ext ? -1 : left.ext > right.ext ? 1 : 0)
    ))
}

/**
 * Groups stored inlay assets by extension. Extensions are only lowercased, so
 * `jpg` and `jpeg` stay apart: import paths keep the original extension, and
 * merging them would hide what is actually stored.
 */
export function summarizeInlayAssets(metadata: readonly InlayBlobMetadata[]): InlayInventory {
    const total = emptyTotals()
    const byType = Object.fromEntries(
        inlayTypes.map((type) => [type, emptyTotals()]),
    ) as Record<InlayBlobType, InlayInventoryTotals>
    const images = new Map<string, InlayInventoryEntry>()
    const others = new Map<string, InlayInventoryEntry>()
    for (const item of metadata) {
        const ext = normalizeBlobExtension(item.ext)
        const rows = item.inlayType === 'image' ? images : others
        const entry = rows.get(ext) ?? { ext, ...emptyTotals() }
        entry.count += 1
        entry.bytes += item.size
        rows.set(ext, entry)
        byType[item.inlayType].count += 1
        byType[item.inlayType].bytes += item.size
        total.count += 1
        total.bytes += item.size
    }
    return { total, byType, images: sorted(images), others: sorted(others) }
}

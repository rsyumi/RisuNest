import type { loreBook } from '../storage/database.svelte'

export function loreListWindow(
    items: readonly loreBook[],
    folder: string,
    page: number,
    size = 60,
    pinned: readonly loreBook[] = [],
    pinEdge: 'start' | 'end' = 'start',
) {
    const matching: { book: loreBook; i: number }[] = []
    items.forEach((book, i) => {
        if ((!folder && !book.folder) || folder === book.folder)
            matching.push({ book, i })
    })
    const boundedPage = Math.min(
        page,
        Math.max(0, Math.ceil(matching.length / size) - 1),
    )
    const start = boundedPage * size
    const rows = matching.slice(start, start + size)
    // Rows kept alive through a page change stay mounted at the edge the reader came from.
    const extra = matching.filter(
        (row, position) =>
            pinned.includes(row.book) &&
            (position < start || position >= start + size),
    )
    return {
        total: matching.length,
        rows: pinEdge === 'start' ? [...extra, ...rows] : [...rows, ...extra],
    }
}

/** The page that shows `book` in its folder level, or null when it is not there. */
export function lorePageOf(
    items: readonly loreBook[],
    folder: string,
    book: loreBook,
    size = 60,
): number | null {
    let position = 0
    for (const item of items) {
        if (!((!folder && !item.folder) || folder === item.folder)) continue
        if (item === book) return Math.floor(position / size)
        position++
    }
    return null
}

/** Insert before the next visible row, or after the previous one, in the full array. */
export function loreDropIndex(
    source: number,
    next: number | null,
    previous: number | null,
    length: number,
): number {
    const insertion = next ?? (previous === null ? length : previous + 1)
    return Math.max(
        0,
        Math.min(length - 1, insertion - (source < insertion ? 1 : 0)),
    )
}

/** Puts the row at `source` into the folder, first among its entries. Null when the folder cannot take it or nothing changes. */
export function dropLoreOnFolder(
    items: readonly loreBook[],
    source: number,
    folderKey: string | null,
): loreBook[] | null {
    const moved = items[source]
    const folder = items.find(item => item.mode === 'folder' && item.key === folderKey)
    if (!moved || !folder || moved.mode === 'folder') return null
    if (items.find(item => item.folder === folder.key) === moved) return null
    moved.folder = folder.key
    const others = items.filter((_, index) => index !== source)
    // A folder lists its entries in array order, which may not follow the folder row.
    const firstEntry = others.findIndex(item => item.folder === folder.key)
    others.splice(firstEntry === -1 ? others.indexOf(folder) + 1 : firstEntry, 0, moved)
    return groupLoreFolders(others)
}

export function groupLoreFolders(items: loreBook[]): loreBook[] {
    const children = new Map<string, loreBook[]>()
    const folders = new Set(items.filter(item => item.mode === 'folder').map(item => item.key))
    for (const item of items) {
        if (item.folder) {
            const bucket = children.get(item.folder) ?? []
            bucket.push(item)
            children.set(item.folder, bucket)
        }
    }
    const seen = new Set<loreBook>()
    const result: loreBook[] = []
    for (const item of items) {
        if (item.folder && folders.has(item.folder)) continue
        if (seen.has(item)) continue
        seen.add(item)
        result.push(item)
        if (item.mode === 'folder') {
            for (const child of children.get(item.key) ?? []) {
                if (!seen.has(child)) {
                    seen.add(child)
                    result.push(child)
                }
            }
        }
    }
    // Keep malformed or nested imported entries rather than dropping them.
    for (const item of items) {
        if (!seen.has(item)) {
            seen.add(item)
            result.push(item)
        }
    }
    return result
}

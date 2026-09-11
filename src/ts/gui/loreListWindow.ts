import type { loreBook } from '../storage/database.svelte'

export function loreListWindow(
    items: readonly loreBook[],
    folder: string,
    page: number,
    size = 60,
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
    return {
        total: matching.length,
        rows: matching.slice(boundedPage * size, (boundedPage + 1) * size),
    }
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

export function groupLoreFolders(items: loreBook[]): loreBook[] {
    const children = new Map<string, loreBook[]>()
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
    return result
}

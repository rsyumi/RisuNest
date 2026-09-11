import { expect, it } from 'vitest'
import {
    groupLoreFolders,
    loreDropIndex,
    loreListWindow,
} from './loreListWindow'
import type { loreBook } from '../storage/database.svelte'

it('bounds 10,000 rows while retaining exact backing indices through folders and deletion', () => {
    const items = Array.from(
        { length: 10_000 },
        (_, i) =>
            ({
                key: String(i),
                folder: i % 2 ? 'folder' : undefined,
            }) as loreBook,
    )
    const window = loreListWindow(items, 'folder', 1)
    expect(window.total).toBe(5000)
    expect(window.rows).toHaveLength(60)
    expect(window.rows[0]).toEqual({ book: items[121], i: 121 })
    items.splice(121, 1)
    expect(loreListWindow(items, 'folder', 1).rows[0].i).toBe(122)
    expect(loreListWindow(items, '', 999).rows.length).toBeGreaterThan(0)
})

it('moves backing rows correctly in both directions and across a filtered folder boundary', () => {
    expect(loreDropIndex(1, 7, 5, 10)).toBe(6)
    expect(loreDropIndex(7, 1, null, 10)).toBe(1)
    expect(loreDropIndex(1, null, 7, 10)).toBe(7)
    expect(loreDropIndex(7, null, null, 10)).toBe(9)
    const folder = { mode: 'folder', key: 'f' } as loreBook
    const child = { folder: 'f' } as loreBook
    const other = {} as loreBook
    expect(groupLoreFolders([folder, other, child])).toEqual([
        folder,
        child,
        other,
    ])
})

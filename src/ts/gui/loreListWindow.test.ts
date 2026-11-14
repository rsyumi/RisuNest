import { expect, it } from 'vitest'
import {
    dropLoreOnFolder,
    groupLoreFolders,
    loreDropIndex,
    loreListWindow,
    lorePageOf,
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

it('keeps a moved folder together with children that precede its new position', () => {
    const folder = { mode: 'folder', key: 'f' } as loreBook
    const child = { folder: 'f' } as loreBook
    const orphan = { folder: 'missing' } as loreBook
    const other = {} as loreBook
    expect(groupLoreFolders([child, other, folder, orphan])).toEqual([other, folder, child, orphan])
})

it('drops a row on a folder as the first of its entries', () => {
    const keys = (rows: loreBook[] | null) => rows?.map(row => row.key) ?? null
    const lore = () => [
        { mode: 'folder', key: 'f' },
        { key: 'c', folder: 'f' },
        { mode: 'folder', key: 'g' },
        { key: 'oc', folder: 'g' },
        { key: 'r' },
    ] as loreBook[]
    const fromRoot = lore()
    expect(keys(dropLoreOnFolder(fromRoot, 4, 'f'))).toEqual(['f', 'r', 'c', 'g', 'oc'])
    expect(fromRoot[4].folder).toBe('f')
    const fromFolder = lore()
    expect(keys(dropLoreOnFolder(fromFolder, 3, 'f'))).toEqual(['f', 'oc', 'c', 'g', 'r'])
    expect(fromFolder[3].folder).toBe('f')
    // Entries stored ahead of their folder row still list after the dropped one.
    const [folder, child, , , root] = lore()
    expect(keys(dropLoreOnFolder([child, root, folder], 1, 'f'))).toEqual(['f', 'r', 'c'])
})

it('refuses a folder, an unknown folder, and a row already first in the folder', () => {
    const folder = { mode: 'folder', key: 'f' } as loreBook
    const other = { mode: 'folder', key: 'g' } as loreBook
    const child = { key: 'c', folder: 'f' } as loreBook
    const root = { key: 'r' } as loreBook
    const items = [folder, child, other, root]
    expect(dropLoreOnFolder(items, 2, 'f')).toBeNull()
    expect(other.folder).toBeUndefined()
    expect(dropLoreOnFolder(items, 3, 'missing')).toBeNull()
    expect(dropLoreOnFolder(items, 3, 'c')).toBeNull()
    expect(root.folder).toBeUndefined()
    expect(dropLoreOnFolder(items, 1, 'f')).toBeNull()
})

it('keeps pinned rows from other pages mounted at the requested edge', () => {
    const items = Array.from({ length: 10 }, (_, i) => ({ key: String(i) }) as loreBook)
    const keys = (rows: { book: loreBook }[]) => rows.map(row => row.book.key)
    expect(keys(loreListWindow(items, '', 1, 4, [items[1]]).rows)).toEqual(['1', '4', '5', '6', '7'])
    expect(keys(loreListWindow(items, '', 1, 4, [items[9], items[8]], 'end').rows)).toEqual(['4', '5', '6', '7', '8', '9'])
    expect(keys(loreListWindow(items, '', 1, 4, [items[5]]).rows)).toEqual(['4', '5', '6', '7'])
    expect(loreListWindow(items, '', 1, 4, [items[1]]).total).toBe(10)
    const child = { key: 'c', folder: 'f' } as loreBook
    expect(keys(loreListWindow([...items, child], '', 0, 4, [child]).rows)).toEqual(['0', '1', '2', '3'])
})

it('finds the page of a row within its folder level', () => {
    const folder = { mode: 'folder', key: 'f' } as loreBook
    const children = Array.from({ length: 5 }, (_, i) => ({ key: `c${i}`, folder: 'f' }) as loreBook)
    const root = Array.from({ length: 5 }, (_, i) => ({ key: `r${i}` }) as loreBook)
    const items = [folder, ...children, ...root]
    expect(lorePageOf(items, 'f', children[4], 2)).toBe(2)
    expect(lorePageOf(items, '', root[0], 2)).toBe(0)
    expect(lorePageOf(items, '', root[1], 2)).toBe(1)
    expect(lorePageOf(items, '', children[0], 2)).toBeNull()
})

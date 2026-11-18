import { describe, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import { parseToggleSyntax, type sidebarToggle } from './util'
import {
    addToggleRow,
    groupSidebarToggles,
    moveToggleRow,
    parseToggleDefinitions,
    removeToggleRow,
    serializeToggleDefinitions,
    setToggleRowField,
    setToggleRowKind,
    toggleOutline,
    type ToggleLine,
    type ToggleLineKind,
} from './toggleDefinitions'

vi.mock('./storage/database.svelte', () => ({}))
vi.mock('./characters', () => ({}))
vi.mock('./stores.svelte', () => ({ DBState: { db: {} }, selectedCharID: writable(0) }))
vi.mock('./process/modules', () => ({ getModuleToggles: () => '' }))
vi.mock('src/lib/UI/PopupList.svelte', () => ({ default: {} }))

// Synthetic definitions covering every form the sidebar reads and lines it skips.
const forms = [
    '',
    'solo=Solo switch',
    'mode=Mode=select=Fast,Slow,,Odd value',
    'notes=Notes=text\nstory=Story=textarea',
    'trailing=Trailing\n',
    '\n\nspaced=Spaced\n\n\n',
    '  padded = Padded name  \n\tindented=Tab\t',
    '=Group=group\ninside=Inside\n=Group=groupEnd\nafter=After',
    '=Open=group\nfirst=First\nsecond=Second=select=A,B',
    '=One=group\na=A\n=Two=group\nb=B\n=Two=groupEnd\nc=C',
    '=Empty=group\n=Empty=groupEnd',
    '==divider\n=Heading=caption\n==caption\nkey=Value=divider',
    'no equals sign\n=only value\nonly key=\n=\n==\n===',
    'extra=Extra=unknown=more=segments\nodd=Odd=SELECT=x',
    '=Stray=groupEnd\nlone=Lone',
    '모델=한국어 이름=select=하나,둘\n=그룹=group\n안=안쪽\n=그룹=groupEnd',
    'a=A\r\n=Mixed=group\r\nb=B\n=Mixed=groupEnd\nc=C\r\n',
    '=Rows=group\na=A\r\nb=B=text\r\n=Rows=groupEnd\nc=C\r\n=Next=group\nd=D\r\n',
]
const corpus = [
    ...forms,
    ...forms.map((text) => text.replaceAll('\r\n', '\n').replaceAll('\n', '\r\n')),
]

type Shape = { type: string; key?: string; value?: string; options?: string[]; children?: Shape[] }

function shape(toggle: sidebarToggle): Shape {
    if (toggle.type === 'group') return { type: 'group', value: toggle.value, children: toggle.children.map(shape) }
    if (toggle.type === 'select') return { type: 'select', key: toggle.key, value: toggle.value, options: toggle.options }
    return { type: toggle.type ?? 'switch', key: toggle.key, value: toggle.value }
}

/** What the sidebar shows for a definition text. */
const sidebar = (text: string) => groupSidebarToggles(parseToggleSyntax(text)).map(shape)

/** What the editor lists for its lines, read back through the sidebar's own parser line by line. */
function editorRows(lines: ToggleLine[]): Shape[] {
    const read = (index: number) => shape(parseToggleSyntax(lines[index].raw)[0])
    return toggleOutline(lines).map((item) => item.type === 'group'
        ? { ...read(item.line), children: item.children.map(read) }
        : read(item.line))
}

const lineIds = (lines: ToggleLine[]) => toggleOutline(lines).flatMap((item) =>
    item.type === 'group' ? [lines[item.line].id, ...item.children.map((child) => lines[child].id)] : [lines[item.line].id])
const idOf = (lines: ToggleLine[], raw: string) => lines.find((line) => line.raw === raw)!.id
const edit = (text: string, change: (lines: ToggleLine[]) => ToggleLine[]) =>
    serializeToggleDefinitions(change(parseToggleDefinitions(text)))

describe('toggle definition text', () => {
    it('writes back every unedited text byte for byte', () => {
        for (const text of corpus) {
            expect(serializeToggleDefinitions(parseToggleDefinitions(text))).toBe(text)
        }
    })

    it('lists exactly the rows and groups the sidebar shows, and nothing it skips', () => {
        for (const text of corpus) {
            expect(editorRows(parseToggleDefinitions(text)), JSON.stringify(text)).toEqual(sidebar(text))
        }
    })

    it('groups the toggles between a group and its end the way the sidebar does', () => {
        expect(sidebar('a=A\n=One=group\nb=B\n=Two=group\nc=C\n=Two=groupEnd\nd=D\n=Stray=groupEnd\ne=E')).toEqual([
            { type: 'switch', key: 'a', value: 'A' },
            { type: 'group', value: 'One', children: [{ type: 'switch', key: 'b', value: 'B' }] },
            { type: 'group', value: 'Two', children: [{ type: 'switch', key: 'c', value: 'C' }] },
            { type: 'switch', key: 'd', value: 'D' },
            { type: 'switch', key: 'e', value: 'E' },
        ])
    })
})

describe('toggle row edits', () => {
    it('changes only the edited field and keeps the rest of the line', () => {
        expect(edit('extra=Old=unknown=more', (lines) => setToggleRowField(lines, lines[0].id, 'name', 'New')))
            .toBe('extra=New=unknown=more')
        expect(edit('a=A\r\nmode=Mode=select=x,y\r\nb=B', (lines) => setToggleRowField(lines, lines[1].id, 'options', 'x,y,z')))
            .toBe('a=A\r\nmode=Mode=select=x,y,z\r\nb=B')
        expect(edit('=', (lines) => setToggleRowField(lines, lines[0].id, 'key', 'ne=w\r\nkey'))).toBe('newkey=')
        expect(edit('k=N=select', (lines) => setToggleRowField(lines, lines[0].id, 'options', 'A,B'))).toBe('k=N=select=A,B')
    })

    it('rewrites a row for its new type and keeps groups closed', () => {
        expect(edit('a=A\nb=B', (lines) => setToggleRowKind(lines, lines[0].id, 'group'))).toBe('=A=group\n=A=groupEnd\nb=B')
        expect(edit('k=N', (lines) => setToggleRowKind(lines, lines[0].id, 'select'))).toBe('k=N=select=')
        expect(edit('k=N=select=a', (lines) => setToggleRowKind(lines, lines[0].id, 'divider'))).toBe('=N=divider')
        expect(edit('=G=group\nc=C\n=G=groupEnd\nd=D', (lines) => setToggleRowKind(lines, lines[0].id, 'switch')))
            .toBe('=G\nc=C\nd=D')
        const after = edit('=One=group\na=A\n=Two=group\nb=B\n=Two=groupEnd\nc=C',
            (lines) => setToggleRowKind(lines, idOf(lines, '=Two=group'), 'caption'))
        expect(after).toBe('=One=group\na=A\n=One=groupEnd\n=Two=caption\nb=B\nc=C')
        expect(sidebar(after).map((item) => item.value)).toEqual(['One', 'Two', 'B', 'C'])
    })

    it('does not put a group inside a group', () => {
        const text = '=G=group\nc=C\n=G=groupEnd'
        expect(edit(text, (lines) => setToggleRowKind(lines, idOf(lines, 'c=C'), 'group'))).toBe(text)
    })

    it('keeps the rows of a removed group', () => {
        expect(edit('=G=group\na=A\n=G=groupEnd\nb=B', (lines) => removeToggleRow(lines, lines[0].id))).toBe('a=A\nb=B')
        expect(edit('=One=group\na=A\n=Two=group\nb=B', (lines) => removeToggleRow(lines, idOf(lines, '=Two=group'))))
            .toBe('=One=group\na=A\n=One=groupEnd\nb=B')
        expect(edit('a=A\r\nb=B\r\nc=C', (lines) => removeToggleRow(lines, idOf(lines, 'b=B\r')))).toBe('a=A\r\nc=C')
    })

    it('adds a row at the end, outside any group, before a final line break', () => {
        expect(edit('', addToggleRow)).toBe('=')
        expect(edit('a=A\n', addToggleRow)).toBe('a=A\n=\n')
        const lines = addToggleRow(parseToggleDefinitions('=G=group\na=A'))
        expect(serializeToggleDefinitions(lines)).toBe('=G=group\na=A\n=G=groupEnd\n=')
        expect(toggleOutline(lines).at(-1)).toEqual({ type: 'row', line: 3 })
    })

    it('moves rows into, out of and past groups', () => {
        const move = (text: string, raw: string, direction: -1 | 1) =>
            edit(text, (lines) => moveToggleRow(lines, idOf(lines, raw), direction))
        expect(move('a=A\nb=B', 'b=B', -1)).toBe('b=B\na=A')
        expect(move('a=A\n=G=group\nb=B\n=G=groupEnd', 'a=A', 1)).toBe('=G=group\na=A\nb=B\n=G=groupEnd')
        expect(move('=G=group\nb=B\n=G=groupEnd\na=A', 'a=A', -1)).toBe('=G=group\nb=B\na=A\n=G=groupEnd')
        expect(move('=G=group\nb=B\nc=C\n=G=groupEnd', 'b=B', -1)).toBe('b=B\n=G=group\nc=C\n=G=groupEnd')
        expect(move('=G=group\nb=B\nc=C', 'b=B', 1)).toBe('=G=group\nc=C\nb=B')
        expect(move('=G=group\nb=B', 'b=B', 1)).toBe('=G=group\n=G=groupEnd\nb=B')
        expect(move('=One=group\na=A\n=Two=group\nb=B\n=Two=groupEnd', 'b=B', -1))
            .toBe('=One=group\na=A\n=One=groupEnd\nb=B\n=Two=group\n=Two=groupEnd')
        expect(move('a=A\n=G=group\nb=B\n=G=groupEnd', '=G=group', -1)).toBe('=G=group\nb=B\n=G=groupEnd\na=A')
        expect(move('=One=group\na=A\n=Two=group\nb=B', '=Two=group', -1))
            .toBe('=Two=group\nb=B\n=Two=groupEnd\n=One=group\na=A\n=One=groupEnd')
        const after = move('=Zero=group\nz=Z\n=G=group\nb=B\n=G=groupEnd\nr=R', '=G=group', 1)
        expect(after).toBe('=Zero=group\nz=Z\n=Zero=groupEnd\nr=R\n=G=group\nb=B\n=G=groupEnd')
        expect(sidebar(after).map((item) => item.value)).toEqual(['Zero', 'R', 'G'])
    })
})

describe('toggle edits over the whole corpus', () => {
    const kinds: Exclude<ToggleLineKind, 'groupEnd'>[] = ['switch', 'select', 'text', 'textarea', 'group', 'divider', 'caption']
    const visible = (text: string) => sidebar(text)
        .flatMap((item) => [item, ...(item.children ?? [])])
        .map((item) => JSON.stringify({ ...item, children: undefined }))
        .sort()

    /**
     * Every line other than the edited one keeps its bytes. A moved row keeps its own bytes too; other edits keep
     * the order of the untouched lines, and may only drop the edited group's end line. Added lines are group ends
     * or the new empty row.
     */
    function check(before: ToggleLine[], after: ToggleLine[], target: number, moved: boolean) {
        const known = new Set(before.map((line) => line.id))
        const group = toggleOutline(before).find((item) => item.type === 'group' && before[item.line].id === target)
        const end = group?.type === 'group' && group.end !== null ? before[group.end] : null
        if (moved) {
            const byId = (lines: ToggleLine[]) => lines.map((line) => `${line.id}:${line.raw}`).sort()
            expect(byId(after.filter((line) => known.has(line.id)))).toEqual(byId(before))
        } else {
            const untouched = (lines: ToggleLine[]) => lines
                .filter((line) => known.has(line.id) && line.id !== target && line.id !== end?.id)
                .map((line) => line.raw)
            expect(untouched(after)).toEqual(untouched(before))
            const keptEnd = after.find((line) => line.id === end?.id)
            if (keptEnd) expect(keptEnd.raw).toBe(end!.raw)
        }
        for (const line of after.filter((line) => !known.has(line.id))) {
            expect(/^=[^=\r]*=groupEnd$/.test(line.raw) || line.raw === '=', line.raw).toBe(true)
        }
    }

    it('keeps every line it does not edit byte for byte, in order', () => {
        for (const text of corpus) {
            const lines = parseToggleDefinitions(text)
            for (const id of lineIds(lines)) {
                check(lines, setToggleRowField(lines, id, 'name', 'Renamed'), id, false)
                check(lines, removeToggleRow(lines, id), id, false)
                for (const kind of kinds) check(lines, setToggleRowKind(lines, id, kind), id, false)
                for (const direction of [-1, 1] as const) {
                    const after = moveToggleRow(lines, id, direction)
                    check(lines, after, id, true)
                    expect(visible(serializeToggleDefinitions(after)), JSON.stringify(text)).toEqual(visible(text))
                }
            }
            // An empty text becomes the new row alone, as checked above.
            if (text !== '') check(lines, addToggleRow(lines), -1, false)
        }
    })
})

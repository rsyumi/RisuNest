import { describe, expect, it, vi } from 'vitest'
import { writable } from 'svelte/store'
import { parseToggleSyntax, type sidebarToggle } from './util'
import {
    addToggleRow,
    groupSidebarToggles,
    missingToggleFields,
    moveToggleRow,
    parseToggleDefinitions,
    placeToggleRow,
    removeToggleRow,
    serializeToggleDefinitions,
    setToggleRowField,
    setToggleRowKind,
    toggleOutline,
    type ToggleGap,
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

/** The editor rows the sidebar shows, read back through the sidebar's own parser line by line. */
function editorRows(lines: ToggleLine[]): Shape[] {
    const read = (index: number) => shape(parseToggleSyntax(lines[index].raw)[0])
    const shown = (index: number) => parseToggleSyntax(lines[index].raw).length > 0
    return toggleOutline(lines).filter((item) => shown(item.line)).map((item) => item.type === 'group'
        ? { ...read(item.line), children: item.children.filter(shown).map(read) }
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

    it('lists the rows and groups the sidebar shows, and every line it skips that is not blank in its place', () => {
        for (const text of corpus) {
            const lines = parseToggleDefinitions(text)
            expect(editorRows(lines), JSON.stringify(text)).toEqual(sidebar(text))
            const skipped = lines.flatMap((line, index) => !parseToggleSyntax(line.raw).length && line.raw.trim() ? [index] : [])
            const listed = toggleOutline(lines).flatMap((item) => item.type === 'group' ? [item.line, ...item.children] : [item.line])
            expect(listed.filter((index) => skipped.includes(index)), JSON.stringify(text)).toEqual(skipped)
        }
    })

    it('names the empty fields that keep the sidebar from showing a line', () => {
        const missing = (raw: string) => [parseToggleDefinitions(raw)[0].kind, missingToggleFields(raw)]
        expect(missing('=Only name')).toEqual(['switch', ['key']])
        expect(missing('only key=')).toEqual(['switch', ['name']])
        expect(missing('no equals sign')).toEqual(['switch', ['name']])
        expect(missing('=')).toEqual(['switch', ['key', 'name']])
        expect(missing('k==select=a,b')).toEqual(['select', ['name']])
        expect(missing('==textarea\r')).toEqual(['textarea', ['key', 'name']])
        expect(missing('==caption')).toEqual(['caption', ['name']])
        for (const shown of ['k=N', '=G=group', '==divider', 'k=N=select']) {
            expect(missing(shown)).toEqual([parseToggleDefinitions(shown)[0].kind, []])
        }
        for (const blank of ['', '\r', '  \t']) expect(missing(blank)).toEqual([null, []])
    })

    it('shows a definition saved with CRLF line breaks as the same toggles as with LF', () => {
        for (const text of forms) {
            const lf = text.replaceAll('\r\n', '\n')
            expect(sidebar(lf.replaceAll('\n', '\r\n')), JSON.stringify(text)).toEqual(sidebar(lf))
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
        // The new row stays a row when its text is read again.
        expect(toggleOutline(parseToggleDefinitions('a=A\n=\n'))).toEqual([{ type: 'row', line: 0 }, { type: 'row', line: 1 }])
        const lines = addToggleRow(parseToggleDefinitions('=G=group\na=A'))
        expect(serializeToggleDefinitions(lines)).toBe('=G=group\na=A\n=G=groupEnd\n=')
        expect(toggleOutline(lines).at(-1)).toEqual({ type: 'row', line: 3 })
    })

    it('adds a row of the chosen type at the end or at the end of a group', () => {
        const add = (text: string, kind: Exclude<ToggleLineKind, 'groupEnd'>, group?: string) =>
            edit(text, (lines) => addToggleRow(lines, kind, group === undefined ? null : idOf(lines, group)))
        expect(add('a=A\n', 'select')).toBe('a=A\n==select=\n')
        expect(add('a=A', 'group')).toBe('a=A\n==group\n==groupEnd')
        // A new group ends an open group by itself; any other row closes it first.
        expect(add('=G=group\na=A', 'group')).toBe('=G=group\na=A\n==group\n==groupEnd')
        expect(add('=G=group\na=A', 'caption')).toBe('=G=group\na=A\n=G=groupEnd\n==caption')
        expect(add('=G=group\r\na=A\r\n\n=G=groupEnd\r\nb=B', 'textarea', '=G=group\r'))
            .toBe('=G=group\r\na=A\r\n==textarea\n\n=G=groupEnd\r\nb=B')
        expect(add('=G=group\n=G=groupEnd', 'text', '=G=group')).toBe('=G=group\n==text\n=G=groupEnd')
        expect(add('=Open=group\na=A\n=Next=group', 'divider', '=Open=group')).toBe('=Open=group\na=A\n==divider\n=Next=group')
        expect(add('=G=group\na=A\n=G=groupEnd', 'group', '=G=group')).toBe('=G=group\na=A\n=G=groupEnd')
        for (const kind of ['switch', 'select', 'text', 'textarea', 'group', 'divider', 'caption'] as const) {
            const lines = addToggleRow(parseToggleDefinitions('a=A'), kind)
            expect(parseToggleDefinitions(serializeToggleDefinitions(lines))[1].kind).toBe(kind)
        }
    })

    it('places a row or a group with its rows at a gap', () => {
        const place = (text: string, raw: string, group: string | null, before: string | null) =>
            edit(text, (lines) => placeToggleRow(lines, idOf(lines, raw), {
                group: group === null ? null : idOf(lines, group),
                before: before === null ? null : idOf(lines, before),
            }))
        const grouped = 'a=A\n=G=group\nb=B\nc=C\n=G=groupEnd\nd=D'
        expect(place(grouped, 'a=A', '=G=group', 'c=C')).toBe('=G=group\nb=B\na=A\nc=C\n=G=groupEnd\nd=D')
        expect(place(grouped, 'd=D', '=G=group', null)).toBe('a=A\n=G=group\nb=B\nc=C\nd=D\n=G=groupEnd')
        expect(place(grouped, 'b=B', null, 'a=A')).toBe('b=B\na=A\n=G=group\nc=C\n=G=groupEnd\nd=D')
        expect(place(grouped, 'c=C', null, null)).toBe('a=A\n=G=group\nb=B\n=G=groupEnd\nd=D\nc=C')
        expect(place('a=A\nb=B\n', 'a=A', null, null)).toBe('b=B\na=A\n')
        // Leaving a group that has no end line for the top level after it writes the end line.
        expect(place('=G=group\nb=B\nc=C', 'b=B', null, null)).toBe('=G=group\nc=C\n=G=groupEnd\nb=B')
        expect(place('=G=group\nb=B\n=H=group\nc=C', 'c=C', null, '=H=group')).toBe('=G=group\nb=B\n=G=groupEnd\nc=C\n=H=group')
        // A group moves with its rows and end line, and one without an end line gets it before a row.
        expect(place(grouped, '=G=group', null, 'a=A')).toBe('=G=group\nb=B\nc=C\n=G=groupEnd\na=A\nd=D')
        expect(place(grouped, '=G=group', null, null)).toBe('a=A\nd=D\n=G=group\nb=B\nc=C\n=G=groupEnd')
        expect(place('a=A\n=G=group\nb=B', '=G=group', null, 'a=A')).toBe('=G=group\nb=B\n=G=groupEnd\na=A')
        expect(place('=F=group\nf=F\n=G=group\ng=G\n=G=groupEnd\nr=R', '=G=group', null, '=F=group'))
            .toBe('=G=group\ng=G\n=G=groupEnd\n=F=group\nf=F\n=F=groupEnd\nr=R')
        // A group is not placed inside a group.
        const two = '=One=group\na=A\n=One=groupEnd\n=Two=group\nb=B\n=Two=groupEnd'
        expect(place(two, '=Two=group', '=One=group', null)).toBe(two)
        expect(place(two, '=Two=group', '=One=group', 'a=A')).toBe(two)
        // A blank line stays in place, and a row placed at a gap it occupies writes nothing.
        const spaced = 'a=A\r\n\nb=B\r\n\nc=C\r\n'
        expect(place(spaced, 'c=C\r', null, 'a=A\r')).toBe('c=C\r\na=A\r\n\nb=B\r\n\n')
        for (const [raw, before] of [['b=B\r', 'b=B\r'], ['b=B\r', 'c=C\r'], ['c=C\r', null]] as const) {
            expect(place(spaced, raw, null, before)).toBe(spaced)
        }
        expect(place('=G=group\nb=B\n\nc=C', 'c=C', '=G=group', null)).toBe('=G=group\nb=B\n\nc=C')
        expect(place('=G=group\nb=B\n\nc=C', 'b=B', '=G=group', 'c=C')).toBe('=G=group\nb=B\n\nc=C')
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

    type Tree = { id: number; children?: number[] }[]
    /** Rows and groups by line id, each group with its rows. */
    const tree = (lines: ToggleLine[]): Tree => toggleOutline(lines).map((item) => item.type === 'group'
        ? { id: lines[item.line].id, children: item.children.map((child) => lines[child].id) }
        : { id: lines[item.line].id })
    /** Every gap: before each row and after the last, at the top level and in each group. */
    const gaps = (lines: ToggleLine[]): ToggleGap[] => tree(lines).flatMap((item, index, items) => [
        { group: null, before: item.id },
        ...(item.children ? [...item.children, null].map((before) => ({ group: item.id, before })) : []),
        ...(index === items.length - 1 ? [{ group: null, before: null }] : []),
    ])
    /** The tree with one item taken out and put at a gap. */
    function moveInTree(items: Tree, id: number, gap: ToggleGap): Tree {
        const next = items.map((item) => ({ ...item, children: item.children && [...item.children] }))
        const top = next.findIndex((item) => item.id === id)
        const [moved] = top !== -1 ? next.splice(top, 1) : [{ id }]
        for (const item of next) if (item.children?.includes(id)) item.children.splice(item.children.indexOf(id), 1)
        if (gap.group === null) {
            next.splice(gap.before === null ? next.length : next.findIndex((item) => item.id === gap.before), 0, moved)
        } else {
            const children = next.find((item) => item.id === gap.group)!.children!
            children.splice(gap.before === null ? children.length : children.indexOf(gap.before), 0, id)
        }
        return next
    }

    /**
     * A placement moves the item, a group with its rows, to the gap and leaves every other row in its group. Every
     * line keeps its bytes, lines that do not move keep their order, and only group end lines are added. A group
     * placed in a group and an item placed at a gap it occupies leave the text as it was.
     */
    function checkPlacement(text: string, lines: ToggleLine[], id: number, gap: ToggleGap) {
        const label = JSON.stringify({ text, id, gap })
        const after = placeToggleRow(lines, id, gap)
        const items = tree(lines)
        const group = toggleOutline(lines).find((item) => item.type === 'group' && lines[item.line].id === id)
        const container = items.find((item) => item.children?.includes(id))?.id ?? null
        const siblings = container === null ? items.map((item) => item.id) : items.find((item) => item.id === container)!.children!
        const occupied = container === gap.group && (gap.before === id || gap.before === (siblings[siblings.indexOf(id) + 1] ?? null))
        if ((group && gap.group !== null) || occupied) {
            expect(serializeToggleDefinitions(after), label).toBe(text)
            return
        }
        expect(tree(after), label).toEqual(moveInTree(items, id, gap))
        check(lines, after, id, true)
        const known = new Set(lines.map((line) => line.id))
        expect(after.filter((line) => !known.has(line.id)).every((line) => /^=[^=\r]*=groupEnd$/.test(line.raw)), label).toBe(true)
        const start = lines.findIndex((line) => line.id === id)
        const block = new Set(lines.slice(start, (group?.type === 'group' ? (group.end ?? group.children.at(-1) ?? group.line) : start) + 1).map((line) => line.id))
        const staying = (list: ToggleLine[]) => list.filter((line) => known.has(line.id) && !block.has(line.id)).map((line) => line.id)
        expect(staying(after), label).toEqual(staying(lines))
        expect(visible(serializeToggleDefinitions(after)), label).toEqual(visible(text))
    }

    it('keeps every line it does not edit byte for byte, in order', () => {
        for (const text of corpus) {
            const lines = parseToggleDefinitions(text)
            for (const id of lineIds(lines)) {
                check(lines, setToggleRowField(lines, id, 'name', 'Renamed'), id, false)
                check(lines, removeToggleRow(lines, id), id, false)
                for (const gap of gaps(lines)) checkPlacement(text, lines, id, gap)
                for (const kind of kinds) check(lines, setToggleRowKind(lines, id, kind), id, false)
                for (const direction of [-1, 1] as const) {
                    const after = moveToggleRow(lines, id, direction)
                    check(lines, after, id, true)
                    expect(visible(serializeToggleDefinitions(after)), JSON.stringify(text)).toEqual(visible(text))
                }
            }
            // An empty text becomes the new row alone, as checked above.
            if (text !== '') check(lines, addToggleRow(lines), -1, false)
            for (const group of [null, ...tree(lines).filter((item) => item.children).map((item) => item.id)]) {
                for (const kind of kinds) {
                    if (text === '') continue
                    const label = JSON.stringify({ text, group, kind })
                    const after = addToggleRow(lines, kind, group)
                    if (group !== null && kind === 'group') {
                        expect(serializeToggleDefinitions(after), label).toBe(text)
                        continue
                    }
                    const known = new Set(lines.map((line) => line.id))
                    expect(after.filter((line) => known.has(line.id)).map((line) => line.raw), label).toEqual(lines.map((line) => line.raw))
                    const [row, ...ends] = after.filter((line) => !known.has(line.id) && line.kind !== 'groupEnd')
                    expect(ends, label).toEqual([])
                    expect(parseToggleDefinitions(row.raw)[0].kind, label).toBe(kind)
                    const expected = tree(lines).map((item) => ({ ...item, children: item.children && [...item.children] }))
                    if (group === null) expected.push(kind === 'group' ? { id: row.id, children: [] } : { id: row.id })
                    else expected.find((item) => item.id === group)!.children!.push(row.id)
                    expect(tree(after), label).toEqual(expected)
                }
            }
        }
    })
})

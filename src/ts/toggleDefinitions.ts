import { get } from 'svelte/store'
import { DBState, selectedCharID } from './stores.svelte'
import { getModuleToggles } from './process/modules'
import { parseToggleSyntax, type sidebarToggle, type sidebarToggleGroup } from './util'

/** Toggle definitions shown in the sidebar: global prompt toggles, enabled modules, and the selected character's own toggles. */
export function currentToggleDefinitions(): string {
    const character = DBState.db.characters[get(selectedCharID)]
    const own = !!character ? (character.customModuleToggle ?? '') : ''
    return `${DBState.db.customPromptTemplateToggle ?? ''}\n${getModuleToggles()}\n${own}`
}

/** `toggle_` variable keys of the definitions that hold a value (switches, selects, text fields). */
export function currentToggleKeys(): string[] {
    return parseToggleSyntax(currentToggleDefinitions())
        .filter(
            (toggle) =>
                toggle.key &&
                toggle.type !== 'group' &&
                toggle.type !== 'groupEnd' &&
                toggle.type !== 'divider' &&
                toggle.type !== 'caption',
        )
        .map((toggle) => `toggle_${toggle.key}`)
}

/** Puts the toggles between a group and its groupEnd into that group, as the sidebar shows them. */
export function groupSidebarToggles(toggles: readonly sidebarToggle[]): sidebarToggle[] {
    const grouped: sidebarToggle[] = []
    let group: sidebarToggleGroup | null = null
    for (const toggle of toggles) {
        if (toggle.type === 'group') {
            group = { ...toggle, children: [] }
            grouped.push(group)
        } else if (toggle.type === 'groupEnd') {
            group = null
        } else if (group) {
            group.children.push(toggle)
        } else {
            grouped.push(toggle)
        }
    }
    return grouped
}

export type ToggleLineKind = 'switch' | 'select' | 'text' | 'textarea' | 'group' | 'groupEnd' | 'divider' | 'caption'

/** One line of a toggle definition text. */
export interface ToggleLine {
    /** Stays the same while the line is edited, so an editor row keeps its inputs. */
    id: number
    /** The line as stored, without its '\n'. A CRLF line keeps its '\r'. */
    raw: string
    /** What the sidebar reads from the line, or null when it skips the line. */
    kind: ToggleLineKind | null
}

export interface ToggleFields {
    key: string
    name: string
    options: string
}

export type ToggleOutlineItem =
    | { type: 'row'; line: number }
    | { type: 'group'; line: number; children: number[]; end: number | null }

type ToggleGroupItem = Extract<ToggleOutlineItem, { type: 'group' }>

const fieldSegment = { key: 0, name: 1, options: 3 } as const
let nextToggleLineId = 0

function createLine(raw: string, kind: ToggleLineKind | null = classifyToggleLine(raw)): ToggleLine {
    return { id: nextToggleLineId++, raw, kind }
}

/** Reads one line exactly as the sidebar does. */
function classifyToggleLine(raw: string): ToggleLineKind | null {
    const [toggle] = parseToggleSyntax(raw)
    if (!toggle) return null
    return toggle.type ?? 'switch'
}

export function parseToggleDefinitions(text: string): ToggleLine[] {
    return text.split('\n').map((raw) => createLine(raw))
}

export function serializeToggleDefinitions(lines: readonly ToggleLine[]): string {
    return lines.map((line) => line.raw).join('\n')
}

export function toggleLineFields(raw: string): ToggleFields {
    const [key = '', name = '', , options = ''] = raw.replace(/\r$/, '').split('=')
    return { key, name, options }
}

/** Field text the syntax can hold: '=' separates fields and a line break ends the line. */
export function sanitizeToggleField(value: string): string {
    return value.replace(/[=\r\n]/g, '')
}

function lineText(kind: ToggleLineKind, fields: ToggleFields): string {
    switch (kind) {
        case 'switch':
            return `${fields.key}=${fields.name}`
        case 'select':
            return `${fields.key}=${fields.name}=select=${fields.options}`
        case 'text':
        case 'textarea':
            return `${fields.key}=${fields.name}=${kind}`
        default:
            return `=${fields.name}=${kind}`
    }
}

/** Rows and groups in the order the sidebar shows them. Lines the sidebar skips are left out. */
export function toggleOutline(lines: readonly ToggleLine[]): ToggleOutlineItem[] {
    const items: ToggleOutlineItem[] = []
    let open: ToggleGroupItem | null = null
    lines.forEach((line, index) => {
        if (line.kind === null) return
        if (line.kind === 'group') {
            open = { type: 'group', line: index, children: [], end: null }
            items.push(open)
        } else if (line.kind === 'groupEnd') {
            if (open) open.end = index
            open = null
        } else if (open) {
            open.children.push(index)
        } else {
            items.push({ type: 'row', line: index })
        }
    })
    return items
}

const indexOfLine = (lines: readonly ToggleLine[], id: number) => lines.findIndex((line) => line.id === id)

function locate(outline: ToggleOutlineItem[], lines: readonly ToggleLine[], id: number) {
    const index = indexOfLine(lines, id)
    for (const [item, entry] of outline.entries()) {
        if (entry.line === index) return { item, child: -1 }
        if (entry.type === 'group') {
            const child = entry.children.indexOf(index)
            if (child !== -1) return { item, child }
        }
    }
    return null
}

function groupOf(lines: readonly ToggleLine[], headerId: number) {
    const index = indexOfLine(lines, headerId)
    return toggleOutline(lines).find((item): item is ToggleGroupItem => item.type === 'group' && item.line === index)
}

function endLine(header: ToggleLine): ToggleLine {
    return createLine(`=${toggleLineFields(header.raw).name}=groupEnd`, 'groupEnd')
}

/** Writes the groupEnd line of a group that runs on until the next group or the end of the text. */
function closeGroup(lines: ToggleLine[], headerId: number): ToggleLine[] {
    const group = groupOf(lines, headerId)
    if (!group || group.end !== null) return lines
    const at = (group.children.at(-1) ?? group.line) + 1
    return [...lines.slice(0, at), endLine(lines[group.line]), ...lines.slice(at)]
}

/** Closes the group before a top-level item, which that item's position would otherwise end. */
function closeGroupBefore(lines: ToggleLine[], id: number): ToggleLine[] {
    const outline = toggleOutline(lines)
    const position = locate(outline, lines, id)
    const previous = position && position.child === -1 ? outline[position.item - 1] : undefined
    return previous?.type === 'group' ? closeGroup(lines, lines[previous.line].id) : lines
}

function moveLine(lines: ToggleLine[], id: number, beforeId: number | null, after = false): ToggleLine[] {
    const next = [...lines]
    const [line] = next.splice(indexOfLine(next, id), 1)
    const target = beforeId === null ? next.length : indexOfLine(next, beforeId) + (after ? 1 : 0)
    next.splice(target, 0, line)
    return next
}

function swapLines(lines: ToggleLine[], a: number, b: number): ToggleLine[] {
    const next = [...lines]
    ;[next[a], next[b]] = [next[b], next[a]]
    return next
}

/** Replaces one field of a row and leaves the rest of the line, including a CRLF line's '\r', as it was. */
export function setToggleRowField(lines: readonly ToggleLine[], id: number, field: keyof ToggleFields, value: string): ToggleLine[] {
    return lines.map((line) => {
        if (line.id !== id) return line
        const ending = line.raw.endsWith('\r') ? '\r' : ''
        const segments = line.raw.slice(0, line.raw.length - ending.length).split('=')
        while (segments.length <= fieldSegment[field]) segments.push('')
        segments[fieldSegment[field]] = sanitizeToggleField(value)
        return { ...line, raw: segments.join('=') + ending }
    })
}

/**
 * Changes a row's type. A new group gets its groupEnd line at once; a group that becomes another type
 * loses its groupEnd line and leaves its rows ungrouped. Only top-level rows become groups.
 */
export function setToggleRowKind(lines: readonly ToggleLine[], id: number, kind: Exclude<ToggleLineKind, 'groupEnd'>): ToggleLine[] {
    let next = [...lines]
    const line = next[indexOfLine(next, id)]
    if (!line || line.kind === kind) return next
    const replaced: ToggleLine = { ...line, raw: lineText(kind, toggleLineFields(line.raw)), kind }
    if (line.kind === 'group') {
        next = closeGroupBefore(next, id)
        const end = groupOf(next, id)?.end ?? null
        if (end !== null) next.splice(end, 1)
    } else if (kind === 'group') {
        if (locate(toggleOutline(next), next, id)?.child !== -1) return next
        next.splice(indexOfLine(next, id) + 1, 0, endLine(replaced))
    }
    next[indexOfLine(next, id)] = replaced
    return next
}

/** Removes a row. Removing a group keeps its rows, ungrouped. */
export function removeToggleRow(lines: readonly ToggleLine[], id: number): ToggleLine[] {
    let next = [...lines]
    const line = next[indexOfLine(next, id)]
    if (!line) return next
    if (line.kind === 'group') {
        next = closeGroupBefore(next, id)
        const end = groupOf(next, id)?.end ?? null
        if (end !== null) next.splice(end, 1)
    }
    next.splice(indexOfLine(next, id), 1)
    return next
}

/** Adds an empty switch row at the end, outside any group. */
export function addToggleRow(lines: readonly ToggleLine[]): ToggleLine[] {
    let next = [...lines]
    const last = toggleOutline(next).at(-1)
    if (last?.type === 'group') next = closeGroup(next, next[last.line].id)
    const row = createLine('=', 'switch')
    if (next.length === 1 && next[0].raw === '') return [row]
    // Keep a final line break after the new row.
    next.splice(next.at(-1)?.raw === '' ? next.length - 1 : next.length, 0, row)
    return next
}

/**
 * Moves a row one step up or down as the outline shows it. A row next to a group enters it, the first or
 * last row of a group leaves it, and a group moves with its rows past the item next to it.
 */
export function moveToggleRow(lines: readonly ToggleLine[], id: number, direction: -1 | 1): ToggleLine[] {
    let next = [...lines]
    const outline = toggleOutline(next)
    const position = locate(outline, next, id)
    if (!position) return next
    const item = outline[position.item]
    if (item.type === 'group' && position.child !== -1) {
        const target = position.child + direction
        if (target >= 0 && target < item.children.length) {
            return swapLines(next, item.children[position.child], item.children[target])
        }
        const headerId = next[item.line].id
        if (direction === -1) {
            next = closeGroupBefore(next, headerId)
            return moveLine(next, id, headerId)
        }
        if (item.end === null) {
            next.splice(indexOfLine(next, id), 0, endLine(next[item.line]))
            return next
        }
        return moveLine(next, id, next[item.end].id, true)
    }
    const neighbor = outline[position.item + direction]
    if (!neighbor) return next
    const neighborId = next[neighbor.line].id
    if (item.type === 'row') {
        if (neighbor.type === 'row') return swapLines(next, item.line, neighbor.line)
        if (direction === 1) return moveLine(next, id, neighborId, true)
        next = closeGroup(next, neighborId)
        return moveLine(next, id, next[groupOf(next, neighborId)!.end!].id)
    }
    // A group passes the neighboring item with its rows. Both are closed so neither takes in the rows it passes,
    // and so is the group before them, which a row moved to the front would otherwise join.
    next = closeGroupBefore(closeGroup(closeGroup(next, id), neighborId), direction === -1 ? neighborId : id)
    const range = (headerId: number): [number, number] => {
        const start = indexOfLine(next, headerId)
        return [start, groupOf(next, headerId)?.end ?? start]
    }
    const [first, second] = direction === -1 ? [range(neighborId), range(id)] : [range(id), range(neighborId)]
    return [
        ...next.slice(0, first[0]),
        ...next.slice(second[0], second[1] + 1),
        ...next.slice(first[1] + 1, second[0]),
        ...next.slice(first[0], first[1] + 1),
        ...next.slice(second[1] + 1),
    ]
}

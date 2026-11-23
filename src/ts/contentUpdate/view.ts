// Adapted from RisuBard (https://github.com/rpaddict/RisuBard, src/ts/characterUpdateView.ts
// and src/ts/storage/externalChangeDiff.ts), GPL-3.0.

import type { UpdateChoice, UpdateItem } from './merge'

// Body fields are always shown as blocks so both sides line up in the diff.
const blockKeys = new Set(['content', 'in', 'out'])
const longText = (key: string, value: string) => blockKeys.has(key) || value.includes('\n') || value.length > 60
const isRecord = (value: unknown) => !!value && typeof value === 'object' && !Array.isArray(value)

/** Plain text of one side of an update item, for the line diff. */
export function updateValueText(value: unknown): string {
    if (value === undefined || value === null) return ''
    if (typeof value === 'string') return value
    if (Array.isArray(value)) {
        if (value.every(item => typeof item === 'string')) return value.join('\n---\n')
        return JSON.stringify(value, null, 2)
    }
    if (typeof value === 'object') {
        const record = value as Record<string, unknown>
        const lines: string[] = []
        const blocks: string[] = []
        for (const key of Object.keys(record).sort()) {
            const item = record[key]
            if (item === undefined || key === 'id' || key === 'loreCache') continue
            if (typeof item === 'string' && longText(key, item)) blocks.push(`[${key}]\n${item}`)
            // Entries such as trigger effects, so the code they hold shows line by line.
            else if (Array.isArray(item) && item.length && item.every(isRecord)) blocks.push(`[${key}]\n${item.map(updateValueText).join('\n---\n')}`)
            else lines.push(`${key}: ${JSON.stringify(item)}`)
        }
        return [...lines, ...blocks].join('\n')
    }
    return JSON.stringify(value)
}

/** The entry's own name inside a list field: lore title, script comment, asset name. */
export function updateItemName(item: UpdateItem): string {
    if (!item.key) return ''
    if (item.field === 'extentions') return item.key
    const value = (item.mine ?? item.theirs) as any
    if (Array.isArray(value)) return String(value[0] ?? '')
    // A regex without a name goes by its pattern, as the order item lists it.
    if (value && typeof value === 'object') return String(value.comment || value.name || value.key || value.in || '').trim()
    return ''
}

/**
 * The text the user can rewrite while comparing: a whole text field, the body
 * of a lore entry, or the output of a regex. Null when the item has none.
 */
function editableKey(item: UpdateItem): '' | 'content' | 'out' | null {
    const value = item.mine ?? item.theirs
    if (typeof value === 'string') return ''
    if (value && typeof value === 'object' && !Array.isArray(value)) {
        const record = value as Record<string, unknown>
        if (typeof record.content === 'string') return 'content'
        if (typeof record.out === 'string' && typeof record.in === 'string') return 'out'
    }
    return null
}

export function isEditable(item: UpdateItem): boolean {
    return editableKey(item) !== null
}

/** What 내 버전 or 새 버전 writes: on an entry merged field by field, only the fields both changed differ. */
export function updateSideValue(item: UpdateItem, side: 'mine' | 'theirs'): unknown {
    if (item.resolved && item.status === 'conflict') return item.resolved[side]
    return side === 'mine' ? item.mine : item.theirs
}

/** The entry the user's text goes into: the merged entry, or what 내 버전 writes. */
function editBase(item: UpdateItem): unknown {
    return item.resolved?.mine ?? item.mine ?? item.theirs
}

function textOf(item: UpdateItem, value: unknown): string {
    const key = editableKey(item)
    if (key === null || value === undefined) return ''
    // The other side may hold another shape, which has no text to start from.
    const text = key === '' ? value : (value as Record<string, unknown> | null)?.[key]
    return typeof text === 'string' ? text : ''
}

export function editableText(item: UpdateItem, side: 'mine' | 'theirs'): string {
    return textOf(item, updateSideValue(item, side) ?? updateSideValue(item, side === 'mine' ? 'theirs' : 'mine'))
}

/** The text an edit starts from before the user picks a side to start from. */
export function editStartText(item: UpdateItem): string {
    return textOf(item, editBase(item))
}

/**
 * What the user's text writes: a text field itself, or the text in an entry whose other fields are what
 * 내 버전 writes when the update is written.
 */
export function editedChoice(item: UpdateItem, text: string): UpdateChoice {
    const key = editableKey(item)
    return key === '' ? { value: text } : { edited: { [key as string]: text } }
}

export type UpdateGroupId = 'info' | 'prompt' | 'lore' | 'regex' | 'trigger' | 'asset' | 'other'
export const updateGroupOrder: UpdateGroupId[] = ['info', 'prompt', 'lore', 'regex', 'trigger', 'asset', 'other']

const groupFields: Record<Exclude<UpdateGroupId, 'other'>, string[]> = {
    info: ['name', 'desc', 'description', 'firstMessage', 'alternateGreetings', 'personality', 'scenario',
        'exampleMessage', 'creatorNotes', 'tags', 'creator', 'characterVersion', 'nickname', 'source', 'license'],
    prompt: ['systemPrompt', 'postHistoryInstructions', 'replaceGlobalNote', 'defaultVariables', 'additionalText', 'depth_prompt', 'cjs'],
    lore: ['globalLore', 'lorebook', 'loreSettings', 'loreExt', 'lorePlus'],
    regex: ['customscript', 'regex'],
    trigger: ['triggerscript', 'trigger'],
    asset: ['image', 'icon', 'emotionImages', 'additionalAssets', 'ccAssets', 'assets', 'vits'],
}
const fieldGroup = new Map<string, UpdateGroupId>()
for (const [group, fields] of Object.entries(groupFields)) for (const field of fields) fieldGroup.set(field, group as UpdateGroupId)

export function updateGroupOf(item: Pick<UpdateItem, 'field'>): UpdateGroupId {
    return fieldGroup.get(item.field) ?? 'other'
}

/** Folder id to folder title, from lore entries of every side. */
export function loreFolderNames(...lists: unknown[]): Map<string, string> {
    const names = new Map<string, string>()
    for (const list of lists) {
        for (const entry of Array.isArray(list) ? list : []) {
            if (entry?.mode === 'folder' && entry.key && !names.has(entry.key)) names.set(entry.key, String(entry.comment || entry.key))
        }
    }
    return names
}

/**
 * The second level inside a group: the lorebook folder an entry is in, or
 * the kind of asset. Empty when the group has no second level.
 */
export function updateSubgroupOf(item: UpdateItem, folders: Map<string, string>, labels: Record<string, string>): { id: string, label: string } {
    const group = updateGroupOf(item)
    if (group === 'asset') return { id: item.field, label: labels[item.field] ?? item.field }
    if (group !== 'lore') return { id: '', label: '' }
    if (!item.key) return { id: 'settings', label: '' }
    const entry = (item.mine ?? item.theirs) as { mode?: string, key?: string, folder?: string } | undefined
    const folder = entry?.mode === 'folder' ? entry.key : entry?.folder
    if (folder) return { id: `folder:${folder}`, label: folders.get(folder) ?? folder }
    return { id: 'root', label: '' }
}

/**
 * Lowercased text an item is searched by: its field, name and the text of
 * both sides. Internal ids and flags are left out so they do not match.
 */
export function updateSearchText(item: UpdateItem, label: string): string {
    const editable = isEditable(item)
    const sides = editable
        ? [editableText(item, 'mine'), editableText(item, 'theirs')]
        : [updateValueText(item.mine), updateValueText(item.theirs)]
    // The keys of a lore entry and the pattern of a regex, besides the text an editable item shows.
    const keys = editable
        ? [item.mine, item.theirs].flatMap((side) => isRecord(side)
            ? ['key', 'secondkey', 'in'].map((key) => (side as Record<string, unknown>)[key]).filter((text) => typeof text === 'string')
            : [])
        : []
    return [label, updateItemName(item), ...keys, ...sides].join('\n').toLowerCase()
}

export interface DiffLine {
    type: 'same' | 'add' | 'del' | 'skip'
    text: string
}

const MAX_CELLS = 2_000_000

/**
 * Line diff with common prefix/suffix trimming and an LCS core. Very large
 * middles fall back to "all removed, then all added". Unchanged runs longer
 * than `context * 2` collapse into a skip marker holding the line count.
 */
export function lineDiff(before: string, after: string, context = 2): DiffLine[] {
    const a = before.split('\n')
    const b = after.split('\n')
    let start = 0
    while (start < a.length && start < b.length && a[start] === b[start]) start++
    let endA = a.length
    let endB = b.length
    while (endA > start && endB > start && a[endA - 1] === b[endB - 1]) { endA--; endB-- }
    const midA = a.slice(start, endA)
    const midB = b.slice(start, endB)
    const middle: DiffLine[] = []
    if (midA.length * midB.length > MAX_CELLS) {
        for (const text of midA) middle.push({ type: 'del', text })
        for (const text of midB) middle.push({ type: 'add', text })
    } else {
        const cols = midB.length + 1
        const table = new Uint32Array((midA.length + 1) * cols)
        for (let i = midA.length - 1; i >= 0; i--) {
            for (let j = midB.length - 1; j >= 0; j--) {
                table[i * cols + j] = midA[i] === midB[j]
                    ? table[(i + 1) * cols + j + 1] + 1
                    : Math.max(table[(i + 1) * cols + j], table[i * cols + j + 1])
            }
        }
        let i = 0
        let j = 0
        while (i < midA.length && j < midB.length) {
            if (midA[i] === midB[j]) { middle.push({ type: 'same', text: midA[i] }); i++; j++ }
            else if (table[(i + 1) * cols + j] >= table[i * cols + j + 1]) middle.push({ type: 'del', text: midA[i++] })
            else middle.push({ type: 'add', text: midB[j++] })
        }
        while (i < midA.length) middle.push({ type: 'del', text: midA[i++] })
        while (j < midB.length) middle.push({ type: 'add', text: midB[j++] })
    }
    const lines: DiffLine[] = [
        ...a.slice(0, start).map(text => ({ type: 'same' as const, text })),
        ...middle,
        ...a.slice(endA).map(text => ({ type: 'same' as const, text })),
    ]
    const changed = lines.map(line => line.type !== 'same')
    const out: DiffLine[] = []
    let skipped = 0
    lines.forEach((line, index) => {
        let keep = false
        for (let k = Math.max(0, index - context); k <= Math.min(lines.length - 1, index + context); k++) {
            if (changed[k]) { keep = true; break }
        }
        if (keep) {
            if (skipped) out.push({ type: 'skip', text: String(skipped) })
            skipped = 0
            out.push(line)
        } else skipped++
    })
    if (skipped) out.push({ type: 'skip', text: String(skipped) })
    return out
}

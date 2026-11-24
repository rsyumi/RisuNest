import type { PromptItem } from './prompt'

export type PromptSearchField = 'name' | 'text' | 'innerFormat' | 'defaultText'

export interface PromptSearchOptions {
    matchCase: boolean
    regex: boolean
}

export type PromptSearchMatcher =
    | { kind: 'empty' }
    | { kind: 'invalid' }
    | { kind: 'ready', pattern: RegExp, literal: boolean }

export interface TextRange {
    start: number
    end: number
}

export interface PromptFieldMatch extends TextRange {
    field: PromptSearchField
}

/** One field a replace wrote: the text before and after, and where the written text sits in `after`. */
export interface PromptFieldChange {
    block: PromptItem
    field: PromptSearchField
    before: string
    after: string
    written: TextRange[]
}

export interface PromptFieldWrite {
    block: PromptItem
    field: PromptSearchField
    value: string
}

/** Fields a block of this type shows. A field left over from an earlier type is not one of them. */
function typeFields(type: PromptItem['type']): PromptSearchField[] {
    switch (type) {
        case 'plain':
        case 'jailbreak':
        case 'cot':
        case 'chatML':
            return ['name', 'text']
        case 'persona':
        case 'description':
        case 'memory':
            return ['name', 'innerFormat']
        case 'authornote':
            return ['name', 'innerFormat', 'defaultText']
        default:
            return ['name']
    }
}

/** The searched fields of a block that hold text, in the order the opened block shows them. */
export function searchFields(block: PromptItem): PromptSearchField[] {
    return typeFields(block.type).filter(field => typeof (block as unknown as Record<string, unknown>)[field] === 'string')
}

export function fieldText(block: PromptItem, field: PromptSearchField): string | undefined {
    if (!searchFields(block).includes(field)) return undefined
    return (block as unknown as Record<string, string>)[field]
}

const escapeRegExp = (text: string) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')

export function buildMatcher(query: string, options: PromptSearchOptions): PromptSearchMatcher {
    if (!query) return { kind: 'empty' }
    if (!options.regex) {
        // An escaped pattern keeps positions in the stored text, which a lower-cased copy may not.
        return { kind: 'ready', pattern: new RegExp(escapeRegExp(query), options.matchCase ? 'g' : 'gi'), literal: true }
    }
    try {
        return { kind: 'ready', pattern: new RegExp(query, options.matchCase ? 'gm' : 'gmi'), literal: false }
    } catch {
        return { kind: 'invalid' }
    }
}

function* nonEmptyMatches(text: string, pattern: RegExp) {
    for (const match of text.matchAll(pattern)) {
        if (match[0].length > 0) yield match
    }
}

export function findRanges(text: string, pattern: RegExp): TextRange[] {
    return [...nonEmptyMatches(text, pattern)].map(match => ({ start: match.index!, end: match.index! + match[0].length }))
}

export function blockMatches(block: PromptItem, pattern: RegExp): PromptFieldMatch[] {
    return searchFields(block).flatMap(field => findRanges(fieldText(block, field)!, pattern).map(range => ({ field, ...range })))
}

/** The replacement text for one match, with the `$` patterns of `String.prototype.replace`. */
function expand(template: string, match: RegExpMatchArray, text: string): string {
    const matched = match[0]
    const position = match.index!
    const captures = match.length - 1
    let result = ''
    let index = 0
    while (index < template.length) {
        const dollar = template.indexOf('$', index)
        if (dollar === -1 || dollar === template.length - 1) {
            result += template.slice(index)
            break
        }
        result += template.slice(index, dollar)
        const next = template[dollar + 1]
        if (next === '$') {
            result += '$'
            index = dollar + 2
        } else if (next === '&') {
            result += matched
            index = dollar + 2
        } else if (next === '`') {
            result += text.slice(0, position)
            index = dollar + 2
        } else if (next === "'") {
            result += text.slice(Math.min(position + matched.length, text.length))
            index = dollar + 2
        } else if (next >= '0' && next <= '9') {
            const second = template[dollar + 2]
            let digits = second !== undefined && second >= '0' && second <= '9' ? 2 : 1
            let group = Number(template.slice(dollar + 1, dollar + 1 + digits))
            if (group > captures && digits === 2) {
                digits = 1
                group = Number(next)
            }
            result += group >= 1 && group <= captures ? match[group] ?? '' : template.slice(dollar, dollar + 1 + digits)
            index = dollar + 1 + digits
        } else if (next === '<') {
            const close = template.indexOf('>', dollar + 2)
            if (close === -1 || match.groups === undefined) {
                result += '$<'
                index = dollar + 2
            } else {
                result += match.groups[template.slice(dollar + 2, close)] ?? ''
                index = close + 1
            }
        } else {
            result += '$'
            index = dollar + 1
        }
    }
    return result
}

const replacementFor = (matcher: Extract<PromptSearchMatcher, { kind: 'ready' }>, replacement: string, match: RegExpMatchArray, text: string) =>
    matcher.literal ? replacement : expand(replacement, match, text)

function replaceInText(text: string, matcher: Extract<PromptSearchMatcher, { kind: 'ready' }>, replacement: string) {
    let after = ''
    let last = 0
    const written: TextRange[] = []
    for (const match of nonEmptyMatches(text, matcher.pattern)) {
        after += text.slice(last, match.index)
        const inserted = replacementFor(matcher, replacement, match, text)
        // A match replaced by the same text is left as it was, so it is not one that was written.
        if (inserted !== match[0]) written.push({ start: after.length, end: after.length + inserted.length })
        after += inserted
        last = match.index! + match[0].length
    }
    return { after: after + text.slice(last), written }
}

/** Every match in every searched field. Only fields whose text changes are returned. */
export function replaceAllInBlocks(blocks: readonly PromptItem[], matcher: PromptSearchMatcher, replacement: string): PromptFieldChange[] {
    if (matcher.kind !== 'ready') return []
    const changes: PromptFieldChange[] = []
    for (const block of blocks) {
        for (const field of searchFields(block)) {
            const before = fieldText(block, field)!
            const { after, written } = replaceInText(before, matcher, replacement)
            if (after !== before) changes.push({ block, field, before, after, written })
        }
    }
    return changes
}

/**
 * The match at `range` alone, computed against the whole field so lookarounds and groups give
 * what replacing every match would give there. Null when that match is no longer in the field.
 */
export function replaceOneInBlock(
    block: PromptItem, field: PromptSearchField, range: TextRange, matcher: PromptSearchMatcher, replacement: string,
): PromptFieldChange | null {
    if (matcher.kind !== 'ready') return null
    const before = fieldText(block, field)
    if (before === undefined) return null
    for (const match of nonEmptyMatches(before, matcher.pattern)) {
        if (match.index! > range.start) break
        if (match.index !== range.start || match.index + match[0].length !== range.end) continue
        const inserted = replacementFor(matcher, replacement, match, before)
        const after = before.slice(0, range.start) + inserted + before.slice(range.end)
        if (after === before) return null
        return { block, field, before, after, written: [{ start: range.start, end: range.start + inserted.length }] }
    }
    return null
}

/** What undoing a replace writes: each field still holding the replaced text, on a block still in the template. */
export function undoWrites(changes: readonly PromptFieldChange[], blocks: readonly PromptItem[]): PromptFieldWrite[] {
    const present = new Set(blocks)
    return changes
        .filter(change => present.has(change.block) && fieldText(change.block, change.field) === change.after)
        .map(change => ({ block: change.block, field: change.field, value: change.before }))
}

const CONTEXT_BEFORE = 24
const CONTEXT_AFTER = 200

/** The line that holds the start of `range`, split around the part of the range on that line. */
export function matchLine(text: string, range: TextRange): { before: string, hit: string, after: string } {
    const lineStart = range.start === 0 ? 0 : text.lastIndexOf('\n', range.start - 1) + 1
    let lineEnd = text.indexOf('\n', range.start)
    if (lineEnd === -1) lineEnd = text.length
    if (lineEnd > range.start && text[lineEnd - 1] === '\r') lineEnd--
    const hitEnd = Math.min(range.end, lineEnd)
    let before = text.slice(lineStart, range.start)
    if (before.length > CONTEXT_BEFORE) {
        let cut = before.length - CONTEXT_BEFORE
        const code = before.charCodeAt(cut)
        if (code >= 0xdc00 && code <= 0xdfff) cut++
        before = '…' + before.slice(cut)
    }
    const hitOnLine = text.slice(range.start, hitEnd)
    const hit = hitOnLine.slice(0, CONTEXT_AFTER)
    const after = hit.length < hitOnLine.length ? '' : text.slice(hitEnd, Math.min(lineEnd, hitEnd + CONTEXT_AFTER))
    return { before, hit, after }
}

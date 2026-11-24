import { describe, expect, it } from 'vitest'
import type { PromptItem } from './prompt'
import {
    blockMatches, buildMatcher, findRanges, matchLine, replaceAllInBlocks, replaceOneInBlock, searchFields, undoWrites,
    type PromptSearchMatcher,
} from './promptSearch'

const ready = (query: string, options = { matchCase: false, regex: false }) => {
    const matcher = buildMatcher(query, options)
    if (matcher.kind !== 'ready') throw new Error(`not ready: ${matcher.kind}`)
    return matcher
}
const regex = (query: string, matchCase = false) => ready(query, { matchCase, regex: true })
const texts = (text: string, matcher: Extract<PromptSearchMatcher, { kind: 'ready' }>) =>
    findRanges(text, matcher.pattern).map(range => text.slice(range.start, range.end))

describe('fields', () => {
    it('searches the fields of the current type only, skipping fields without text', () => {
        const blocks = [
            { type: 'plain', type2: 'normal', role: 'system', name: 'n', text: 't' },
            { type: 'chatML', text: 't' },
            { type: 'persona', name: 'n', innerFormat: 'f' },
            { type: 'description', innerFormat: null },
            { type: 'authornote', name: 'n', innerFormat: 'f', defaultText: 'd' },
            { type: 'authornote', defaultText: 'd' },
            { type: 'chat', name: 'n', rangeStart: -1000, rangeEnd: 'end' },
            { type: 'cache', name: '', depth: 1, role: 'all' },
        ] as unknown as PromptItem[]
        expect(blocks.map(searchFields)).toEqual([
            ['name', 'text'], ['text'], ['name', 'innerFormat'], [], ['name', 'innerFormat', 'defaultText'],
            ['defaultText'], ['name'], ['name'],
        ])
    })

    it('leaves a field of an earlier type unread and unwritten', () => {
        // A block switched from plain to lorebook keeps its text on the object.
        const block = { type: 'lorebook', name: 'Lore', text: 'Lore in leftover text' } as unknown as PromptItem
        const matcher = ready('lore')
        expect(blockMatches(block, matcher.pattern)).toEqual([{ field: 'name', start: 0, end: 4 }])
        const changes = replaceAllInBlocks([block], matcher, 'X')
        expect(changes.map(change => [change.field, change.after])).toEqual([['name', 'X']])
        expect(replaceOneInBlock(block, 'text', { start: 0, end: 4 }, matcher, 'X')).toBeNull()
    })
})

describe('matching', () => {
    it('finds plain text with regular expression characters literally', () => {
        expect(texts('a.b (x) a+b [y] $1 a.b', ready('a.b'))).toEqual(['a.b', 'a.b'])
        expect(texts('a.b (x) a+b [y] $1', ready('(x) a+b [y] $1'))).toEqual(['(x) a+b [y] $1'])
        expect(texts('axb', ready('a.b'))).toEqual([])
    })

    it('ignores case unless asked, with positions in the stored text', () => {
        expect(texts('Abc ABC abc', ready('abc'))).toEqual(['Abc', 'ABC', 'abc'])
        expect(texts('Abc ABC abc', ready('abc', { matchCase: true, regex: false }))).toEqual(['abc'])
        expect(texts('Abc ABC abc', regex('a.c', true))).toEqual(['abc'])
        // Lower-casing İ makes it longer, which would move every later position.
        const text = 'İİ then Name'
        expect(findRanges(text, ready('name').pattern)).toEqual([{ start: 8, end: 12 }])
    })

    it('matches line starts and ends in regular expression mode and ignores zero-length matches', () => {
        expect(texts('one\ntwo', regex('^\\w+$'))).toEqual(['one', 'two'])
        expect(texts('a.\nb', regex('.+'))).toEqual(['a.', 'b'])
        expect(findRanges('aa b aaa', regex('a*').pattern)).toEqual([{ start: 0, end: 2 }, { start: 5, end: 8 }])
        expect(findRanges('text', regex('^').pattern)).toEqual([])
    })

    it('reports an invalid pattern instead of throwing', () => {
        expect(buildMatcher('(unclosed', { matchCase: false, regex: true })).toEqual({ kind: 'invalid' })
        expect(buildMatcher('(unclosed', { matchCase: false, regex: false }).kind).toBe('ready')
        expect(buildMatcher('', { matchCase: false, regex: true })).toEqual({ kind: 'empty' })
    })

    it('shows the line that holds a match, cutting long text before it', () => {
        expect(matchLine('first\nsecond line\nthird', { start: 13, end: 17 })).toEqual({ before: 'second ', hit: 'line', after: '', cut: false })
        // A match across lines shows its first line.
        expect(matchLine('ab\r\ncd', { start: 1, end: 5 })).toEqual({ before: 'a', hit: 'b', after: '', cut: false })
        expect(matchLine('\nx', { start: 0, end: 2 })).toEqual({ before: '', hit: '', after: '', cut: false })
        expect(matchLine(`${'w'.repeat(24)}hit`, { start: 24, end: 27 }).cut).toBe(false)
        const long = `${'w'.repeat(40)}hit after`
        expect(matchLine(long, { start: 40, end: 43 })).toEqual({ before: 'w'.repeat(24), hit: 'hit', after: ' after', cut: true })
    })
})

describe('replacing', () => {
    const block = (text: string) => ({ type: 'plain', type2: 'normal', role: 'system', text }) as PromptItem

    it('writes a plain replacement as literal text', () => {
        const [change] = replaceAllInBlocks([block('cost $5 and $5')], ready('$5'), '$& or $1 $$')
        expect(change.after).toBe('cost $& or $1 $$ and $& or $1 $$')
        expect(change.written.map(range => change.after.slice(range.start, range.end))).toEqual(['$& or $1 $$', '$& or $1 $$'])
    })

    it('expands groups in a regular expression replacement as String.prototype.replace does', () => {
        const [change] = replaceAllInBlocks([block('Kim Minji, Lee Jiwoo')], regex('(\\w+) (?<given>\\w+)'), '$<given> $1')
        expect(change.after).toBe('Minji Kim, Jiwoo Lee')
        expect(change.written).toEqual([{ start: 0, end: 9 }, { start: 11, end: 20 }])
        const text = 'ab-cd-ef'
        for (const pattern of ['(\\w)(?<n>\\w)', '(\\w)(\\w)']) {
            for (const template of ['$1$2', '$&!', '$`|', "$'|", '$$', '$0', '$01', '$10', '$3', '$<n>', '$<x', 'end$', '$<missing>']) {
                const [replaced] = replaceAllInBlocks([block(text)], regex(pattern), template)
                expect(replaced?.after ?? text, `${pattern} ${template}`).toBe(text.replace(new RegExp(pattern, 'gmi'), template))
            }
        }
    })

    it('replaces one match against the whole field, as replacing all would there', () => {
        const matcher = regex('(?<=@)(\\w+)(?= )')
        const text = 'x@one y@two z@three end'
        const all = replaceAllInBlocks([block(text)], matcher, '[$1]')[0].after
        const ranges = findRanges(text, matcher.pattern)
        expect(ranges.map(range => text.slice(range.start, range.end))).toEqual(['one', 'two', 'three'])
        const one = replaceOneInBlock(block(text), 'text', ranges[1], matcher, '[$1]')!
        expect(one.after).toBe('x@one y@[two] z@three end')
        expect(all).toBe('x@[one] y@[two] z@[three] end')
        expect(one.written).toEqual([{ start: 8, end: 13 }])
        // A match that is no longer at that place is not replaced.
        expect(replaceOneInBlock(block(text), 'text', { start: 3, end: 5 }, matcher, '[$1]')).toBeNull()
    })

    it('deletes matches with an empty replacement and returns only fields that change', () => {
        const blocks = [block('keep'), { type: 'lorebook', name: 'drop me' } as PromptItem]
        expect(replaceAllInBlocks(blocks, ready(' me'), '').map(change => change.after)).toEqual(['drop'])
        expect(replaceAllInBlocks(blocks, ready('keep'), 'keep')).toEqual([])
        expect(replaceAllInBlocks([block('Foo FOO')], regex('foo'), '$&')).toEqual([])
    })

    it('records only the matches whose text changes, however they are spread over fields', () => {
        const written = (blocks: PromptItem[], matcher: Extract<PromptSearchMatcher, { kind: 'ready' }>, replacement: string) =>
            replaceAllInBlocks(blocks, matcher, replacement).flatMap(change => change.written.map(range => change.after.slice(range.start, range.end) + '@' + range.start))
        expect(written([block('foo Foo')], ready('foo'), 'Foo')).toEqual(['Foo@0'])
        expect(written([block('Foo foo')], ready('foo'), 'Foo')).toEqual(['Foo@4'])
        expect(written([block('foo'), block('Foo')], ready('foo'), 'Foo')).toEqual(['Foo@0'])
        // Positions after an unchanged match follow the text written before it.
        expect(written([block('a aa aaa')], regex('a+'), 'aa')).toEqual(['aa@0', 'aa@6'])
    })
})

describe('undo', () => {
    it('restores fields that still hold the replaced text, following blocks by identity', () => {
        const moved = { type: 'plain', type2: 'normal', role: 'system', name: 'cat one', text: 'cat two' } as PromptItem
        const edited = { type: 'chatML', text: 'cat three' } as PromptItem
        const removed = { type: 'lorebook', name: 'cat four' } as PromptItem
        const retyped = { type: 'chatML', text: 'cat five' } as PromptItem
        const template = [moved, edited, removed, retyped]
        const changes = replaceAllInBlocks(template, ready('cat'), 'dog')
        for (const change of changes) (change.block as unknown as Record<string, string>)[change.field] = change.after
        ;(edited as { text: string }).text = 'dog three, edited'
        ;(retyped as { type: string }).type = 'lorebook'
        const after = [edited, retyped, moved]
        const writes = undoWrites(changes, after)
        expect(writes.map(write => [write.block, write.field, write.value])).toEqual([
            [moved, 'name', 'cat one'], [moved, 'text', 'cat two'],
        ])
    })
})

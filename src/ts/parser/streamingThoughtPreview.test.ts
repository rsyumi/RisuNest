import { describe, expect, it } from 'vitest'
import { getStreamingThoughtPreview } from './streamingThoughtPreview'

describe('streaming Thought preview', () => {
    it('extracts full nested and multiple thoughts only when requested for an expanded block', () => {
        const source =
            'A<Thoughts>old</Thoughts>B<Thoughts>new<Thoughts> inner</Thoughts> tail</Thoughts>C'
        expect(getStreamingThoughtPreview(source)?.full).toBeUndefined()
        expect(getStreamingThoughtPreview(source, true)).toMatchObject({
            full: 'old\n\nnew inner tail',
            recent: 'new inner tail',
            before: 'A',
            after: 'BC',
        })
    })
    it('keeps only the latest four lines without losing surrounding answer text', () => {
        expect(
            getStreamingThoughtPreview(
                'Before\n<Thoughts>one\ntwo\nthree\nfour\nfive</Thoughts>\nAnswer',
            ),
        ).toEqual({
            before: 'Before\n',
            after: '\nAnswer',
            recent: 'two\nthree\nfour\nfive',
            truncated: true,
        })
    })

    it('bounds even a single huge unfinished paragraph', () => {
        const input = '<Thoughts>' + 'old '.repeat(125_000) + 'LATEST'
        const preview = getStreamingThoughtPreview(input)!
        expect(preview.recent.length).toBeLessThanOrEqual(800)
        expect(preview.recent.endsWith('LATEST')).toBe(true)
        expect(preview.truncated).toBe(true)
        expect(preview.before + preview.after).toBe('')
        expect(input.length).toBeGreaterThan(500_000)
    })

    it('handles nested and multiple blocks using the latest thought only', () => {
        expect(
            getStreamingThoughtPreview(
                'A<Thoughts>old</Thoughts>B<Thoughts>new<Thoughts> inner</Thoughts> tail</Thoughts>C',
            ),
        ).toEqual({
            before: 'A',
            after: 'BC',
            recent: 'new inner tail',
            truncated: false,
        })
    })

    it('withholds incomplete delimiters inside an active block', () => {
        const input = '<Thoughts>Latest</Thoughts>Answer'
        for (
            let end = '<Thoughts>'.length;
            end < input.indexOf('Answer');
            end++
        ) {
            const preview = getStreamingThoughtPreview(input.slice(0, end))!
            expect(preview.recent).not.toContain('<')
            expect(preview.after).toBe('')
        }
        expect(getStreamingThoughtPreview('<Thoughts>x<Thought')).toMatchObject(
            { recent: 'x' },
        )
        expect(getStreamingThoughtPreview('<Thoughts>x<Thud')).toMatchObject({
            recent: 'x<Thud',
        })
    })

    it('accepts provider snapshots which insert before an existing closing tag', () => {
        expect(
            getStreamingThoughtPreview('<Thoughts>A</Thoughts>'),
        ).toMatchObject({ recent: 'A' })
        expect(
            getStreamingThoughtPreview('<Thoughts>AB</Thoughts>Final'),
        ).toMatchObject({ recent: 'AB', after: 'Final' })
        expect(
            getStreamingThoughtPreview('<Thoughts>replacement</Thoughts>'),
        ).toMatchObject({ recent: 'replacement' })
    })

    it('does not guess other grammars or alter ordinary text', () => {
        for (const text of [
            'Answer',
            '<think>x</think>',
            '<Thought>x</Thought>',
            '<Thou',
            '</Thoughts>',
        ]) {
            expect(getStreamingThoughtPreview(text)).toBeNull()
        }
        expect(
            getStreamingThoughtPreview(
                '<Thoughts><img src=x onerror=alert(1)></Thoughts>',
            ),
        ).toMatchObject({ recent: '<img src=x onerror=alert(1)>' })
    })

    it('handles CRLF and does not start a truncated preview with a broken surrogate', () => {
        expect(
            getStreamingThoughtPreview(
                '<Thoughts>1\r\n2\r\n3\r\n4\r\n5\r\n</Thoughts>',
            )?.recent,
        ).toBe('2\r\n3\r\n4\r\n5')
        const preview = getStreamingThoughtPreview(
            '<Thoughts>' + '😀'.repeat(500) + 'z',
        )!
        const first = preview.recent.charCodeAt(0)
        expect(first >= 0xdc00 && first <= 0xdfff).toBe(false)
        expect(preview.recent.endsWith('z')).toBe(true)
    })
})

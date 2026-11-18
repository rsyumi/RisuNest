import { describe, expect, it } from 'vitest'
import { fitFolderIconText, graphemes } from './folderIconFit'

// One em per grapheme, a narrow space and Latin letters, so sizes follow from the counts alone.
const measure = (line: string) => graphemes(line).reduce((width, unit) => width + (unit === ' ' ? 0.3 : /[A-Za-z]/.test(unit) ? 0.6 : 1), 0)
const fit = (text: string, round = false) => fitFolderIconText(text, round, measure)

describe('fitFolderIconText', () => {
    it('keeps short text on one line and never splits fewer than two syllables onto a line', () => {
        expect(fit('판타')).toEqual({ lines: ['판타'], size: 23, clipped: false, maxWidths: [46] })
        expect(fit('판타지').lines).toEqual(['판타지'])
    })

    it('breaks a long Hangul word between syllables and between words when there are words', () => {
        const syllables = fit('가나다라마바사아')
        expect(syllables.lines).toEqual(['가나다', '라마바', '사아'])
        expect(syllables.clipped).toBe(false)
        expect(fit('가나다라마바사').lines).toEqual(['가나다', '라마', '바사'])
        expect(fit('현대 판타지 로맨스').lines).toEqual(['현대', '판타지', '로맨스'])
    })

    it('breaks words into syllables only when that is clearly larger than breaking between words', () => {
        expect(fit('가나다 라마바사').lines).toEqual(['가나다', '라마바사'])
    })

    it('keeps the user line breaks and blank lines between them, joining any beyond the third line', () => {
        expect(fit('로맨스\n판타지').lines).toEqual(['로맨스', '판타지'])
        expect(fit('일\n이\n삼\n사').lines).toEqual(['일', '이', '삼 사'])
        expect(fit('\n로맨스\n\n판타지\n').lines).toEqual(['로맨스', '', '판타지'])
    })

    it('never breaks a Latin word or number and cuts what does not fit at the floor size', () => {
        expect(fit('Fantasyland')).toMatchObject({ lines: ['Fantasyland'], size: 11, clipped: true })
        expect(fit('Slice of life').lines).toEqual(['Slice', 'of', 'life'])
        // Hangul next to them still breaks between syllables, and the spaces beside them stay.
        expect(fit('현대판타지로맨스2').lines).toEqual(['현대판', '타지로', '맨스2'])
        expect(fit('가나다라마바12345').lines).toEqual(['가나다', '라마', '바12345'])
        expect(fit('AI RP 가나다라마바사아').lines).toEqual(['AI RP 가나', '다라마', '바사아'])
        expect(fit('시즌1.25').lines).toEqual(['시즌1.25'])
        expect(fit('시즌1,234').lines).toEqual(['시즌1,234'])
    })

    it('counts graphemes, so an emoji sequence is never split', () => {
        const family = '👨‍👩‍👧‍👦'
        expect(fit(family.repeat(4)).lines).toEqual([family.repeat(2), family.repeat(2)])
    })

    it('fits a round tile inside its circle', () => {
        const round = fit('판타', true)
        expect(round.size).toBeLessThan(fit('판타').size)
        // A wide middle line uses the room the outer lines leave at the top and bottom of the circle.
        expect(fit('가 가나다라 가', true)).toMatchObject({ lines: ['가', '가나다라', '가'], size: 11.5, clipped: false })
        const long = fit('가나다라마바사아자차카타파하가나다라마바사아자차카타', true)
        expect(long.clipped).toBe(true)
        const [first, middle, last] = long.maxWidths
        expect(first).toBe(last)
        expect(first).toBeLessThan(middle)
        expect(middle).toBeLessThan(48)
    })
})

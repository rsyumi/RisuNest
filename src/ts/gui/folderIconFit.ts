/** Fits folder icon text into the 56px sidebar tile: one to three lines at the largest readable size. */

export const folderIconLineHeight = 1.12
const maxFontPx = 26
const minFontPx = 11
/** Usable radius of a round tile and usable side of a square one, inside the 56px fill. */
const roundRadius = 24
const squareSide = 46
/** More lines must beat fewer lines by this much. */
const extraLineGain = 1.06
/** Breaking words into syllables must beat the best word layout by this much. */
const syllablePenalty = 0.85
const maxEnumeratedWords = 12
const maxLines = 3

/** Width of a line in em, measured in the tile's bold font. */
export type MeasureLine = (line: string) => number

export interface FolderIconFit {
    lines: string[]
    /** Font size in px. */
    size: number
    /** True when the text is shown at the floor size and cut with an ellipsis. */
    clipped: boolean
    /** Per line, the widest text in px that stays inside the tile at this size. */
    maxWidths: number[]
}

type Layout = { lines: string[], syllable: boolean }

const segmenter = typeof Intl.Segmenter === 'function' ? new Intl.Segmenter(undefined, { granularity: 'grapheme' }) : null

export function graphemes(text: string): string[] {
    return segmenter ? Array.from(segmenter.segment(text), (part) => part.segment) : Array.from(text)
}

function wordLayouts(words: string[], count: number, measure: MeasureLine): string[][] {
    if (words.length < count) return []
    if (words.length <= maxEnumeratedWords) {
        const result: string[][] = []
        const walk = (start: number, left: number, taken: string[]) => {
            if (left === 1) { result.push([...taken, words.slice(start).join(' ')]); return }
            for (let end = start + 1; end <= words.length - left + 1; end++) walk(end, left - 1, [...taken, words.slice(start, end).join(' ')])
        }
        walk(0, count, [])
        return result
    }
    // Too many words to try every split: fill each line up to an equal share of the width.
    const target = measure(words.join(' ')) / count
    const lines: string[] = []
    let current: string[] = []
    for (const [index, word] of words.entries()) {
        const remainingLines = count - lines.length
        const remainingWords = words.length - index
        if (current.length && remainingLines > 1 && (measure([...current, word].join(' ')) > target || remainingWords < remainingLines)) {
            lines.push(current.join(' '))
            current = []
        }
        current.push(word)
    }
    lines.push(current.join(' '))
    return lines.length === count ? [lines] : []
}

const unbreakable = /[\p{Script=Latin}\p{Nd}]/u
const closing = /^[\p{Pe}\p{Pf}\p{Po}]$/u

function syllableLayout(words: string[], count: number): Layout | null {
    // Other characters may break between any two and drop the spaces between them. Closing punctuation stays with what
    // precedes it, and Latin words and numbers (1.25 and 1,234 included) never break and keep the spaces beside them.
    const units: string[] = []
    let spaced = false
    for (const unit of graphemes(words.join(' '))) {
        if (unit === ' ') { spaced = true; continue }
        const last = units.at(-1) ?? ''
        if (last && (closing.test(unit) || (unbreakable.test(unit) && unbreakable.test(last)))) units[units.length - 1] = last + (spaced ? ' ' : '') + unit
        else units.push(spaced && (unbreakable.test(unit) || unbreakable.test(last)) ? ` ${unit}` : unit)
        spaced = false
    }
    if (units.length < 2 * count) return null
    // Even lines, longer ones first, so every line keeps at least two.
    const lines: string[] = []
    let start = 0
    for (let line = 0; line < count; line++) {
        const length = Math.floor(units.length / count) + (line < units.length % count ? 1 : 0)
        lines.push(units.slice(start, start + length).join('').trim())
        start += length
    }
    return { lines, syllable: words.length > 1 }
}

function layouts(text: string, measure: MeasureLine): Layout[] {
    // Blank lines between lines keep their place.
    const given = text.trim().split('\n').map((line) => line.trim())
    if (given.length > 1) {
        const lines = given.length > maxLines ? [...given.slice(0, maxLines - 1), given.slice(maxLines - 1).filter(Boolean).join(' ')] : given
        return [{ lines, syllable: false }]
    }
    const single = given[0] ?? ''
    const words = single.split(/\s+/).filter(Boolean)
    const result: Layout[] = [{ lines: [single], syllable: false }]
    for (let count = 2; count <= maxLines; count++) {
        for (const lines of wordLayouts(words, count, measure)) result.push({ lines, syllable: false })
        const syllable = syllableLayout(words, count)
        if (syllable) result.push(syllable)
    }
    return result
}

/** Distance in em from the tile centre to the edge of a line farther from it. */
function lineEdge(index: number, count: number): number {
    return Math.max(Math.abs(index - count / 2), Math.abs(index + 1 - count / 2)) * folderIconLineHeight
}

function sizeFor(widths: number[], round: boolean): number {
    const width = Math.max(...widths)
    if (width <= 0) return Infinity
    if (!round) return Math.min(squareSide / width, squareSide / (widths.length * folderIconLineHeight))
    // In a circle each line may be as wide as the chord at its outer edge.
    return Math.min(...widths.map((line, index) => (2 * roundRadius) / Math.hypot(line, 2 * lineEdge(index, widths.length))))
}

export function fitFolderIconText(text: string, round: boolean, measure: MeasureLine): FolderIconFit {
    let best: { score: number, size: number, lines: string[] } = { score: 0, size: maxFontPx, lines: [''] }
    for (const layout of layouts(text, measure)) {
        const size = sizeFor(layout.lines.map(measure), round)
        const score = size * (layout.syllable ? syllablePenalty : 1)
        if (score > (layout.lines.length > best.lines.length ? best.score * extraLineGain : best.score)) best = { score, size, lines: layout.lines }
    }
    const fitted = Math.min(maxFontPx, Math.floor(best.size * 2) / 2)
    const clipped = fitted < minFontPx
    const size = Math.max(minFontPx, fitted)
    const maxWidths = best.lines.map((_, index) => {
        if (!round) return squareSide
        const edge = lineEdge(index, best.lines.length) * size
        return Math.floor(20 * Math.sqrt(Math.max(0, roundRadius ** 2 - edge ** 2))) / 10
    })
    return { lines: best.lines, size, clipped, maxWidths }
}

/** Approximate em widths for engines without canvas text metrics. */
export function estimateLineWidth(line: string): number {
    let width = 0
    for (const unit of graphemes(line)) width += /[ᄀ-ᇿ　-鿿가-힯豈-﫿＀-￯]|\p{Extended_Pictographic}/u.test(unit) ? 1 : unit === ' ' ? 0.28 : 0.6
    return width
}

let measureCanvas: HTMLCanvasElement | undefined

function measureContext(): CanvasRenderingContext2D | null {
    try {
        if (!measureCanvas) {
            measureCanvas = document.createElement('canvas')
            measureCanvas.hidden = true
            measureCanvas.setAttribute('aria-hidden', 'true')
        }
        // A canvas outside the document can fall back to another font for Hangul than the page uses.
        if (!measureCanvas.isConnected) document.body.append(measureCanvas)
        return measureCanvas.getContext('2d')
    } catch {
        return null
    }
}

/** Measures lines in the tile's bold font, or estimates them where canvas text metrics are unavailable. */
export function canvasLineMeasure(font: string): MeasureLine {
    const context = font ? measureContext() : null
    if (!context) return estimateLineWidth
    return (line) => {
        context.font = `700 100px ${font}`
        return context.measureText(line).width / 100
    }
}

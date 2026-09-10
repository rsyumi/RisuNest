const openTag = '<Thoughts>'
const closeTag = '</Thoughts>'
const maxPreviewLength = 800
const previewLines = 4
const segmenter =
    typeof Intl.Segmenter === 'function'
        ? new Intl.Segmenter(undefined, { granularity: 'grapheme' })
        : undefined

export interface StreamingThoughtPreview {
    readonly before: string
    readonly after: string
    readonly recent: string
    readonly truncated: boolean
    readonly full?: string
}

/** Display-only projection. Never pass this shortened text to scripts or storage. */
export function getStreamingThoughtPreview(
    source: string,
    includeFull = false,
): StreamingThoughtPreview | null {
    if (!source.includes(openTag)) return null
    let depth = 0
    let found = false
    let before = ''
    const outside: string[] = []
    let recent = ''
    let truncated = false
    let cursor = 0
    let spanStart = 0
    const full: string[] = []

    function appendSpan(end: number) {
        if (end <= spanStart) return
        if (depth === 0) {
            outside.push(source.slice(spanStart, end))
            return
        }
        // Slice only a bounded suffix, even when a provider replaces a huge block.
        const length = end - spanStart
        if (includeFull) full.push(source.slice(spanStart, end))
        if (length >= maxPreviewLength) {
            truncated ||= recent.length + length > maxPreviewLength
            recent = source.slice(end - maxPreviewLength, end)
        } else {
            recent += source.slice(spanStart, end)
            if (recent.length > maxPreviewLength) {
                truncated = true
                recent = recent.slice(-maxPreviewLength)
            }
        }
    }

    // One delimiter scan per accepted snapshot. No character-by-character copying
    // of the Thought body, and no assumptions about append-only provider output.
    while (cursor < source.length) {
        const next = source.indexOf('<', cursor)
        if (next === -1) break
        const opening = source.startsWith(openTag, next)
        const closing = depth > 0 && source.startsWith(closeTag, next)
        if (opening || closing) {
            appendSpan(next)
            if (opening) {
                if (depth === 0) {
                    if (includeFull && found) full.push('\n\n')
                    if (!found) {
                        before = outside.join('')
                        outside.length = 0
                    }
                    found = true
                    recent = ''
                    truncated = false
                }
                depth++
            } else depth--
            cursor = next + (opening ? openTag.length : closeTag.length)
            spanStart = cursor
        } else {
            const remaining = source.length - next
            if (
                remaining < closeTag.length &&
                (openTag.startsWith(source.slice(next)) ||
                    (depth > 0 && closeTag.startsWith(source.slice(next))))
            ) {
                // A delimiter split across updates must not leak into the preview.
                appendSpan(next)
                spanStart = source.length
                break
            }
            cursor = next + 1
        }
    }
    appendSpan(source.length)
    if (!found) return null

    if (truncated && recent) {
        // A bounded slice may begin inside a grapheme. Drop its first cluster;
        // older WebViews at least avoid a split UTF-16 surrogate pair.
        const first = segmenter?.segment(recent)[Symbol.iterator]().next().value
        recent = recent.slice(
            first?.segment.length ??
                (recent.charCodeAt(0) >= 0xdc00 &&
                recent.charCodeAt(0) <= 0xdfff
                    ? 1
                    : 0),
        )
    }
    recent = recent.trimEnd()
    let lineStart = recent.length
    for (let line = 0; line < previewLines; line++) {
        lineStart = recent.lastIndexOf('\n', lineStart - 1)
        if (lineStart < 0) break
    }
    if (lineStart >= 0) {
        recent = recent.slice(lineStart + 1)
        truncated = true
    }
    return {
        before,
        after: outside.join(''),
        recent,
        truncated,
        ...(includeFull ? { full: full.join('') } : {}),
    }
}

/** Markup is still passed through the normal Markdown sanitizer. */
export function renderStreamingThoughtPreview(
    preview: StreamingThoughtPreview,
    label: string,
): string {
    // Keep the HTML block on one source line so Markdown cannot resume parsing
    // inside a blank line in the thought text.
    const escape = (value: string) =>
        value
            .replaceAll('&', '&amp;')
            .replaceAll('<', '&lt;')
            .replaceAll('>', '&gt;')
            .replaceAll('"', '&quot;')
            .replaceAll('\r', '&#13;')
            .replaceAll('\n', '&#10;')
    return `${preview.before}\n\n<div class="x-risu-streaming-thought-preview" role="note" data-streaming-thought-preview><div class="x-risu-streaming-thought-label">${escape(label)}</div><div class="x-risu-streaming-thought-window" data-truncated="${preview.truncated}"><div class="x-risu-streaming-thought-text">${escape(preview.recent || '…')}</div></div></div>\n\n${preview.after}`
}

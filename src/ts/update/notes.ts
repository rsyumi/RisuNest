export type ReleaseNoteInline =
    | { type: 'text'; text: string }
    | { type: 'strong'; text: string }
    | { type: 'code'; text: string }
    | { type: 'link'; text: string; url: string }

export type ReleaseNoteBlock =
    | { type: 'heading'; level: 2 | 3; content: ReleaseNoteInline[] }
    | { type: 'paragraph'; content: ReleaseNoteInline[] }
    | { type: 'list-item'; depth: 1 | 2; content: ReleaseNoteInline[] }

const NOTE_LIMIT_BYTES = 4096

export function selectLocalizedNotes(
    localized: Record<string, string>,
    locale: string,
    fallback: string,
): string {
    const normalized = locale.toLowerCase()
    const base = normalized.split('-')[0]
    const entries = Object.entries(localized)
    const selected = localized[locale]
        ?? entries.find(([key]) => key.toLowerCase() === normalized)?.[1]
        ?? entries.find(([key]) => key.toLowerCase() === base)?.[1]
        ?? localized.en
        ?? entries.find(([key]) => key.toLowerCase() === 'en')?.[1]
        ?? entries[0]?.[1]
        ?? fallback
        ?? ''
    return truncateAtLineBoundary(selected, NOTE_LIMIT_BYTES)
}

export function parseReleaseNotes(markdown: string): ReleaseNoteBlock[] {
    const blocks: ReleaseNoteBlock[] = []
    const paragraphs: string[] = []
    const flush = () => {
        const text = paragraphs.join(' ').trim()
        if (text) blocks.push({ type: 'paragraph', content: parseInline(text) })
        paragraphs.length = 0
    }
    let inCodeBlock = false
    for (const sourceLine of markdown.replace(/\r\n?/g, '\n').split('\n')) {
        const line = sourceLine.replace(/<[^>]*>/g, match => match.replace(/[<>]/g, ''))
        if (line.trimStart().startsWith('```')) {
            flush()
            inCodeBlock = !inCodeBlock
            continue
        }
        if (inCodeBlock) {
            paragraphs.push(line)
            continue
        }
        const heading = line.match(/^(#{2,3})\s+(.+)$/)
        if (heading) {
            flush()
            blocks.push({ type: 'heading', level: heading[1].length as 2 | 3, content: parseInline(heading[2]) })
            continue
        }
        const item = line.match(/^(\s{0,4})[-*+]\s+(.+)$/)
        if (item) {
            flush()
            blocks.push({ type: 'list-item', depth: item[1].length >= 2 ? 2 : 1, content: parseInline(item[2]) })
            continue
        }
        if (!line.trim()) {
            flush()
            continue
        }
        if (/^(!?\||#{1}\s|>\s)/.test(line)) {
            paragraphs.push(line.replace(/^[>|]\s?/, ''))
        } else {
            paragraphs.push(line)
        }
    }
    flush()
    return blocks
}

function parseInline(value: string): ReleaseNoteInline[] {
    const tokens: ReleaseNoteInline[] = []
    const pattern = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\[[^\]\n]+\]\([^\s)]+\))/g
    let offset = 0
    for (const match of value.matchAll(pattern)) {
        if (match.index! > offset) tokens.push({ type: 'text', text: value.slice(offset, match.index) })
        const token = match[0]
        if (token.startsWith('`')) tokens.push({ type: 'code', text: token.slice(1, -1) })
        else if (token.startsWith('**')) tokens.push({ type: 'strong', text: token.slice(2, -2) })
        else {
            const link = token.match(/^\[([^\]]+)\]\(([^)]+)\)$/)!
            if (isSafeLink(link[2])) tokens.push({ type: 'link', text: link[1], url: link[2] })
            else tokens.push({ type: 'text', text: link[1] })
        }
        offset = match.index! + token.length
    }
    if (offset < value.length) tokens.push({ type: 'text', text: value.slice(offset) })
    return tokens
}

function isSafeLink(value: string): boolean {
    try {
        const url = new URL(value)
        return url.protocol === 'https:' && !url.username && !url.password
    } catch {
        return false
    }
}

function truncateAtLineBoundary(value: string, maxBytes: number): string {
    if (new TextEncoder().encode(value).byteLength <= maxBytes) return value
    const lines = value.replace(/\r\n?/g, '\n').split('\n')
    let result = ''
    for (const line of lines) {
        const candidate = result ? `${result}\n${line}` : line
        if (new TextEncoder().encode(candidate).byteLength > maxBytes) break
        result = candidate
    }
    return result
}

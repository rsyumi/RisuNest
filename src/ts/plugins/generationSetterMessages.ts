import type { Message } from '../storage/database.svelte'
import { pluginMessageDigest, pluginMessageIdDigest, type PluginMessageFingerprints } from './pluginReadBaselines'

export interface GenerationSetterRange {
    start: number
    before: Message[]
    messages: Message[]
    anchor?: Message
    head?: boolean
    anchorBefore?: boolean
    compound?: boolean
}

function indexes(ids: Float64Array | number[]) {
    const result = new Map<number, number>()
    ids.forEach((id, index) => { if (id >= 0) result.set(id, result.has(id) ? -1 : index) })
    return result
}

/** Resolve submitted changes against the current owner, preserving untouched rows and intervening writes. */
export function planGenerationSetterRanges(
    baseline: PluginMessageFingerprints | undefined,
    submitted: readonly Message[],
    current: readonly Message[],
    protectedIndexes: ReadonlySet<number>,
    positionalProof: (index: number) => boolean,
): GenerationSetterRange[] {
    const oldIds = baseline?.ids ?? new Float64Array()
    const nextIds = submitted.map(row => pluginMessageIdDigest(row.chatId))
    const liveIds = current.map(row => pluginMessageIdDigest(row.chatId))
    const old = indexes(oldIds), next = indexes(nextIds), live = indexes(liveIds)
    const hadIdless = oldIds.includes(-1)
    const positional = oldIds.length === submitted.length &&
        oldIds.every((id, index) => id === nextIds[index])
    const changes = new Map<number, Message>()
    const removed = new Set<number>()
    const paired: number[] = []
    const inserts = new Map<number, Message[]>()
    let anchor: number | undefined = -1
    for (let index = 0; index < submitted.length; index++) {
        const message = submitted[index], id = nextIds[index]
        const previous = id >= 0 ? old.get(id) : undefined
        const isPair = id >= 0 && previous !== undefined && previous >= 0 && next.get(id) === index
        if (isPair) paired.push(previous)
        const positionalPair = positional && index < current.length && (id < 0 || (previous !== undefined && previous < 0 && liveIds[index] === id)) &&
            positionalProof(index)
        if (isPair || positionalPair) {
            const baseIndex = isPair ? previous : index
            const target = isPair ? live.get(id!) : index
            anchor = target !== undefined && target >= 0 ? target : undefined
            if (anchor !== undefined && (!isPair || current[anchor].chatId === message.chatId) &&
                pluginMessageDigest(message) !== baseline!.digests[baseIndex]) {
                const replacement = message.chatId ? message : { ...message, chatId: current[anchor].chatId || crypto.randomUUID() }
                changes.set(anchor, replacement)
            }
        } else if (anchor !== undefined && (
            (id >= 0 && previous === undefined && !live.has(id)) ||
            (id < 0 && !hadIdless)
        )) {
            if (protectedIndexes.has(anchor) && protectedIndexes.has(anchor + 1)) continue
            const rows = inserts.get(anchor) ?? []
            rows.push(id >= 0 && next.get(id) === index ? message : { ...message, chatId: crypto.randomUUID() })
            inserts.set(anchor, rows)
        }
    }
    for (const [id, index] of old) {
        const position = live.get(id)
        if (index >= 0 && !next.has(id) && position !== undefined && position >= 0 && !protectedIndexes.has(position))
            removed.add(position)
    }
    const pairedIndexes = new Set(paired)
    const order: number[] = []
    for (let i = 0; i < oldIds.length; i++) if (pairedIndexes.has(i)) order.push(i)
    const firstMove = paired.findIndex((value, i) => value !== order[i])
    let move: { start: number; end: number; order: number[] } | undefined
    if (firstMove >= 0) {
        let last = paired.length - 1
        while (paired[last] === order[last]) last--
        const oldRange = order.slice(firstMove, last + 1).map(i => live.get(oldIds[i]!))
        const first = oldRange[0]
        let cursor = first ?? -1
        const contiguous = oldRange.every(position => {
            while (removed.has(cursor)) cursor++
            return position === cursor++ && !protectedIndexes.has(position)
        })
        if (first !== undefined && first >= 0 && contiguous) {
            move = { start: first, end: cursor, order: paired.slice(firstMove, last + 1).map(i => live.get(oldIds[i]!)!) }
        }
    }

    const ranges: GenerationSetterRange[] = []
    let offset = 0
    const emit = (start: number, end: number, messages: Message[], compound = false, anchorBefore = false) => {
        ranges.push({ start: start + offset, before: current.slice(start, end), messages,
            anchor: current[anchorBefore ? start - 1 : start], head: start === 0 && end === 0, anchorBefore, compound })
        offset += messages.length - (end - start)
    }
    if (inserts.has(-1)) emit(0, 0, inserts.get(-1)!)
    for (let i = 0; i < current.length; i++) {
        if (i === move?.start) {
            const rows: Message[] = []
            for (const j of move.order) {
                rows.push(changes.get(j) ?? current[j])
                for (const inserted of inserts.get(j) ?? []) rows.push(inserted)
            }
            emit(i, move.end, rows, true)
            i = move.end - 1
            continue
        }
        if (removed.has(i)) emit(i, i + 1, [])
        else if (changes.has(i)) emit(i, i + 1, [changes.get(i)!])
        if (inserts.has(i)) emit(i + 1, i + 1, inserts.get(i)!, false, true)
    }
    return ranges
}

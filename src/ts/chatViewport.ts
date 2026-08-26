export type ChatViewportPinReason = 'streaming' | 'editor' | 'playing-media'

export interface ChatViewportAnchor {
    key: string
    indexHint: number
    relativeOffset: number
}

export interface ChatViewportPin {
    key: string
    reason: ChatViewportPinReason
}

export interface ChatViewportInput {
    keys: readonly string[]
    budget: number
    overscan: number
    estimatedMessageHeight: number
    measuredHeights?: ReadonlyMap<string, number>
    anchor?: ChatViewportAnchor | null
    jumpTarget?: number
    pins?: readonly ChatViewportPin[]
}

export interface ChatViewportMessageRow {
    kind: 'message'
    index: number
    key: string
    pinReasons: readonly ChatViewportPinReason[]
}

export interface ChatViewportGapRow {
    kind: 'gap'
    startIndex: number
    endIndex: number
    height: number
}

export type ChatViewportRow = ChatViewportMessageRow | ChatViewportGapRow

export interface ChatViewportPinOverflow {
    count: number
    pinnedMessageCount: number
    pinnedKeys: readonly string[]
}

export interface ChatViewportResult {
    rows: readonly ChatViewportRow[]
    messageRows: readonly ChatViewportMessageRow[]
    mountedMessageCount: number
    anchor: ChatViewportAnchor | null
    jumpAccepted: boolean | null
    pinOverflow: ChatViewportPinOverflow | null
}

function normalizeBudget(value: number): number {
    return Number.isInteger(value) && value > 0 ? value : 1
}

function normalizeOverscan(value: number, budget: number): number {
    if (!Number.isInteger(value) || value < 0) return 0
    return Math.min(value, budget - 1)
}

function resolveAnchor(keys: readonly string[], anchor: ChatViewportAnchor | null | undefined): ChatViewportAnchor | null {
    if (keys.length === 0) return null
    if (!anchor) {
        const index = keys.length - 1
        return { key: keys[index], indexHint: index, relativeOffset: 0 }
    }

    const stableIndex = keys.indexOf(anchor.key)
    if (stableIndex >= 0) return { ...anchor, indexHint: stableIndex }

    const index = Math.min(Math.max(Math.trunc(anchor.indexHint), 0), keys.length - 1)
    return { key: keys[index], indexHint: index, relativeOffset: anchor.relativeOffset }
}

function gapHeight(input: ChatViewportInput, startIndex: number, endIndex: number): number {
    const estimate = Number.isFinite(input.estimatedMessageHeight)
        ? Math.max(0, input.estimatedMessageHeight)
        : 0
    let height = 0
    for (let index = startIndex; index < endIndex; index++) {
        const measured = input.measuredHeights?.get(input.keys[index])
        height += measured !== undefined && Number.isFinite(measured) && measured >= 0
            ? measured
            : estimate
    }
    return height
}

function createRows(
    input: ChatViewportInput,
    messageRows: readonly ChatViewportMessageRow[],
): ChatViewportRow[] {
    const rows: ChatViewportRow[] = []
    let nextIndex = 0
    for (const messageRow of messageRows) {
        if (nextIndex < messageRow.index) {
            rows.push({
                kind: 'gap',
                startIndex: nextIndex,
                endIndex: messageRow.index,
                height: gapHeight(input, nextIndex, messageRow.index),
            })
        }
        rows.push(messageRow)
        nextIndex = messageRow.index + 1
    }
    if (nextIndex < input.keys.length) {
        rows.push({
            kind: 'gap',
            startIndex: nextIndex,
            endIndex: input.keys.length,
            height: gapHeight(input, nextIndex, input.keys.length),
        })
    }
    return rows
}

export function buildChatViewport(input: ChatViewportInput): ChatViewportResult {
    const budget = normalizeBudget(input.budget)
    const overscan = normalizeOverscan(input.overscan, budget)
    const previousAnchor = resolveAnchor(input.keys, input.anchor)
    const hasJump = input.jumpTarget !== undefined
    const jumpAccepted = hasJump
        ? Number.isInteger(input.jumpTarget) && input.jumpTarget! >= 0 && input.jumpTarget! < input.keys.length
        : null
    const anchor = jumpAccepted
        ? {
            key: input.keys[input.jumpTarget!],
            indexHint: input.jumpTarget!,
            relativeOffset: 0,
        }
        : previousAnchor
    const focusIndex = anchor?.indexHint ?? 0
    const beforeFocus = overscan
    const maxStart = Math.max(0, input.keys.length - budget)
    const startIndex = input.anchor || hasJump
        ? Math.min(Math.max(0, focusIndex - beforeFocus), maxStart)
        : maxStart
    const endIndex = Math.min(input.keys.length, startIndex + budget)
    const selectedIndices = new Set<number>()
    for (let index = startIndex; index < endIndex; index++) selectedIndices.add(index)

    const pinReasonsByIndex = new Map<number, ChatViewportPinReason[]>()
    for (const pin of input.pins ?? []) {
        const index = input.keys.indexOf(pin.key)
        if (index < 0) continue
        const reasons = pinReasonsByIndex.get(index) ?? []
        if (!reasons.includes(pin.reason)) reasons.push(pin.reason)
        pinReasonsByIndex.set(index, reasons)
        selectedIndices.add(index)
    }

    const mountedLimit = Math.max(budget, pinReasonsByIndex.size)
    while (selectedIndices.size > mountedLimit) {
        const removable = [...selectedIndices]
            .filter((index) => !pinReasonsByIndex.has(index))
            .sort((left, right) => {
                const distance = Math.abs(right - focusIndex) - Math.abs(left - focusIndex)
                return distance || right - left
            })[0]
        if (removable === undefined) break
        selectedIndices.delete(removable)
    }

    const messageRows = [...selectedIndices]
        .sort((left, right) => left - right)
        .map((index): ChatViewportMessageRow => ({
            kind: 'message',
            index,
            key: input.keys[index],
            pinReasons: pinReasonsByIndex.get(index) ?? [],
        }))
    const rows = createRows(input, messageRows)

    return {
        rows,
        messageRows,
        mountedMessageCount: messageRows.length,
        anchor,
        jumpAccepted,
        pinOverflow: pinReasonsByIndex.size > budget
            ? {
                count: pinReasonsByIndex.size - budget,
                pinnedMessageCount: pinReasonsByIndex.size,
                pinnedKeys: [...pinReasonsByIndex.keys()]
                    .sort((left, right) => left - right)
                    .map((index) => input.keys[index]),
            }
            : null,
    }
}

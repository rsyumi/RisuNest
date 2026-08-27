export type ChatViewportPinReason = 'streaming' | 'editor' | 'playing-media'

export interface ChatViewportAnchor {
    key: string
    indexHint: number
    relativeOffset: number
}

export interface ChatViewportPin {
    key: string
    reason: ChatViewportPinReason
    indexHint?: number
}

export interface ChatViewportJumpOptions {
    align?: 'start' | 'center'
    highlight?: boolean
}

export interface ChatViewportHandle {
    jumpTo(index: number, options?: ChatViewportJumpOptions): Promise<boolean>
    jumpToLatestMessage(): Promise<void>
    scrollToLatestMessage(): Promise<void>
}

export interface ChatViewportKeySource {
    readonly length: number
    keyAt(index: number): string | undefined
    indexOf(key: string): number
}

export interface ChatViewportInput {
    keys?: readonly string[]
    keySource?: ChatViewportKeySource
    budget: number
    overscan: number
    estimatedMessageHeight: number
    measuredHeights?: ReadonlyMap<string, number>
    measuredHeightsByIndex?: ReadonlyMap<number, number>
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

function keySource(input: ChatViewportInput): ChatViewportKeySource {
    if (input.keySource) return input.keySource
    const keys = input.keys ?? []
    return {
        length: keys.length,
        keyAt: (index) => keys[index],
        indexOf: (key) => keys.indexOf(key),
    }
}

function requireKey(source: ChatViewportKeySource, index: number): string {
    const key = source.keyAt(index)
    if (key === undefined) throw new RangeError(`Viewport key ${index} is unavailable`)
    return key
}

function resolveAnchor(
    source: ChatViewportKeySource,
    anchor: ChatViewportAnchor | null | undefined,
): ChatViewportAnchor | null {
    if (source.length === 0) return null
    if (!anchor) {
        const index = source.length - 1
        return { key: requireKey(source, index), indexHint: index, relativeOffset: 0 }
    }

    const hintedIndex = Math.trunc(anchor.indexHint)
    const stableIndex = (
        hintedIndex >= 0 &&
        hintedIndex < source.length &&
        source.keyAt(hintedIndex) === anchor.key
    )
        ? hintedIndex
        : source.indexOf(anchor.key)
    if (stableIndex >= 0) return { ...anchor, indexHint: stableIndex }

    const index = Math.min(Math.max(Math.trunc(anchor.indexHint), 0), source.length - 1)
    return { key: requireKey(source, index), indexHint: index, relativeOffset: anchor.relativeOffset }
}

function gapHeight(
    input: ChatViewportInput,
    source: ChatViewportKeySource,
    startIndex: number,
    endIndex: number,
): number {
    const estimate = Number.isFinite(input.estimatedMessageHeight)
        ? Math.max(0, input.estimatedMessageHeight)
        : 0
    let height = (endIndex - startIndex) * estimate
    if (input.measuredHeightsByIndex) {
        for (const [index, measured] of input.measuredHeightsByIndex) {
            if (
                index < startIndex ||
                index >= endIndex ||
                !Number.isFinite(measured) ||
                measured < 0
            ) continue
            height += measured - estimate
        }
        return height
    }
    if (!input.measuredHeights) return height
    for (let index = startIndex; index < endIndex; index++) {
        const measured = input.measuredHeights.get(requireKey(source, index))
        if (measured !== undefined && Number.isFinite(measured) && measured >= 0) {
            height += measured - estimate
        }
    }
    return height
}

function createRows(
    input: ChatViewportInput,
    source: ChatViewportKeySource,
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
                height: gapHeight(input, source, nextIndex, messageRow.index),
            })
        }
        rows.push(messageRow)
        nextIndex = messageRow.index + 1
    }
    if (nextIndex < source.length) {
        rows.push({
            kind: 'gap',
            startIndex: nextIndex,
            endIndex: source.length,
            height: gapHeight(input, source, nextIndex, source.length),
        })
    }
    return rows
}

export function buildChatViewport(input: ChatViewportInput): ChatViewportResult {
    const source = keySource(input)
    const budget = normalizeBudget(input.budget)
    const overscan = normalizeOverscan(input.overscan, budget)
    const previousAnchor = resolveAnchor(source, input.anchor)
    const hasJump = input.jumpTarget !== undefined
    const jumpAccepted = hasJump
        ? Number.isInteger(input.jumpTarget) && input.jumpTarget! >= 0 && input.jumpTarget! < source.length
        : null
    const anchor = jumpAccepted
        ? {
            key: requireKey(source, input.jumpTarget!),
            indexHint: input.jumpTarget!,
            relativeOffset: 0,
        }
        : previousAnchor
    const focusIndex = anchor?.indexHint ?? 0
    const beforeFocus = overscan
    const maxStart = Math.max(0, source.length - budget)
    const startIndex = input.anchor || hasJump
        ? Math.min(Math.max(0, focusIndex - beforeFocus), maxStart)
        : maxStart
    const endIndex = Math.min(source.length, startIndex + budget)
    const selectedIndices = new Set<number>()
    for (let index = startIndex; index < endIndex; index++) selectedIndices.add(index)

    const pinReasonsByIndex = new Map<number, ChatViewportPinReason[]>()
    for (const pin of input.pins ?? []) {
        const hintedIndex = pin.indexHint
        const index = (
            hintedIndex !== undefined &&
            Number.isSafeInteger(hintedIndex) &&
            hintedIndex >= 0 &&
            hintedIndex < source.length &&
            source.keyAt(hintedIndex) === pin.key
        )
            ? hintedIndex
            : source.indexOf(pin.key)
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
            key: requireKey(source, index),
            pinReasons: pinReasonsByIndex.get(index) ?? [],
        }))
    const rows = createRows(input, source, messageRows)

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
                    .map((index) => requireKey(source, index)),
            }
            : null,
    }
}

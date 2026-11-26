import { v4 } from 'uuid'
import type { Message } from './database.svelte'
import { RevisionConflictError, type ConversationMutation, type PersistentRevisionReader } from './persistentDataStore'
import { safeStructuredClone } from '../safeStructuredClone'

type Reader = Pick<PersistentRevisionReader, 'revision' | 'readConversationWindow'>
type Mutation = Extract<ConversationMutation, { type: 'replace-range' }>

function usableId(id: unknown): id is string {
    return typeof id === 'string' && id.length > 0
}

// Targets use local identity even when the submitted body changes its chatId.
export interface WindowedRowTarget { readonly key: number }
type Target = WindowedRowTarget
interface Binding { index: number; id?: string; unique: boolean; checked?: boolean; unknownId?: boolean; body?: Message }
interface Output { target: Target; message: Message; previous?: Target }
interface Edit {
    sequence: number
    before: Target[]
    after: Output[]
    append?: boolean
    head?: boolean
    anchor?: Target
    anchorBefore?: boolean
    tail?: boolean
}
type Piece = { stored: number; length: number } | { output: Output; length: 1 }
export type WindowedOverlayInterval =
    | { start: number; length: number; storedStart: number }
    | { start: number; length: number; messages: Message[] }

export interface WindowedOverlayProjection {
    readonly count: number
    readonly revision: number
    readonly epoch: number
    readonly version: number
    intervals(start: number, limit: number): WindowedOverlayInterval[]
    row(index: number): { message: Message } | { storedIndex: number } | null
    storedToVisible(index: number): number | null
}

export interface WindowedOverlayPrefix {
    readonly version: number
    readonly dirty: boolean
    readonly through: number
    readonly entries: readonly Edit[]
}
type Occurrence = { kind: 'unique'; index: number; message: Message } | { kind: 'missing' | 'duplicate' }
interface Evaluation {
    view: SparseRows
    kept: Set<number>
}
export interface WindowedOverlayPlan {
    readonly revision: number
    readonly prefix: WindowedOverlayPrefix
    readonly mutations: Mutation[]
    readonly storedCount: number
    readonly evaluation: Evaluation
    readonly bindings: Map<Target, Binding>
    readonly occurrences: Map<string, Occurrence>
}

// Untouched history occupies a single interval, independently of its length.
class SparseRows {
    pieces: Piece[]
    readonly aliases = new Map<Target, Target>()
    private index?: {
        visibleStarts: number[]
        stores: Array<{ start: number; end: number; visible: number }>
        outputs: Map<Target, number>
    }
    constructor(public count: number, readonly bindings: ReadonlyMap<Target, Binding>) {
        this.pieces = count ? [{ stored: 0, length: count }] : []
        const at = new Map<number, Target>()
        for (const [target, binding] of bindings) {
            const canonical = at.get(binding.index)
            if (canonical) this.aliases.set(target, canonical)
            else at.set(binding.index, target)
        }
    }

    private indexed() {
        if (this.index) return this.index
        const visibleStarts: number[] = []
        const stores: Array<{ start: number; end: number; visible: number }> = []
        const outputs = new Map<Target, number>()
        let visible = 0
        for (const piece of this.pieces) {
            visibleStarts.push(visible)
            if ('stored' in piece) stores.push({ start: piece.stored, end: piece.stored + piece.length, visible })
            else outputs.set(this.canonical(piece.output.target), visible)
            visible += piece.length
        }
        return this.index = { visibleStarts, stores, outputs }
    }

    at(index: number): { output: Output } | { stored: number } | null {
        if (!Number.isSafeInteger(index) || index < 0 || index >= this.count) return null
        const starts = this.indexed().visibleStarts
        const at = floorIndex(starts, (value) => value <= index)
        const piece = this.pieces[at]
        return 'stored' in piece ? { stored: piece.stored + index - starts[at] } : { output: piece.output }
    }

    storedToVisible(index: number): number | null {
        const stores = this.indexed().stores
        const range = stores[floorIndex(stores, (value) => value.start <= index)]
        return range && index < range.end ? range.visible + index - range.start : null
    }

    private canonical(original: Target) {
        let target = original
        while (this.aliases.has(target)) target = this.aliases.get(target)!
        return target
    }

    locate(original: Target): number | null {
        const target = this.canonical(original)
        const visible = this.indexed().outputs.get(target)
        if (visible !== undefined) return visible
        const binding = this.bindings.get(target)
        return binding ? this.storedToVisible(binding.index) : null
    }

    private split(index: number): number {
        for (let i = 0; i < this.pieces.length; i++) {
            if (index === 0) return i
            const piece = this.pieces[i]
            if (index < piece.length && 'stored' in piece) {
                this.pieces.splice(i, 1,
                    { stored: piece.stored, length: index },
                    { stored: piece.stored + index, length: piece.length - index })
                return i + 1
            }
            index -= piece.length
        }
        return this.pieces.length
    }

    splice(start: number, count: number, after: Output[]) {
        this.index = undefined
        if (start === this.count && count === 0) {
            for (const output of after) this.pieces.push({ output, length: 1 })
            this.count += after.length
            return
        }
        const left = this.split(start)
        const right = this.split(start + count)
        const inserted: Piece[] = after.map((output) => ({ output, length: 1 }))
        this.pieces = this.pieces.slice(0, left).concat(inserted, this.pieces.slice(right))
        this.count += after.length - count
        for (const { target, previous } of after) {
            if (previous && this.canonical(previous) !== this.canonical(target)) this.aliases.set(this.canonical(previous), target)
        }
    }

    replaceSlots(changes: Map<number, Output>) {
        if (!changes.size) return
        const positions = [...changes.keys()].sort((a, b) => a - b)
        const result: Piece[] = []
        let visible = 0
        let change = 0
        for (const piece of this.pieces) {
            const end = visible + piece.length
            let cursor = visible
            while (change < positions.length && positions[change] < end) {
                const at = positions[change++]
                if (cursor < at && 'stored' in piece) result.push({ stored: piece.stored + cursor - visible, length: at - cursor })
                result.push({ output: changes.get(at)!, length: 1 })
                cursor = at + 1
            }
            if (cursor === visible) result.push(piece)
            else if (cursor < end && 'stored' in piece) result.push({ stored: piece.stored + cursor - visible, length: end - cursor })
            visible = end
        }
        this.pieces = result
        this.index = undefined
    }

    intervals(start: number, limit: number): WindowedOverlayInterval[] {
        const result: WindowedOverlayInterval[] = []
        if (limit <= 0 || start >= this.count || !this.pieces.length) return result
        const starts = this.indexed().visibleStarts
        const firstPiece = Math.max(0, floorIndex(starts, (offset) => offset <= start))
        for (let i = firstPiece; i < this.pieces.length; i++) {
            const piece = this.pieces[i]
            const offset = starts[i]
            if (offset >= start + limit) break
            const first = Math.max(start, offset)
            const last = Math.min(start + limit, offset + piece.length)
            if (first < last) {
                if ('stored' in piece) {
                    result.push({ start: first, length: last - first, storedStart: piece.stored + first - offset })
                } else {
                    const previous = result.at(-1)
                    if (previous && 'messages' in previous) {
                        previous.messages.push(piece.output.message)
                        previous.length++
                    } else result.push({ start: first, length: 1, messages: [piece.output.message] })
                }
            }
        }
        return result
    }

    mutations(storedCount: number, characterId: string, conversationId: string): Mutation[] {
        const ranges: Mutation[] = []
        let start = 0
        let inserted: Message[] = []
        const flush = (end: number) => {
            if (end > start || inserted.length) {
                ranges.push({ type: 'replace-range', characterId, conversationId, start, deleteCount: end - start, messages: inserted })
            }
            inserted = []
        }
        for (const piece of this.pieces) {
            if ('stored' in piece) {
                flush(piece.stored)
                start = piece.stored + piece.length
            } else inserted.push(piece.output.message)
        }
        flush(storedCount)
        return ranges.reverse()
    }
}

function floorIndex<T>(values: T[], beforeOrEqual: (value: T) => boolean): number {
    let lo = 0
    let hi = values.length
    while (lo < hi) {
        const mid = Math.floor((lo + hi) / 2)
        if (beforeOrEqual(values[mid])) lo = mid + 1
        else hi = mid
    }
    return lo - 1
}

function evaluate(count: number, bindings: ReadonlyMap<Target, Binding>, entries: readonly Edit[],
    occurrences?: ReadonlyMap<string, Occurrence>): Evaluation {
    const view = new SparseRows(count, bindings)
    const kept = new Set<number>()
    let pending = new Map<number, Output>()
    let appendIds: Set<string> | undefined
    for (const edit of entries) {
        if (edit.before.length === 1 && edit.after.length === 1 && edit.after[0].target === edit.before[0]) {
            const index = view.locate(edit.before[0])
            if (index !== null) {
                pending.set(index, edit.after[0])
                kept.add(edit.sequence)
            }
            continue
        }
        if (pending.size) {
            view.replaceSlots(pending)
            pending = new Map()
            appendIds = undefined
        }
        if (!edit.before.length && occurrences) {
            appendIds ??= new Set(view.pieces.flatMap((piece) => 'output' in piece ? [piece.output.message.chatId] : []))
            const ids = edit.after.map((output) => output.message.chatId!)
            if (new Set(ids).size !== ids.length ||
                ids.some((id) => occurrences.get(id)?.kind !== 'missing' || appendIds!.has(id))) continue
        }
        let start: number | null
        if (edit.before.length) {
            start = view.locate(edit.before[0])
            if (start === null || edit.before.some((target, i) => view.locate(target) !== start! + i)) continue
            if (edit.tail && start + edit.before.length !== view.count) continue
        } else if (edit.head) {
            start = 0
        } else if (edit.append) {
            appendIds?.add(edit.after[0].message.chatId!)
            start = view.count
        } else if (edit.anchor) {
            start = view.locate(edit.anchor)
            if (start === null) continue
            if (edit.anchorBefore) start++
        } else continue
        view.splice(start, edit.before.length, edit.after)
        if (!edit.append) appendIds = undefined
        kept.add(edit.sequence)
    }
    view.replaceSlots(pending)
    return { view, kept }
}

export class WindowedConversationOverlay {
    private bindings = new Map<Target, Binding>()
    private readonly held = new Map<Target, number>()
    private readonly freezes = new Set<WindowedOverlayPrefix>()
    // A conflict in a commit carrying other edits can invalidate a tentative drop.
    private readonly unresolved = new Map<Target, string>()
    private hidden = new Set<number>()
    private collisionProofs = new Map<string, Occurrence>()
    private entries: Edit[] = []
    private sequence = 0
    private nextKey = 0
    private sessionVersion = 0
    private generation = 0
    private preparingRevision = -1
    private accepting = true
    private view: SparseRows
    version = 0
    epoch = 0
    persistedVersion: number

    constructor(readonly characterId: string, readonly conversationId: string,
        public revision: number, public storedCount: number, sessionVersion = 0) {
        this.view = new SparseRows(storedCount, this.bindings)
        this.sessionVersion = sessionVersion
        this.persistedVersion = sessionVersion
    }

    get count() { return this.view.count }
    get dirty() { return this.entries.length > 0 }

    captureProjection(): WindowedOverlayProjection {
        const { view, revision, epoch, version } = this
        return {
            count: view.count, revision, epoch, version,
            intervals: (start, limit) => view.intervals(start, limit),
            storedToVisible: (index) => view.storedToVisible(index),
            row: (index) => {
                const slot = view.at(index)
                if (!slot) return null
                return 'stored' in slot ? { storedIndex: slot.stored } : { message: slot.output.message }
            },
        }
    }

    /** Complete sessions can resolve against their two resident arrays without a store lease. */
    projectCompleteRows(base: readonly Message[], stored: readonly Message[]): {
        mutations: Mutation[]
        locate(target: WindowedRowTarget): number | null
    } {
        if (base.length !== this.storedCount) throw new RangeError('Complete projection requires the captured message count')
        const needed = new Set<string>()
        const capturedId = (binding: Binding) => binding.unknownId ? base[binding.index]?.chatId : binding.id
        for (const binding of this.bindings.values()) {
            const id = capturedId(binding)
            if (usableId(id)) needed.add(id)
        }
        for (const entry of this.entries) {
            if (!entry.before.length) for (const output of entry.after) if (output.message.chatId) needed.add(output.message.chatId)
        }
        const occurrences = (messages: readonly Message[]) => {
            const result = new Map<string, Occurrence>()
            for (const id of needed) result.set(id, { kind: 'missing' })
            messages.forEach((message, index) => {
                const id = message.chatId
                if (id && needed.has(id)) result.set(id, result.get(id)!.kind === 'missing'
                    ? { kind: 'unique', index, message } : { kind: 'duplicate' })
            })
            return result
        }
        const source = occurrences(base)
        const destination = occurrences(stored)
        const resolved = new Map<Target, Binding>()
        for (const [target, binding] of this.bindings) {
            const id = capturedId(binding)
            const original = id ? source.get(id) : undefined
            const current = id ? destination.get(id) : undefined
            if (original?.kind === 'unique' && original.index === binding.index && current?.kind === 'unique') {
                resolved.set(target, { index: current.index, id, unique: true })
            }
        }
        const { view } = evaluate(stored.length, resolved, this.entries, destination)
        return {
            mutations: view.mutations(stored.length, this.characterId, this.conversationId),
            locate: (target) => view.locate(target),
        }
    }

    row(index: number) { return this.captureProjection().row(index) }
    intervals(start: number, limit: number) { return this.view.intervals(start, limit) }
    storedToVisible(index: number) { return this.view.storedToVisible(index) }

    private targetAt(index: number, message?: Readonly<Message>, byIndex?: Map<number, Target>): Target {
        const slot = this.view.at(index)
        if (!slot) throw new RangeError('Windowed edit refers to an absent row')
        if ('output' in slot) return slot.output.target
        let existing = byIndex?.get(slot.stored)
        if (!byIndex) for (const [target, binding] of this.bindings) {
            if (binding.index === slot.stored) { existing = target; break }
        }
        if (existing) {
            const binding = this.bindings.get(existing)!
            if (binding.unknownId && message) {
                this.bindings.set(existing, { ...binding, id: usableId(message.chatId) ? message.chatId : undefined, unknownId: false })
            }
            return existing
        }
        const target = { key: ++this.nextKey }
        this.bindings.set(target, { index: slot.stored, id: usableId(message?.chatId) ? message.chatId : undefined,
            unknownId: !message, unique: false })
        byIndex?.set(slot.stored, target)
        return target
    }

    bind(index: number, message: Readonly<Message>): Target {
        const target = this.targetAt(index, message)
        this.held.set(target, (this.held.get(target) ?? 0) + 1)
        const binding = this.bindings.get(target)
        if (binding && !binding.body) binding.body = safeStructuredClone(message)
        return target
    }

    locate(target: Target) { return this.view.locate(target) }
    readTarget(target: Target): Message | null {
        const index = this.locate(target)
        if (index === null) return null
        const slot = this.view.at(index)
        return slot && 'output' in slot ? slot.output.message : this.bindings.get(target)?.body ?? null
    }

    release(target: Target) {
        const holds = this.held.get(target) ?? 0
        if (holds > 1) this.held.set(target, holds - 1)
        else {
            this.held.delete(target)
            const binding = this.bindings.get(target)
            if (binding) delete binding.body
            this.prune()
        }
    }

    private async occurrence(reader: Reader, id: string): Promise<Occurrence> {
        const lookup = async (anchorOccurrence: 'first' | 'last') => {
            const result = await reader.readConversationWindow({
                characterId: this.characterId, conversationId: this.conversationId,
                anchorMessageId: id, anchorOccurrence, before: 0, after: 0,
            })
            if (!result) return null
            if (result.revision !== reader.revision) throw new RevisionConflictError(reader.revision, result.revision)
            const page = result.value
            if (page.characterId !== this.characterId || page.conversationId !== this.conversationId ||
                page.messages.length !== 1 || page.messages[0].chatId !== id) {
                throw new Error('Windowed conversation anchor returned an inconsistent row')
            }
            return { index: page.startIndex, message: page.messages[0] }
        }
        const first = await lookup('first')
        if (!first) return { kind: 'missing' }
        const last = await lookup('last')
        if (!last) throw new Error('Windowed conversation anchor disappeared on its pinned revision')
        return first.index === last.index ? { kind: 'unique', ...first } : { kind: 'duplicate' }
    }

    async prove(target: Target, reader: Reader): Promise<void> {
        const binding = this.bindings.get(target)
        if (reader.revision !== this.revision || !binding?.id || binding.unique || binding.checked) return
        const hit = await this.occurrence(reader, binding.id)
        if (reader.revision === this.revision && this.bindings.get(target) === binding) {
            binding.unique = hit.kind === 'unique' && hit.index === binding.index
            binding.checked = true
            if (binding.unique && hit.kind === 'unique' && this.held.has(target)) binding.body = hit.message
        }
    }

    private refresh(structural = false) {
        if (structural) this.epoch++
        this.view = evaluate(this.storedCount, this.bindings, this.entries.filter((edit) => !this.hidden.has(edit.sequence))).view
        this.version++
    }

    applyRange(start: number, before: readonly Message[], after: readonly Message[], sessionVersion: number,
        options: { compound?: boolean; tail?: boolean; head?: boolean; anchor?: Readonly<Message>; anchorBefore?: boolean } = {}): () => void {
        if (!this.accepting || !Number.isSafeInteger(start) || start < 0 || start + before.length > this.count) {
            throw new RangeError('Windowed edit exceeds the current conversation')
        }
        const previousEntries = this.entries
        const previousVersion = this.sessionVersion
        const previousEpoch = this.epoch
        const previousView = this.view
        const previousBindings = this.bindings
        this.bindings = new Map(this.bindings)
        this.entries = this.entries.slice()
        const rollback = () => {
            this.entries = previousEntries
            this.sessionVersion = previousVersion
            this.epoch = previousEpoch
            this.bindings = previousBindings
            this.view = previousView
            this.version++
        }
        try {
            const byIndex = before.length > 1
                ? new Map(Array.from(this.bindings, ([target, binding]) => [binding.index, target])) : undefined
            const targets = before.map((message, i) => this.targetAt(start + i, message, byIndex))
            const values = after.map((message) => {
                const value = safeStructuredClone(message)
                if (!usableId(value.chatId)) value.chatId = v4()
                return value
            })
            const sameIds = before.length === values.length && before.every((message, i) => message.chatId === values[i].chatId)
            const independent = !options.compound && !options.tail && sameIds &&
                (before.length <= 1 || before.every((message, i) => !message.responseVariants && !values[i].responseVariants))
            const frozenThrough = Math.max(0, ...Array.from(this.freezes, (prefix) => prefix.through))
            const sequence = () => ++this.sequence
            if (independent) {
                const producers = new Map<Target, number>()
                for (let i = 0; i < this.entries.length; i++) {
                    const edit = this.entries[i]
                    if (edit.sequence > frozenThrough) for (const output of edit.after) producers.set(output.target, i)
                }
                const replacements = new Map<number, Map<Target, Message>>()
                for (let i = 0; i < targets.length; i++) {
                    const target = targets[i]
                    const message = values[i]
                    const producer = producers.get(target)
                    if (producer !== undefined) {
                        const changes = replacements.get(producer) ?? new Map<Target, Message>()
                        changes.set(target, message)
                        replacements.set(producer, changes)
                    } else {
                        this.entries.push({ sequence: sequence(), before: [target], after: [{ target, message }] })
                    }
                }
                for (const [producer, changes] of replacements) {
                    const entry = this.entries[producer]
                    this.entries[producer] = { ...entry, after: entry.after.map((output) => {
                        const message = changes.get(output.target)
                        return message ? { ...output, message } : output
                    }) }
                }
            } else if (!before.length && !options.head && start === this.count && (!options.anchorBefore || !this.count)) {
                for (const message of values) this.entries.push({
                    sequence: sequence(), before: [], append: true, after: [{ target: { key: ++this.nextKey }, message }],
                })
            } else if (before.length || values.length) {
                const cancelled = targets.length === 1 && !values.length
                    ? this.entries.findIndex((entry) => entry.sequence > frozenThrough && entry.append &&
                        entry.after[0].target === targets[0] &&
                        !this.entries.some((dependent) => dependent.anchor === targets[0] || dependent.before.includes(targets[0])))
                    : -1
                if (cancelled >= 0) {
                    this.entries.splice(cancelled, 1)
                } else {
                    const used = new Set<Target>()
                    const originalIndexes = new Map<string, number>()
                    before.forEach((message, index) => {
                        if (message.chatId) originalIndexes.set(message.chatId, originalIndexes.has(message.chatId) ? -1 : index)
                    })
                    const outputs = values.map((message, i) => {
                        let previous: Target | undefined
                        if (before[i]?.chatId === message.chatId) previous = targets[i]
                        else {
                            const at = originalIndexes.get(message.chatId)
                            if (at !== undefined && at >= 0) previous = targets[at]
                            else if (before.length === 1 && values.length === 1) previous = targets[0]
                        }
                        if (previous && used.has(previous)) previous = undefined
                        if (previous) used.add(previous)
                        return { target: { key: ++this.nextKey }, previous, message }
                    })
                    const anchorBefore = options.anchorBefore && start > 0
                    const anchorIndex = anchorBefore ? start - 1 : start
                    const anchor = !targets.length && !options.head && anchorIndex >= 0 && anchorIndex < this.count
                        ? this.targetAt(anchorIndex, options.anchor) : undefined
                    this.entries.push({
                        sequence: sequence(), before: targets, after: outputs, tail: options.tail,
                        anchor, anchorBefore, head: options.head,
                    })
                }
            }
            this.sessionVersion = sessionVersion
            this.refresh(!sameIds)
            return rollback
        } catch (error) {
            rollback()
            throw error
        }
    }

    freeze(): WindowedOverlayPrefix {
        const prefix = {
            version: this.sessionVersion, through: this.sequence, dirty: this.dirty, entries: this.entries.slice(),
        }
        this.freezes.add(prefix)
        return prefix
    }

    abandon(prefix: WindowedOverlayPrefix) {
        this.freezes.delete(prefix)
        if (!this.freezes.size) {
            const producers = new Map<Target, number>()
            const compact: Edit[] = []
            for (const entry of this.entries) {
                const target = entry.before[0]
                const producer = producers.get(target)
                if (producer !== undefined && entry.before.length === 1 && entry.after.length === 1 && entry.after[0].target === target) {
                    const original = compact[producer]
                    compact[producer] = { ...original, after: original.after.map((output) =>
                        output.target === target ? { ...output, message: entry.after[0].message } : output) }
                } else {
                    for (const output of entry.after) producers.set(output.target, compact.length)
                    compact.push(entry)
                }
            }
            this.entries = compact
        }
        this.prune()
    }

    private prune() {
        const keep = new Set(this.held.keys())
        const visit = (edit: Edit) => {
            for (const target of edit.before) keep.add(target)
            if (edit.anchor) keep.add(edit.anchor)
            for (const { previous, target } of edit.after) {
                keep.add(target)
                if (previous) keep.add(previous)
            }
        }
        for (const entry of this.entries) visit(entry)
        for (const prefix of this.freezes) for (const entry of prefix.entries) visit(entry)
        for (const target of this.bindings.keys()) if (!keep.has(target)) this.bindings.delete(target)
        for (const target of this.unresolved.keys()) if (!keep.has(target)) this.unresolved.delete(target)
    }

    private async prepare(reader: Reader, count: number, entries: readonly Edit[]) {
        const cache = new Map<string, Promise<Occurrence>>()
        const lookup = (id: string) => {
            let result = cache.get(id)
            if (!result) {
                result = this.occurrence(reader, id)
                cache.set(id, result)
            }
            return result
        }
        const bindings = new Map<Target, Binding>()
        const visited = new Set<Target>()
        const known = this.revision === reader.revision
        // A chunk can introduce another target while the first lookup is pending.
        const collect = async () => {
            for (const [target, binding] of this.bindings) {
                if (visited.has(target)) continue
                visited.add(target)
                if (known) {
                    if (binding.unknownId) {
                        const window = await reader.readConversationWindow({
                            characterId: this.characterId, conversationId: this.conversationId, startIndex: binding.index, limit: 1,
                        })
                        if (window && window.revision !== reader.revision) throw new RevisionConflictError(reader.revision, window.revision)
                        if (!window || window.value.characterId !== this.characterId || window.value.conversationId !== this.conversationId ||
                            window.value.startIndex !== binding.index || window.value.messages.length !== 1) {
                            throw new Error('Windowed conversation insertion anchor is missing from its captured revision')
                        }
                        const id = window.value.messages[0].chatId
                        binding.id = usableId(id) ? id : undefined
                        binding.unknownId = false
                    }
                    if (binding.id && !binding.unique && !binding.checked) {
                        const hit = await lookup(binding.id)
                        if (reader.revision === this.revision && this.bindings.get(target) === binding) {
                            binding.unique = hit.kind === 'unique' && hit.index === binding.index
                            binding.checked = true
                        }
                    }
                    bindings.set(target, binding)
                }
                else if (binding.unique && binding.id) {
                    const hit = await lookup(binding.id)
                    if (hit.kind === 'unique') bindings.set(target, {
                        id: binding.id, index: hit.index, unique: true, body: this.held.has(target) ? hit.message : undefined,
                    })
                }
            }
            if (!known) for (const [target, id] of this.unresolved) {
                if (visited.has(target)) continue
                visited.add(target)
                const hit = await lookup(id)
                if (hit.kind === 'unique') bindings.set(target, {
                    index: hit.index, id, unique: true, body: this.held.has(target) ? hit.message : undefined,
                })
            }
        }
        const occurrences = new Map<string, Occurrence>()
        const collectIntroduced = async (edits: readonly Edit[]) => {
            for (const edit of edits) for (const { message, previous, target } of edit.after) {
                const id = message.chatId
                if (!id || occurrences.has(id)) continue
                // An ordinary body update to a stored or pending target needs no lookup.
                if (!edit.append && (target === edit.before[0] || previous && this.bindings.get(previous)?.id === id)) continue
                const installed = known ? this.collisionProofs.get(id) : undefined
                occurrences.set(id, installed ?? await lookup(id))
            }
        }
        await collectIntroduced(entries)
        await collect()
        return { bindings, occurrences, collect, collectIntroduced,
            evaluate: (edits: readonly Edit[]) => evaluate(count, bindings, edits, occurrences) }
    }

    async plan(prefix: WindowedOverlayPrefix, reader: Reader, storedCount: number): Promise<WindowedOverlayPlan> {
        if (!this.accepting || !this.freezes.has(prefix)) throw new Error('Windowed save prefix is no longer owned')
        const entries = prefix.entries
        const prepared = await this.prepare(reader, storedCount, entries)
        const evaluation = prepared.evaluate(entries)
        return {
            revision: reader.revision, prefix, storedCount, evaluation, bindings: prepared.bindings,
            occurrences: prepared.occurrences,
            mutations: evaluation.view.mutations(storedCount, this.characterId, this.conversationId),
        }
    }

    async rebase(reader: Reader, storedCount: number): Promise<boolean> {
        if (!this.accepting || reader.revision < this.revision || reader.revision < this.preparingRevision) return false
        if (reader.revision === this.revision) return this.storedCount === storedCount
        this.preparingRevision = reader.revision
        const generation = ++this.generation
        const prepared = await this.prepare(reader, storedCount, this.entries)
        let version: number
        do {
            version = this.version
            await prepared.collectIntroduced(this.entries)
            await prepared.collect()
            if (!this.accepting || generation !== this.generation || reader.revision < this.revision) return false
        } while (this.version !== version)
        const evaluated = prepared.evaluate(this.entries)
        if (this.freezes.size) {
            for (const [target, binding] of this.bindings) {
                if (!prepared.bindings.has(target) && binding.id && binding.unique) this.unresolved.set(target, binding.id)
            }
            this.hidden = new Set(this.entries.filter((entry) => !evaluated.kept.has(entry.sequence)).map((entry) => entry.sequence))
        } else {
            this.unresolved.clear()
            this.hidden.clear()
            this.entries = this.entries.filter((entry) => evaluated.kept.has(entry.sequence))
        }
        this.bindings = prepared.bindings
        this.collisionProofs = prepared.occurrences
        this.revision = reader.revision
        this.storedCount = storedCount
        this.refresh(true)
        this.prune()
        return true
    }

    advanceUntouched(expectedRevision: number, revision: number): boolean {
        if (!this.accepting || this.revision !== expectedRevision || revision < expectedRevision || revision < this.preparingRevision) return false
        this.generation++
        this.revision = revision
        return true
    }

    advanceRewritten(expectedRevision: number, revision: number,
        rewrites: readonly { start: number; deleteCount: number; messages: readonly Message[] }[]): boolean {
        if (!this.accepting || expectedRevision !== this.revision || revision < expectedRevision) return false
        const originals = new Map<number, Binding>()
        for (const binding of this.bindings.values()) originals.set(binding.index, binding)
        const updates = new Map<number, Message>()
        for (const range of rewrites) {
            if (range.deleteCount !== range.messages.length) return false
            for (let i = 0; i < range.deleteCount; i++) {
                const index = range.start + i
                const original = originals.get(index)
                if (!original || original.unknownId || original.id !== range.messages[i].chatId) return false
                updates.set(index, range.messages[i])
            }
        }
        if (!this.advanceUntouched(expectedRevision, revision)) return false
        for (const [target, binding] of this.bindings) {
            const message = updates.get(binding.index)
            if (message && this.held.has(target)) binding.body = safeStructuredClone(message)
        }
        this.version++
        return true
    }

    acknowledge(plan: WindowedOverlayPlan, durableRevision: number): void {
        if (!this.freezes.delete(plan.prefix) || !this.accepting) return
        if (durableRevision < this.revision || durableRevision < plan.revision) {
            throw new Error('Windowed acknowledgement precedes the installed revision')
        }
        const { view } = plan.evaluation
        const base = plan.bindings
        // A synchronous command may have bound a previously untouched stored row during the write.
        if (plan.revision === this.revision) {
            for (const [target, binding] of this.bindings) if (!base.has(target)) base.set(target, binding)
        }
        const targets = new Set([...base.keys(), ...this.held.keys()])
        for (const entry of plan.prefix.entries) for (const output of entry.after) targets.add(output.target)
        const next = new Map<Target, Binding>()
        const dirtyIds = new Map<string, number[]>()
        for (const interval of view.intervals(0, view.count)) {
            if ('messages' in interval) interval.messages.forEach((message, i) => {
                if (!message.chatId) return
                const indexes = dirtyIds.get(message.chatId) ?? []
                indexes.push(interval.start + i)
                dirtyIds.set(message.chatId, indexes)
            })
        }
        for (const target of targets) {
            const index = view.locate(target)
            if (index === null) continue
            const slot = view.at(index)!
            const old = base.get(target)
            const body = 'output' in slot ? slot.output.message : old?.body
            const id = 'output' in slot ? slot.output.message.chatId : old?.id
            const changed = id ? dirtyIds.get(id) : undefined
            let unique = old?.id === id && old?.unique === true
            if (id && changed) {
                const proof = plan.occurrences.get(id)
                const previous = 'output' in slot ? base.get(slot.output.previous ?? slot.output.target) : undefined
                unique = changed.length === 1 && changed[0] === index &&
                    (unique || previous?.unique === true && previous.id === id ||
                        proof?.kind === 'missing' || proof?.kind === 'unique' && view.storedToVisible(proof.index) === null)
            }
            next.set(target, { index, id, unique, body: this.held.has(target) ? body : undefined })
        }
        this.bindings = next
        this.collisionProofs.clear()
        this.persistedVersion = plan.prefix.version
        this.entries = this.entries.filter((entry) => entry.sequence > plan.prefix.through && !this.hidden.has(entry.sequence))
        this.unresolved.clear()
        this.hidden.clear()
        const unknown = this.revision !== plan.revision
        this.revision = durableRevision
        this.storedCount = view.count
        this.generation++
        this.refresh(unknown)
        this.prune()
    }

    retire() {
        this.accepting = false
        this.generation++
        this.bindings.clear()
        this.entries = []
        this.held.clear()
        this.freezes.clear()
        this.unresolved.clear()
        this.hidden.clear()
        this.storedCount = 0
        this.collisionProofs.clear()
        this.refresh(true)
    }
}

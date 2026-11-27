export type PluginReadKind = 'database' | 'character' | 'conversation' | 'preset'

export interface PluginReadProvenance {
    path: Array<string | number>
    token: string
}

export type PluginSnapshotChunk =
    | { type: 'provenance'; entries: PluginReadProvenance[] }
    | { type: 'set'; key: string; value: unknown }
    | { type: 'arrayStart'; key: string }
    | { type: 'arrayPush'; key: string; value: unknown }
    | { type: 'recordStart'; key: string }
    | { type: 'recordSet'; key: string; entryKey: string; value: unknown }
    | { type: 'characterStart'; key: 'characters'; value: unknown }
    | { type: 'conversationStart'; key: 'characters'; value: unknown }
    | { type: 'message'; key: 'characters'; value: unknown }

interface DigestCost { bytes: number }

function accountValue(value: unknown, cost?: DigestCost): void {
    if (cost) cost.bytes += typeof value === 'string' ? 16 + value.length * 2 : 16
}

const provenance = new WeakMap<object, string>()

export function attachPluginReadProvenance(value: unknown, entries: readonly PluginReadProvenance[]): void {
    for (const entry of entries) {
        let object: any = value
        for (const key of entry.path) object = object?.[key]
        if (object && typeof object === 'object') provenance.set(object, entry.token)
    }
}

export function collectPluginReadProvenance(value: unknown): PluginReadProvenance[] {
    const entries: PluginReadProvenance[] = []
    const seen = new WeakSet<object>()
    const visit = (object: unknown, path: Array<string | number>) => {
        if (!object || typeof object !== 'object' || seen.has(object)) return
        seen.add(object)
        const token = provenance.get(object)
        if (token) entries.push({ path, token })
        // Typed array elements are never objects, so enumerating them only costs time and memory.
        if (ArrayBuffer.isView(object)) return
        for (const [key, child] of Object.entries(object)) {
            visit(child, [...path, Array.isArray(object) ? Number(key) : key])
        }
    }
    visit(value, [])
    return entries
}

function canonical(value: unknown): string {
    if (value === undefined) return 'undefined'
    if (Array.isArray(value)) return '[' + value.map(canonical).join(',') + ']'
    if (value && typeof value === 'object') {
        return '{' + Object.keys(value).sort().map(key => JSON.stringify(key) + ':' + canonical((value as any)[key])).join(',') + '}'
    }
    return JSON.stringify(value)
}

export interface PluginFieldIntent {
    path: Array<string | number>
    type: 'set' | 'delete'
    value?: unknown
}

// Record arrays merge by identity; other arrays remain a single field intent.
function recordIdentityOf(field: string | number | undefined): string | undefined {
    if (field === 'characters') return 'chaId'
    if (field === 'plugins') return 'name'
    if (['chats', 'botPresets', 'personas', 'modules', 'loadouts', 'customModels'].includes(String(field))) return 'id'
    return undefined
}

function recordIdentity(path: readonly (string | number)[]): string | undefined {
    return recordIdentityOf(path.at(-1))
}

const wholeRecordFields = ['plugins', 'modules', 'loadouts', 'customModels']
const databaseRootObjects = ['pluginCustomStorage', 'globalChatVariables']

// Baselines keep a 53-bit digest in place of content. Two values share a digest
// exactly when `canonical` renders them alike, barring a hash collision.
let lane1 = 0
let lane2 = 0

function begin(tag: number): void {
    lane1 = 0xdeadbeef ^ tag
    lane2 = 0x41c6ce57 ^ tag
}

function mix(word: number): void {
    lane1 = Math.imul(lane1 ^ word, 2654435761)
    lane2 = Math.imul(lane2 ^ word, 1597334677)
}

function mixText(text: string): void {
    mix(text.length)
    for (let index = 0; index < text.length; index++) mix(text.charCodeAt(index))
}

function mixDigest(digest: number): void {
    mix(digest >>> 0)
    mix(Math.floor(digest / 4294967296))
}

function end(): number {
    let first = Math.imul(lane1 ^ (lane1 >>> 16), 2246822507)
    first ^= Math.imul(lane2 ^ (lane2 >>> 13), 3266489909)
    let second = Math.imul(lane2 ^ (lane2 >>> 16), 2246822507)
    second ^= Math.imul(first ^ (first >>> 13), 3266489909)
    return 4294967296 * (2097151 & second) + (first >>> 0)
}

const STRING = 1
const PRIMITIVE = 2
const OBJECT = 3
const ARRAY = 4

// Children are digested before their parent begins, so the shared lanes never interleave.
function objectDigest(keys: readonly string[], digests: ArrayLike<number>): number {
    const order = keys.map((_, index) => index).sort((a, b) => keys[a] < keys[b] ? -1 : keys[a] > keys[b] ? 1 : 0)
    begin(OBJECT)
    mix(keys.length)
    for (const index of order) {
        mixText(keys[index])
        mixDigest(digests[index])
    }
    return end()
}

function arrayDigest(digests: ArrayLike<number>): number {
    begin(ARRAY)
    mix(digests.length)
    for (let index = 0; index < digests.length; index++) mixDigest(digests[index])
    return end()
}

function digestValue(value: unknown, memo?: WeakMap<object, number>, cost?: DigestCost): number {
    accountValue(value, cost)
    if (typeof value === 'string') {
        begin(STRING)
        mixText(value)
        return end()
    }
    if (!value || typeof value !== 'object') {
        begin(PRIMITIVE)
        mixText((value === undefined ? undefined : JSON.stringify(value)) ?? 'undefined')
        return end()
    }
    const known = memo?.get(value)
    if (known !== undefined) return known
    let digest: number
    if (Array.isArray(value)) {
        const digests = new Float64Array(value.length)
        for (let index = 0; index < value.length; index++) digests[index] = digestValue(value[index], memo, cost)
        digest = arrayDigest(digests)
    } else {
        const keys = Object.keys(value)
        const digests = new Float64Array(keys.length)
        for (let index = 0; index < keys.length; index++) {
            accountValue(keys[index], cost)
            digests[index] = digestValue((value as Record<string, unknown>)[keys[index]], memo, cost)
        }
        digest = objectDigest(keys, digests)
    }
    memo?.set(value, digest)
    return digest
}

// A node keeps what the diff walks: field names, record identities, and the
// nodes of the fields it descends into. Everything else is a digest.
class ValueNode {
    constructor(readonly digest: number, readonly identity?: unknown) {}
}

export const pluginMessageDigest = (value: unknown): number => digestValue(value)
export const pluginMessageIdDigest = (id?: string): number => typeof id === 'string' && id.length > 0 ? digestValue(id) : -1

export interface PluginMessageFingerprints {
    positionKeys?: Map<number, number>
    ids: Float64Array
    digests: Float64Array
}

class MessageListNode extends ValueNode {
    constructor(readonly rows: PluginMessageFingerprints) {
        super(arrayDigest(rows.digests))
    }
}

const admissionPositions = new WeakMap<object, (index: number) => string | undefined>()
export function bindPluginAdmissionMessages(messages: object, keyAt: (index: number) => string | undefined): void {
    admissionPositions.set(messages, keyAt)
}

const messageBaselines = new WeakMap<object, PluginMessageFingerprints>()
export const getPluginMessageBaseline = (messages: object) => messageBaselines.get(messages)

export type PluginReadPositionProof = (conversation: string, index: number) => string | undefined

function bindReadPosition(node: BaselineNode, target: string, proof?: PluginReadPositionProof) {
    const list = node instanceof ObjectNode && node.children.get('message')
    if (!(list instanceof MessageListNode)) return
    bindFingerprintPositions(list.rows, proof && (index => proof(target, index)))
}

function bindFingerprintPositions(rows: PluginMessageFingerprints, keyAt?: (index: number) => string | undefined) {
    if (!keyAt) return
    const seen = new Set<number>(), duplicates = new Set<number>()
    for (const id of rows.ids) {
        if (seen.has(id)) duplicates.add(id)
        seen.add(id)
    }
    rows.ids.forEach((id, index) => {
        if (id >= 0 && !duplicates.has(id)) return
        const key = keyAt(index)
        if (key !== undefined) (rows.positionKeys ??= new Map()).set(index, digestValue(key))
    })
}

class ObjectNode {
    constructor(
        readonly digest: number,
        readonly keys: readonly string[],
        readonly digests: Float64Array,
        readonly children: ReadonlyMap<string, BaselineNode>,
        readonly identity?: unknown,
    ) {}
}

class RecordsNode {
    constructor(readonly digest: number, readonly items: readonly BaselineNode[], readonly identity?: unknown) {}
}

type BaselineNode = ValueNode | ObjectNode | RecordsNode

// An object identity never matches a submitted record, whose identity is a clone.
const identityValue = (value: unknown) => value !== null && (typeof value === 'object' || typeof value === 'function') ? {} : value

const descendsInto = (key: string, databaseRoot: boolean) => recordIdentityOf(key) !== undefined || (databaseRoot && databaseRootObjects.includes(key))

function objectNode(keys: readonly string[], nodes: readonly BaselineNode[], databaseRoot: boolean, identity?: unknown): ObjectNode {
    const digests = new Float64Array(keys.length)
    const children = new Map<string, BaselineNode>()
    keys.forEach((key, index) => {
        digests[index] = nodes[index].digest
        if (nodes[index] instanceof MessageListNode || descendsInto(key, databaseRoot)) children.set(key, nodes[index])
    })
    return new ObjectNode(objectDigest(keys, digests), keys, digests, children, identity)
}

function recordsNode(items: readonly BaselineNode[], identity?: unknown): RecordsNode {
    return new RecordsNode(arrayDigest(items.map(item => item.digest)), items, identity)
}

function fieldNode(value: unknown, field: string, databaseRoot: boolean, cost?: DigestCost): BaselineNode {
    if (field === 'message' && Array.isArray(value)) {
        accountValue(value, cost)
        const ids = new Float64Array(value.length), digests = new Float64Array(value.length)
        value.forEach((message, index) => {
            ids[index] = pluginMessageIdDigest(message?.chatId)
            digests[index] = digestValue(message, undefined, cost)
        })
        const rows = { ids, digests }
        const admission = admissionPositions.get(value)
        if (admission) bindFingerprintPositions(rows, admission)
        return new MessageListNode(rows)
    }
    return descendsInto(field, databaseRoot) ? buildNode(value, field, false, undefined, cost) : new ValueNode(digestValue(value, undefined, cost))
}

// `field` is the last path element, which selects the record identity of an array.
function buildNode(value: unknown, field: string | undefined, databaseRoot: boolean, identity?: unknown, cost?: DigestCost): BaselineNode {
    if (Array.isArray(value)) {
        const id = recordIdentityOf(field)
        if (!id) return new ValueNode(digestValue(value, undefined, cost), identity)
        accountValue(value, cost)
        const whole = wholeRecordFields.includes(field!)
        const items: BaselineNode[] = []
        for (let index = 0; index < value.length; index++) {
            const item = value[index]
            const itemIdentity = identityValue(item?.[id])
            items.push(whole
                ? new ValueNode(digestValue(item, undefined, cost), itemIdentity)
                : buildNode(item, typeof itemIdentity === 'string' ? itemIdentity : undefined, false, itemIdentity, cost))
        }
        return recordsNode(items, identity)
    }
    if (value && typeof value === 'object') {
        const keys = Object.keys(value)
        accountValue(value, cost)
        return objectNode(keys, keys.map(key => {
            accountValue(key, cost)
            return fieldNode((value as Record<string, unknown>)[key], key, databaseRoot, cost)
        }), databaseRoot, identity)
    }
    return new ValueNode(digestValue(value, undefined, cost), identity)
}

function withRecordField(node: ObjectNode, field: string, value: BaselineNode): ObjectNode {
    const position = node.keys.indexOf(field)
    const keys = position < 0 ? [...node.keys, field] : node.keys
    const digests = new Float64Array(keys.length)
    digests.set(node.digests)
    digests[position < 0 ? keys.length - 1 : position] = value.digest
    return new ObjectNode(objectDigest(keys, digests), keys, digests, new Map(node.children).set(field, value), node.identity)
}

function hasStringIdentities(records: readonly unknown[], id: string): boolean {
    for (let index = 0; index < records.length; index++) {
        if (typeof (records[index] as any)?.[id] !== 'string') return false
    }
    return true
}

const emptyRecords = recordsNode([])
const emptyObject = objectNode([], [], false)

function diffBaselineNode(baseline: BaselineNode, submitted: unknown, kind: PluginReadKind): PluginFieldIntent[] {
    const intents: PluginFieldIntent[] = []
    const memo = new WeakMap<object, number>()
    const digestOf = (value: unknown) => digestValue(value, memo)
    const visit = (before: BaselineNode, after: any, path: Array<string | number>, partial = false) => {
        if (before.digest === digestOf(after)) return
        const field = path.at(-1)
        const id = recordIdentityOf(field)
        if (id && before instanceof RecordsNode && Array.isArray(after)
            && before.items.every(record => typeof record.identity === 'string') && hasStringIdentities(after, id)) {
            const oldRecords = new Map<string, BaselineNode>()
            for (const record of before.items) oldRecords.set(record.identity as string, record)
            const newRecords = new Map<string, unknown>(after.map(record => [record[id], record]))
            for (const key of new Set([...oldRecords.keys(), ...newRecords.keys()])) {
                const recordPath = [...path, key]
                if (!newRecords.has(key)) intents.push({ path: recordPath, type: 'delete' })
                else if (!oldRecords.has(key)) intents.push({ path: recordPath, type: 'set', value: structuredClone(newRecords.get(key)) })
                else if (wholeRecordFields.includes(String(field))) {
                    if (oldRecords.get(key)!.digest !== digestOf(newRecords.get(key))) intents.push({ path: recordPath, type: 'set', value: structuredClone(newRecords.get(key)) })
                } else visit(oldRecords.get(key)!, newRecords.get(key), recordPath)
            }
            const order: string[] = after.map(record => record[id])
            if (order.length !== before.items.length || order.some((key, index) => key !== before.items[index].identity)) {
                intents.push({ path: [...path, '$order'], type: 'set', value: order })
            }
            return
        }
        if (before instanceof ObjectNode && after && typeof after === 'object' && !Array.isArray(after)) {
            const databaseRoot = kind === 'database' && path.length === 0
            const positions = new Map(before.keys.map((key, index) => [key, index]))
            for (const key of new Set([...before.keys, ...Object.keys(after)])) {
                const position = positions.get(key)
                if (!Object.hasOwn(after, key)) {
                    if (!partial) {
                        const child = before.children.get(key)
                        if (recordIdentityOf(key) && child instanceof RecordsNode) visit(child, [], [...path, key])
                        else intents.push({ path: [...path, key], type: 'delete' })
                    }
                } else if (position === undefined) {
                    if (recordIdentityOf(key) && Array.isArray(after[key])) visit(emptyRecords, after[key], [...path, key])
                    else if (databaseRoot && databaseRootObjects.includes(key)) visit(emptyObject, after[key], [...path, key])
                    else intents.push({ path: [...path, key], type: 'set', value: structuredClone(after[key]) })
                } else {
                    // Fields, metadata values and message lists are atomic units.
                    const child = before.children.get(key)
                    if (child) visit(child, after[key], [...path, key])
                    else if (before.digests[position] !== digestOf(after[key])) intents.push({ path: [...path, key], type: 'set', value: structuredClone(after[key]) })
                }
            }
            return
        }
        const value = structuredClone(after)
        if (before instanceof MessageListNode && Array.isArray(value)) messageBaselines.set(value, before.rows)
        intents.push({ path, type: 'set', value })
    }
    visit(baseline, submitted, [], kind === 'database')
    return intents.sort((a, b) => canonical(a.path) < canonical(b.path) ? -1 : canonical(a.path) > canonical(b.path) ? 1 : 0)
}

export function diffPluginReadBaseline(baseline: any, submitted: any, kind: PluginReadKind): PluginFieldIntent[] {
    return diffBaselineNode(buildNode(baseline, undefined, kind === 'database'), submitted, kind)
}

// Parents keep only child targets; a child's digests live once, in its own newest read.
type StoredValue =
    | { kind: 'conversation' | 'preset'; node: BaselineNode }
    | { kind: 'character'; detail: BaselineNode; chats?: string[] }
    | { kind: 'database'; fields: Map<string, { childKind: PluginReadKind; targets: unknown[] } | BaselineNode> }

interface PreparedRead {
    kind: PluginReadKind
    target: string
    path: Array<string | number>
    stored: StoredValue
}

/** Provisional digest-only state. No baseline is changed until finish succeeds. */
export class PluginSnapshotBaseline {
    private readonly reads: PreparedRead[] = []
    private readonly fields = new Map<string, { childKind: PluginReadKind; targets: unknown[] } | BaselineNode>()
    private readonly records = new Map<string, Map<string, BaselineNode>>()
    private readonly arrays = new Map<string, BaselineNode[]>()
    private character?: { target: string; index: number; chats: string[] }
    private conversation?: { read: PreparedRead; metadata: ObjectNode; ids: number[]; digests: number[] }
    private characterIndex = -1
    private finished = false

    constructor(
        private readonly kind: 'database' | 'character' | 'conversation',
        private readonly target: string,
        private readonly publish: (reads: PreparedRead[]) => PluginReadProvenance[],
    ) {
        if (kind === 'database') this.reads.push({ kind, target, path: [], stored: { kind, fields: this.fields } })
    }

    private finishConversation(): void {
        if (!this.conversation) return
        const { read, metadata, ids, digests } = this.conversation
        read.stored = { kind: 'conversation', node: withRecordField(metadata, 'message', new MessageListNode({ ids: Float64Array.from(ids), digests: Float64Array.from(digests) })) }
        this.conversation = undefined
    }

    add(chunk: PluginSnapshotChunk): number {
        if (this.finished) throw new Error('Plugin snapshot baseline is closed')
        const cost = { bytes: 64 }
        if (chunk.type === 'provenance') return cost.bytes
        accountValue(chunk.key, cost)
        if (chunk.type !== 'message') this.finishConversation()
        if (chunk.type === 'set') {
            this.fields.set(chunk.key, fieldNode(chunk.value, chunk.key, true, cost))
        } else if (chunk.type === 'arrayStart') {
            const nested = nestedRecords.find(([key]) => key === chunk.key)
            if (nested) this.fields.set(chunk.key, { childKind: nested[1], targets: [] })
            else this.arrays.set(chunk.key, [])
        } else if (chunk.type === 'arrayPush') {
            const field = this.fields.get(chunk.key)
            if (field && 'targets' in field) {
                // Characters stream through characterStart; only presets arrive whole.
                if (field.childKind !== 'preset') throw new Error('Invalid plugin snapshot record chunk')
                const value = chunk.value as any
                const target = value?.id
                this.reads.push({ kind: 'preset', target, path: [chunk.key, field.targets.length],
                    stored: { kind: 'preset', node: buildNode(value, undefined, false, identityValue(target), cost) } })
                field.targets.push(target)
            } else {
                const value = chunk.value as any
                const id = recordIdentityOf(chunk.key)
                const identity = identityValue(id ? value?.[id] : undefined)
                this.arrays.get(chunk.key)!.push(id && !wholeRecordFields.includes(chunk.key)
                    ? buildNode(value, typeof identity === 'string' ? identity : undefined, false, identity, cost)
                    : new ValueNode(digestValue(value, undefined, cost), identity))
            }
        } else if (chunk.type === 'recordStart') {
            this.records.set(chunk.key, new Map())
        } else if (chunk.type === 'recordSet') {
            accountValue(chunk.entryKey, cost)
            this.records.get(chunk.key)!.set(chunk.entryKey, fieldNode(chunk.value, chunk.entryKey, false, cost))
        } else if (chunk.type === 'characterStart') {
            const { chats, ...detail } = chunk.value as any
            const target = this.kind === 'character' ? this.target : detail.chaId
            this.character = { target, index: ++this.characterIndex, chats: [] }
            const node = buildNode(detail, undefined, false, identityValue(detail.chaId), cost)
            // The guest replaces the source's chats field; it still occupies transfer bytes.
            if (chats !== undefined) digestValue(chats, undefined, cost)
            if (this.kind !== 'conversation') {
                this.reads.push({ kind: 'character', target, path: ['characters', this.characterIndex],
                    stored: { kind: 'character', detail: node, chats: this.character.chats } })
                const field = this.fields.get('characters')
                if (field && 'targets' in field) field.targets.push(target)
            }
        } else if (chunk.type === 'conversationStart') {
            const value = chunk.value as any
            const character = this.character!
            const target = this.kind === 'conversation' ? this.target : JSON.stringify([character.target, value?.id])
            const read: PreparedRead = { kind: 'conversation', target,
                path: ['characters', character.index, 'chats', character.chats.length],
                stored: { kind: 'conversation', node: new ValueNode(0) } }
            character.chats.push(target)
            this.reads.push(read)
            const metadata = buildNode({ ...value, message: undefined }, undefined, false, identityValue(value?.id), cost) as ObjectNode
            if (value?.message !== undefined) digestValue(value.message, undefined, cost)
            this.conversation = { read, metadata, ids: [], digests: [] }
        } else if (chunk.type === 'message') {
            const id = (chunk.value as { chatId?: unknown })?.chatId
            this.conversation!.ids.push(pluginMessageIdDigest(typeof id === 'string' ? id : undefined))
            this.conversation!.digests.push(digestValue(chunk.value, undefined, cost))
        }
        return cost.bytes
    }

    finish(): PluginReadProvenance[] {
        if (this.finished) throw new Error('Plugin snapshot baseline is closed')
        this.finished = true
        this.finishConversation()
        for (const [key, fields] of this.records) {
            this.fields.set(key, objectNode([...fields.keys()], [...fields.values()], false))
        }
        for (const [key, items] of this.arrays) this.fields.set(key, recordIdentityOf(key)
            ? recordsNode(items) : new ValueNode(arrayDigest(items.map(item => item.digest))))
        return this.publish(this.reads)
    }
}

interface Baseline {
    token: string
    authority: number
    stored: StoredValue
}

const nestedRecords = [['characters', 'character', 'chaId'], ['botPresets', 'preset', 'id']] as const

export class PluginReadBaselineError extends Error {
    constructor(reason: 'stale' | 'target' | 'closed') {
        super(`Plugin whole-object write rejected: ${reason} read baseline`)
        this.name = 'PluginReadBaselineError'
    }
}

/**
 * Keeps the newest completed read of each target as digests. Database reads keep
 * each top-level key separately, so a read of fewer keys replaces only those keys.
 */
export class PluginReadBaselines {
    private readonly baselines = new Map<string, Baseline>()
    private readonly tokens = new Map<string, { kind: PluginReadKind; target: string }>()
    private readonly history = new Set<string>()
    private closed = false

    constructor(private readonly owner: string, private readonly authority: () => number) {}

    get retainedBaselineCount(): number { return this.baselines.size }

    expire(): void { this.baselines.clear(); this.tokens.clear() }
    close(): void { this.closed = true; this.expire() }

    assertOpen(): void {
        if (this.closed) throw new PluginReadBaselineError('closed')
    }

    hasRead(kind: PluginReadKind, target: string): boolean {
        return this.history.has(JSON.stringify([this.owner, kind, target]))
    }

    track<T>(value: T, kind: PluginReadKind, target: string, _revision: number, expectedAuthority = this.authority(), proof?: PluginReadPositionProof): T {
        if (this.closed) throw new PluginReadBaselineError('closed')
        if (expectedAuthority !== this.authority()) throw new PluginReadBaselineError('stale')
        this.store(value, kind, target, expectedAuthority, proof)
        return value
    }

    beginSnapshot(kind: 'database' | 'character' | 'conversation', target: string, expectedAuthority = this.authority(), proof?: PluginReadPositionProof): PluginSnapshotBaseline {
        return new PluginSnapshotBaseline(kind, target, reads => {
            this.assertOpen()
            if (expectedAuthority !== this.authority()) throw new PluginReadBaselineError('stale')
            return reads.map(read => {
                if (read.stored.kind === 'conversation') bindReadPosition(read.stored.node, read.target, proof)
                const baseline = this.ensure(read.kind, read.target, expectedAuthority)
                if (read.stored.kind === 'database' && baseline.stored.kind === 'database') {
                    for (const [key, value] of read.stored.fields) baseline.stored.fields.set(key, value)
                } else baseline.stored = read.stored
                return { path: read.path, token: baseline.token }
            })
        })
    }

    private ensure(kind: PluginReadKind, target: string, authority: number): Baseline {
        const key = JSON.stringify([kind, target])
        let baseline = this.baselines.get(key)
        if (!baseline || baseline.authority !== authority) {
            if (baseline) this.tokens.delete(baseline.token)
            baseline = { token: crypto.randomUUID(), authority, stored: { kind: 'database', fields: new Map() } }
            this.baselines.set(key, baseline)
            this.tokens.set(baseline.token, { kind, target })
        }
        this.history.add(JSON.stringify([this.owner, kind, target]))
        return baseline
    }

    private store(value: unknown, kind: PluginReadKind, target: string, authority: number, proof?: PluginReadPositionProof): void {
        const baseline = this.ensure(kind, target, authority)
        if (value && typeof value === 'object') provenance.set(value, baseline.token)
        const record = value as any
        if (kind === 'database') {
            if (baseline.stored.kind !== 'database') baseline.stored = { kind, fields: new Map() }
            const fields = baseline.stored.fields
            for (const field of record && typeof record === 'object' ? Object.keys(record) : []) {
                const nested = nestedRecords.find(([name]) => name === field)
                if (nested && Array.isArray(record[field])) {
                    const [, childKind, id] = nested
                    for (const child of record[field]) this.store(child, childKind, child?.[id], authority, proof)
                    fields.set(field, { childKind, targets: record[field].map((child: any) => child?.[id]) })
                } else fields.set(field, fieldNode(record[field], field, true))
            }
        } else if (kind === 'character' && record && typeof record === 'object' && Array.isArray(record.chats)) {
            const { chats, ...detail } = record
            const targets = chats.map((chat: any) => JSON.stringify([target, chat?.id]))
            chats.forEach((chat: unknown, index: number) => this.store(chat, 'conversation', targets[index], authority, proof))
            baseline.stored = { kind, detail: buildNode(detail, undefined, false, identityValue(record.chaId)), chats: targets }
        } else if (kind === 'character') baseline.stored = { kind, detail: buildNode(value, undefined, false, identityValue(record?.chaId)) }
        else {
            const node = buildNode(value, undefined, false, identityValue(record?.id))
            if (kind === 'conversation') bindReadPosition(node, target, proof)
            baseline.stored = { kind: kind as 'conversation' | 'preset', node }
        }
    }

    private current(kind: PluginReadKind, target: string): Baseline | undefined {
        const baseline = this.baselines.get(JSON.stringify([kind, target]))
        return baseline?.authority === this.authority() ? baseline : undefined
    }

    // Composes a read baseline from the current baselines of its parts.
    private compose(kind: PluginReadKind, target: string): BaselineNode | undefined {
        const stored = this.current(kind, target)?.stored
        if (!stored) return undefined
        const present = (node: BaselineNode | undefined): node is BaselineNode => node !== undefined
        if (stored.kind === 'character') {
            if (!stored.chats) return stored.detail
            return withRecordField(stored.detail as ObjectNode, 'chats', recordsNode(stored.chats.map(chat => this.compose('conversation', chat)).filter(present)))
        }
        if (stored.kind !== 'database') return stored.node
        const nodes = [...stored.fields.values()].map(entry => 'targets' in entry
            ? recordsNode(entry.targets.map(child => this.compose(entry.childKind, child as string)).filter(present))
            : entry)
        return objectNode([...stored.fields.keys()], nodes, true)
    }

    // Resolves the baseline of a submitted value: its token's target, or the newest read of the target.
    private resolve(value: unknown, kind: PluginReadKind, target: string, fallback: () => BaselineNode | undefined, staleWithoutRead: boolean): BaselineNode | undefined {
        const token = value && typeof value === 'object' ? provenance.get(value) : undefined
        if (token) {
            const read = this.tokens.get(token)
            const baseline = read && this.current(read.kind, read.target)
            if (!read || baseline?.token !== token) throw new PluginReadBaselineError('stale')
            if (read.kind !== kind || read.target !== target) throw new PluginReadBaselineError('target')
            return this.compose(kind, target)
        }
        if (this.current(kind, target)) return this.compose(kind, target)
        if (staleWithoutRead && this.hasRead(kind, target)) throw new PluginReadBaselineError('stale')
        return fallback()
    }

    private withNestedBaselines(baseline: BaselineNode | undefined, submitted: any, kind: PluginReadKind, target: string): BaselineNode | undefined {
        if (!(baseline instanceof ObjectNode) || !submitted || typeof submitted !== 'object') return baseline
        const overlay = (records: BaselineNode | undefined, children: unknown, childKind: PluginReadKind, id: string, childTarget: (child: any) => string) => {
            if (!Array.isArray(children)) return records
            const original = records instanceof RecordsNode ? records.items : undefined
            let items = original
            const positions = new Map<unknown, number>()
            items?.forEach((record, index) => { if (!positions.has(record.identity)) positions.set(record.identity, index) })
            for (const child of children) {
                const index = positions.get(child?.[id]) ?? -1
                const previous = index >= 0 ? items![index] : undefined
                const target = childTarget(child)
                const next = this.withNestedBaselines(this.resolve(child, childKind, target, () => previous, true), child, childKind, target)
                if (next === undefined || next === previous) continue
                if (items === original) items = [...(items ?? [])]
                if (index >= 0) (items as BaselineNode[])[index] = next
                else positions.set(child?.[id], (items as BaselineNode[]).push(next) - 1)
            }
            return items === original ? records : recordsNode(items!, records?.identity)
        }
        let result = baseline
        const replace = (field: string, value: BaselineNode | undefined) => {
            if (value === undefined || value === result.children.get(field)) return
            result = withRecordField(result, field, value)
        }
        if (kind === 'database') {
            for (const [field, childKind, id] of nestedRecords) {
                replace(field, overlay(result.children.get(field), submitted[field], childKind, id, child => child?.[id]))
            }
        } else if (kind === 'character') {
            replace('chats', overlay(result.children.get('chats'), submitted.chats, 'conversation', 'id', chat => JSON.stringify([target, chat?.id])))
        }
        return result
    }

    intent(value: unknown, kind: PluginReadKind, target: string, admission: unknown): PluginFieldIntent[] {
        if (this.closed) throw new PluginReadBaselineError('closed')
        const baseline = this.resolve(value, kind, target, () => buildNode(admission, undefined, kind === 'database'), true)
        return diffBaselineNode(this.withNestedBaselines(baseline, value, kind, target) ?? buildNode(undefined, undefined, false), value, kind)
    }
}

export function rebasePluginFieldIntents<T>(latest: T, intents: readonly PluginFieldIntent[]): T {
    const result: any = structuredClone(latest)
    for (const intent of intents) {
        let parent = result
        for (let index = 0; index < intent.path.length - 1; index++) {
            const key = intent.path[index]
            if (Array.isArray(parent)) {
                const identity = recordIdentity(intent.path.slice(0, index))
                parent = parent.find(record => record?.[identity!] === key)
            } else parent = parent?.[key]
            if (!parent) throw new PluginReadBaselineError('target')
        }
        const key = intent.path.at(-1)!
        if (Array.isArray(parent)) {
            const identity = recordIdentity(intent.path.slice(0, -1))!
            if (key === '$order') {
                const order = intent.value as string[]
                const ordered = order.map(id => parent.find(record => record[identity] === id)).filter(Boolean)
                parent.splice(0, parent.length, ...ordered, ...parent.filter(record => !order.includes(record[identity])))
            } else {
                const index = parent.findIndex(record => record[identity] === key)
                if (intent.type === 'delete') { if (index >= 0) parent.splice(index, 1) }
                else if (index >= 0) parent[index] = structuredClone(intent.value)
                else parent.push(structuredClone(intent.value))
            }
        } else if (intent.type === 'delete') delete parent[key]
        else Object.defineProperty(parent, key, { value: structuredClone(intent.value), enumerable: true, configurable: true, writable: true })
    }
    return result
}

import type { Database, botPreset, character, groupChat } from './database.svelte'
import type { PersistentRoot, PluginStorageMutation } from './persistentDataStore'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { UNOWNED_PLUGIN_OWNER } from '../plugins/pluginOwner'

function canonicalize(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(canonicalize)
    if (value && typeof value === 'object') {
        const result: Record<string, unknown> = {}
        for (const key of Object.keys(value).sort()) {
            const entry = (value as Record<string, unknown>)[key]
            if (entry !== undefined) {
                defineOwnEnumerableProperty(result, key, canonicalize(entry))
            }
        }
        return result
    }
    return value
}

export function canonicalJson(value: unknown): string {
    const serialized = JSON.stringify(canonicalize(value))
    if (serialized === undefined) {
        throw new TypeError('Canonical JSON value is not serializable')
    }
    return serialized
}

export function canonicalClone<T>(value: T): T {
    return JSON.parse(canonicalJson(value)) as T
}

export function pluginStorageJson(storage: Database['pluginCustomStorage']): string {
    const normalized: Database['pluginCustomStorage'] = {}
    for (const key of Object.keys(storage)) {
        const value = storage[key]
        if (value !== undefined) {
            defineOwnEnumerableProperty(normalized, key, canonicalize(value))
        }
    }
    return JSON.stringify(normalized)
}

export interface PluginStorageCapture {
    /** Owned encoded entries, or null when callable hooks require whole-object JSON. */
    readonly entries: readonly (readonly [key: string, json: string])[] | null
    readonly encodedStrings?: readonly (readonly [key: string, value: string, json: string])[]
    readonly json: string
    /** A fresh detached value on every read, so callers cannot mutate the cache. */
    readonly value: Database['pluginCustomStorage']
}

interface PluginStorageEncodedEntry {
    readonly key: string
    readonly primitive: unknown
    readonly reusable: boolean
    readonly keyJson: string
    readonly json: string | undefined
}

function containsCallable(value: unknown): boolean {
    if (typeof value === 'function') return true
    if (!value || typeof value !== 'object') return false
    return Object.keys(value).some((key) =>
        containsCallable((value as Record<string, unknown>)[key]),
    )
}

/**
 * Reuse encoded immutable entries without assuming that plugin objects are
 * reactive. Mutable values and getters are inspected on every capture.
 */
export class PluginStorageCaptureCache {
    private entries = new Map<string, PluginStorageEncodedEntry>()
    private encoded: readonly PluginStorageEncodedEntry[] | undefined
    private latest: PluginStorageCapture | undefined

    clear(): void {
        this.entries = new Map()
        this.encoded = undefined
        this.latest = undefined
    }

    capture(storage: Database['pluginCustomStorage']): PluginStorageCapture {
        const normalized: Record<string, unknown> = {}
        const primitives = new Map<string, unknown>()
        let hasCallable = false
        // Match pluginStorageJson's read/normalization order before serialization.
        for (const key of Object.keys(storage)) {
            const value = storage[key]
            if (value === undefined) continue
            const reusable =
                value === null || (typeof value !== 'object' && typeof value !== 'function')
            if (reusable) primitives.set(key, value)
            const canonical = reusable ? value : canonicalize(value)
            if (!reusable && containsCallable(canonical)) hasCallable = true
            defineOwnEnumerableProperty(normalized, key, canonical)
        }

        // Callable toJSON hooks can depend on their key, parent or sibling state.
        // Preserve full-object JSON semantics for these non-data values.
        if (hasCallable) {
            const json = JSON.stringify(normalized)
            this.entries = new Map()
            this.encoded = undefined
            if (this.latest?.entries === null && this.latest.json === json) return this.latest
            this.latest = this.snapshot(() => json, null)
            return this.latest
        }

        const nextEntries = new Map<string, PluginStorageEncodedEntry>()
        const encoded: PluginStorageEncodedEntry[] = []
        for (const key of Object.keys(normalized)) {
            const previous = this.entries.get(key)
            const reusable = primitives.has(key)
            const primitive = primitives.get(key)
            const entry =
                reusable && previous?.reusable && Object.is(previous.primitive, primitive)
                    ? previous
                    : {
                          key,
                          primitive,
                          reusable,
                          keyJson: previous?.keyJson ?? JSON.stringify(key),
                          json: JSON.stringify(normalized[key]),
                      }
            nextEntries.set(key, entry)
            if (entry.json !== undefined) encoded.push(entry)
        }
        const unchanged =
            this.latest !== undefined &&
            encoded.length === this.encoded?.length &&
            encoded.every(
                (entry, index) =>
                    entry.keyJson === this.encoded![index].keyJson &&
                    entry.json === this.encoded![index].json,
            )
        this.entries = nextEntries
        this.encoded = encoded
        if (unchanged) return this.latest!

        const bytes = encoded.map(({ keyJson, json }) => [keyJson, json] as const)
        const entries = Object.freeze(
            encoded.map(({ key, json }) => Object.freeze([key, json!] as const)),
        )
        this.latest = this.snapshot(
            () => `{${bytes.map(([keyJson, json]) => `${keyJson}:${json}`).join(',')}}`,
            entries,
            Object.freeze(
                encoded.flatMap(({ key, primitive, reusable, json }) =>
                    reusable && typeof primitive === 'string' && json !== undefined
                        ? [Object.freeze([key, primitive, json] as const)]
                        : [],
                ),
            ),
        )
        return this.latest
    }

    private snapshot(
        encode: () => string,
        entries: PluginStorageCapture['entries'],
        encodedStrings?: PluginStorageCapture['encodedStrings'],
    ): PluginStorageCapture {
        let json: string | undefined
        const read = () => (json ??= encode())
        return Object.freeze({
            entries,
            encodedStrings,
            get json() {
                return read()
            },
            get value() {
                return JSON.parse(read()) as Database['pluginCustomStorage']
            },
        })
    }
}

function pluginStorageClone(
    storage: Database['pluginCustomStorage'],
): Database['pluginCustomStorage'] {
    return JSON.parse(pluginStorageJson(storage)) as Database['pluginCustomStorage']
}

export function canonicalDatabaseClone(database: Database): Database {
    const includesPluginStorage = Object.prototype.hasOwnProperty.call(
        database,
        'pluginCustomStorage',
    )
    const orderedPluginStorage = includesPluginStorage
        ? pluginStorageClone(database.pluginCustomStorage ?? {})
        : null
    const cloned = canonicalClone(database)
    if (orderedPluginStorage !== null) cloned.pluginCustomStorage = orderedPluginStorage
    return cloned
}

function isPluginStorageArrayIndex(key: string): boolean {
    if (!/^(0|[1-9]\d*)$/.test(key)) return false
    const value = Number(key)
    return Number.isSafeInteger(value) && value >= 0 && value < 4_294_967_295
}

export function rebaseRootMutation(
    base: PersistentRoot,
    mutated: PersistentRoot,
    live: PersistentRoot,
): PersistentRoot {
    const rebased = canonicalClone(live) as PersistentRoot & Record<string, unknown>
    const baseRecord = base as PersistentRoot & Record<string, unknown>
    const mutatedRecord = mutated as PersistentRoot & Record<string, unknown>
    for (const key of new Set([...Object.keys(baseRecord), ...Object.keys(mutatedRecord)])) {
        if (canonicalJson({ value: baseRecord[key] }) === canonicalJson({ value: mutatedRecord[key] })) {
            continue
        }
        if (!Object.hasOwn(mutatedRecord, key) || mutatedRecord[key] === undefined) {
            delete rebased[key]
        } else {
            rebased[key] = canonicalClone(mutatedRecord[key])
        }
    }
    return rebased
}

function canonicalValuesEqual(left: unknown, right: unknown): boolean {
    return canonicalJson({ value: left }) === canonicalJson({ value: right })
}

export function messageReplaceRange<T>(
    baseline: readonly T[],
    current: readonly T[],
): { start: number; deleteCount: number; messages: T[] } {
    let start = 0
    const sharedLength = Math.min(baseline.length, current.length)
    while (start < sharedLength && canonicalValuesEqual(baseline[start], current[start])) {
        start++
    }

    let baselineEnd = baseline.length
    let currentEnd = current.length
    while (
        baselineEnd > start &&
        currentEnd > start &&
        canonicalValuesEqual(baseline[baselineEnd - 1], current[currentEnd - 1])
    ) {
        baselineEnd--
        currentEnd--
    }

    return {
        start,
        deleteCount: baselineEnd - start,
        messages: current.slice(start, currentEnd),
    }
}

function stableArrayEntryId(value: unknown): string | null {
    if (!value || typeof value !== 'object') return null
    const record = value as Record<string, unknown>
    if (typeof record.id === 'string' && record.id) return `id:${record.id}`
    if (typeof record.chaId === 'string' && record.chaId) return `chaId:${record.chaId}`
    return null
}

function stableArrayEntries(values: readonly unknown[]): Map<string, unknown> | null {
    const entries = new Map<string, unknown>()
    for (const value of values) {
        const id = stableArrayEntryId(value)
        if (!id || entries.has(id)) return null
        entries.set(id, value)
    }
    return entries
}

function namedArrayEntries(values: readonly unknown[]): Map<string, unknown> | null {
    const entries = new Map<string, unknown>()
    for (const value of values) {
        if (!value || typeof value !== 'object') return null
        const name = (value as Record<string, unknown>).name
        if (typeof name !== 'string' || !name || entries.has(name)) return null
        entries.set(name, value)
    }
    return entries
}

function sameIdSet(left: readonly string[], right: readonly string[]): boolean {
    return left.length === right.length && left.every((id) => right.includes(id))
}

function alignNamedEntriesByBasePosition(
    baseIds: readonly string[],
    values: readonly unknown[],
    entries: Map<string, unknown>,
): Map<string, unknown> | null {
    if (baseIds.length !== values.length) return null
    const valueIds = [...entries.keys()]
    const aligned = new Map<string, unknown>()
    let renamedCount = 0
    for (let index = 0; index < baseIds.length; index++) {
        const valueId = valueIds[index]
        if (valueId !== baseIds[index]) {
            if (baseIds.includes(valueId) || ++renamedCount > 1) return null
        }
        aligned.set(baseIds[index], values[index])
    }
    return aligned
}

function rebaseIdentifiedArray(
    baseEntries: Map<string, unknown>,
    liveEntries: Map<string, unknown>,
    candidateEntries: Map<string, unknown>,
): unknown[] {
    const baseIds = [...baseEntries.keys()]
    const liveIds = [...liveEntries.keys()]
    const candidateIds = [...candidateEntries.keys()]
    const liveStructureChanged = !canonicalValuesEqual(baseIds, liveIds)
    const resultIds = liveStructureChanged
        ? [
            ...liveIds.filter((id) =>
                candidateEntries.has(id) || !baseEntries.has(id)),
            ...candidateIds.filter((id) =>
                !baseEntries.has(id) && !liveEntries.has(id)),
        ]
        : candidateIds
    return resultIds.map((id) => {
        const candidateValue = candidateEntries.get(id)
        const liveValue = liveEntries.get(id)
        const baseValue = baseEntries.get(id)
        if (candidateValue === undefined) return canonicalClone(liveValue)
        if (liveValue === undefined || baseValue === undefined) {
            return canonicalClone(candidateValue)
        }
        return rebaseConcurrentLiveDelta(baseValue, liveValue, candidateValue)
    })
}

function exactArrayPermutation(
    base: readonly unknown[],
    values: readonly unknown[],
): number[] | null {
    if (base.length !== values.length) return null
    const baseIndexes = new Map<string, number>()
    for (let index = 0; index < base.length; index++) {
        const canonical = canonicalJson(base[index])
        if (baseIndexes.has(canonical)) return null
        baseIndexes.set(canonical, index)
    }
    const permutation: number[] = []
    for (const value of values) {
        const index = baseIndexes.get(canonicalJson(value))
        if (index === undefined || permutation.includes(index)) return null
        permutation.push(index)
    }
    return permutation
}

function permutationChanged(permutation: readonly number[] | null): permutation is number[] {
    return permutation !== null && permutation.some((value, index) => value !== index)
}

export function rebaseConcurrentLiveDelta<T>(base: T, live: T, candidate: T): T {
    if (canonicalValuesEqual(live, base)) return canonicalClone(candidate)
    if (canonicalValuesEqual(candidate, base)) return canonicalClone(live)
    if (Array.isArray(base) && Array.isArray(live) && Array.isArray(candidate)) {
        const baseEntries = stableArrayEntries(base)
        const liveEntries = stableArrayEntries(live)
        const candidateEntries = stableArrayEntries(candidate)
        if (baseEntries && liveEntries && candidateEntries) {
            return rebaseIdentifiedArray(
                baseEntries,
                liveEntries,
                candidateEntries,
            ) as T
        }
        const baseNamedEntries = namedArrayEntries(base)
        const liveNamedEntries = namedArrayEntries(live)
        const candidateNamedEntries = namedArrayEntries(candidate)
        if (baseNamedEntries && liveNamedEntries && candidateNamedEntries) {
            const baseIds = [...baseNamedEntries.keys()]
            let alignedLiveEntries = liveNamedEntries
            let alignedCandidateEntries = candidateNamedEntries
            let liveMembershipChanged = !sameIdSet(baseIds, [...liveNamedEntries.keys()])
            let candidateMembershipChanged = !sameIdSet(
                baseIds,
                [...candidateNamedEntries.keys()],
            )
            if (liveMembershipChanged && live.length === base.length) {
                const aligned = alignNamedEntriesByBasePosition(
                    baseIds,
                    live,
                    liveNamedEntries,
                )
                if (!aligned) throw new Error('Cannot safely identify renamed live array entries')
                alignedLiveEntries = aligned
                liveMembershipChanged = false
            }
            if (candidateMembershipChanged && candidate.length === base.length) {
                const aligned = alignNamedEntriesByBasePosition(
                    baseIds,
                    candidate,
                    candidateNamedEntries,
                )
                if (!aligned) {
                    throw new Error('Cannot safely identify renamed candidate array entries')
                }
                alignedCandidateEntries = aligned
                candidateMembershipChanged = false
            }
            if (
                (liveMembershipChanged && candidateMembershipChanged)
            ) {
                throw new Error('Cannot safely rebase concurrent named array changes')
            }
            return rebaseIdentifiedArray(
                baseNamedEntries,
                alignedLiveEntries,
                alignedCandidateEntries,
            ) as T
        }
        if (base.length === live.length && base.length === candidate.length) {
            const livePermutation = exactArrayPermutation(base, live)
            const candidatePermutation = exactArrayPermutation(base, candidate)
            const liveReordered = permutationChanged(livePermutation)
            const candidateReordered = permutationChanged(candidatePermutation)
            if (liveReordered || candidateReordered) {
                const resultOrder = liveReordered ? livePermutation : candidatePermutation!
                return resultOrder.map((baseIndex) => {
                    const liveIndex = liveReordered
                        ? livePermutation.indexOf(baseIndex)
                        : baseIndex
                    const candidateIndex = candidateReordered
                        ? candidatePermutation.indexOf(baseIndex)
                        : baseIndex
                    return rebaseConcurrentLiveDelta(
                        base[baseIndex],
                        live[liveIndex],
                        candidate[candidateIndex],
                    )
                }) as T
            }
            if (base.every((entry, index) =>
                canonicalValuesEqual(entry, live[index]) ||
                canonicalValuesEqual(entry, candidate[index]))) {
                return base.map((entry, index) => rebaseConcurrentLiveDelta(
                    entry,
                    live[index],
                    candidate[index],
                )) as T
            }
            throw new Error('Cannot safely rebase ambiguous positional array changes')
        }
        throw new Error('Cannot safely rebase concurrent structural array changes')
    }
    if (
        base && typeof base === 'object' &&
        live && typeof live === 'object' &&
        candidate && typeof candidate === 'object'
    ) {
        const baseRecord = base as Record<string, unknown>
        const liveRecord = live as Record<string, unknown>
        const candidateRecord = canonicalClone(candidate) as Record<string, unknown>
        for (const key of new Set([...Object.keys(baseRecord), ...Object.keys(liveRecord)])) {
            const baseHasKey = Object.hasOwn(baseRecord, key)
            const liveHasKey = Object.hasOwn(liveRecord, key)
            if (baseHasKey && !liveHasKey) {
                delete candidateRecord[key]
                continue
            }
            if (!liveHasKey) continue
            if (!baseHasKey) {
                defineOwnEnumerableProperty(
                    candidateRecord,
                    key,
                    canonicalClone(liveRecord[key]),
                )
                continue
            }
            if (!canonicalValuesEqual(baseRecord[key], liveRecord[key])) {
                defineOwnEnumerableProperty(
                    candidateRecord,
                    key,
                    Object.hasOwn(candidateRecord, key)
                        ? rebaseConcurrentLiveDelta(
                              baseRecord[key],
                              liveRecord[key],
                              candidateRecord[key],
                          )
                        : canonicalClone(liveRecord[key]),
                )
            }
        }
        return candidateRecord as T
    }
    return canonicalClone(live)
}

export function splitDatabase(database: Database): {
    root: PersistentRoot
    characters: Array<character | groupChat>
    presets: botPreset[]
    pluginStorage: Database['pluginCustomStorage']
} {
    const {
        characters,
        botPresets,
        pluginCustomStorage,
        pluginStorageMeta: _pluginStorageMeta,
        ...root
    } = database
    return {
        root,
        characters,
        presets: botPresets ?? [],
        pluginStorage: pluginCustomStorage ?? {},
    }
}

/**
 * The flat compatibility projection cannot express ownership, so a change that
 * reaches the store through it is attributed by the resolver. A key the resolver
 * cannot place lands on the sentinel owner, where the plugin data screen shows
 * it rather than handing it to a plugin that did not write it.
 */
export type PluginStorageOwnerResolver = (key: string) => string

const resolveUnowned: PluginStorageOwnerResolver = () => UNOWNED_PLUGIN_OWNER

export function diffPluginStorage(
    baseline: string | null,
    current: Database['pluginCustomStorage'],
    ownerOf: PluginStorageOwnerResolver = resolveUnowned,
): PluginStorageMutation[] {
    const previous = baseline
        ? JSON.parse(baseline) as Database['pluginCustomStorage']
        : {}
    const currentKeys = Object.keys(current)
    const mutations: PluginStorageMutation[] = []
    const previousKeys = Object.keys(previous)
    for (const key of previousKeys) {
        if (!Object.hasOwn(current, key))
            mutations.push({ type: 'delete', owner: ownerOf(key), key })
    }
    const previousStringKeys = previousKeys.filter((key) =>
        !isPluginStorageArrayIndex(key) && Object.hasOwn(current, key),
    )
    const currentStringKeys = currentKeys.filter((key) => !isPluginStorageArrayIndex(key))
    let previousPosition = 0
    let stablePrefixLength = 0
    for (const key of currentStringKeys) {
        if (!Object.hasOwn(previous, key)) break
        const position = previousStringKeys.indexOf(key, previousPosition)
        if (position < 0) break
        previousPosition = position + 1
        stablePrefixLength++
    }
    const movedKeys = new Set(
        currentStringKeys
            .slice(stablePrefixLength)
            .filter((key) => Object.hasOwn(previous, key)),
    )
    for (const key of previousKeys) {
        if (movedKeys.has(key)) mutations.push({ type: 'delete', owner: ownerOf(key), key })
    }
    for (const key of currentKeys) {
        if (
            movedKeys.has(key) ||
            !Object.hasOwn(previous, key) ||
            !canonicalValuesEqual(previous[key], current[key])
        ) {
            mutations.push({ type: 'set', owner: ownerOf(key), key, value: current[key] })
        }
    }
    return mutations
}

export function applyPluginStorageMutations(
    storage: Database['pluginCustomStorage'],
    mutations: readonly PluginStorageMutation[],
    ownerOf?: PluginStorageOwnerResolver,
): Database['pluginCustomStorage'] {
    const next = pluginStorageClone(storage)
    applyPluginStorageMutationsInPlace(next, mutations, ownerOf)
    return next
}

/** Detached canonical values, updated by key without rebuilding a large baseline. */
export class PluginStorageBaseline {
    private readonly entries = new Map<string, string>()
    private serialized: string | undefined

    constructor(source: string | PluginStorageCapture) {
        if (typeof source !== 'string' && source.entries !== null) {
            for (const [key, json] of source.entries) this.entries.set(key, json)
            return
        }
        const serialized = typeof source === 'string' ? source : source.json
        const storage = JSON.parse(serialized) as Record<string, unknown>
        for (const key of Object.keys(storage)) {
            this.entries.set(key, JSON.stringify(storage[key]))
        }
        this.serialized = serialized
    }

    apply(
        mutations: readonly PluginStorageMutation[],
        ownerOf?: PluginStorageOwnerResolver,
    ): void {
        for (const mutation of mutations) {
            if (mutation.type === 'clear') {
                if (!ownerOf) this.entries.clear()
                else {
                    for (const key of [...this.entries.keys()]) {
                        if (ownerOf(key) === mutation.owner) this.entries.delete(key)
                    }
                }
            }
            else if (mutation.type === 'delete') this.entries.delete(mutation.key)
            else if (mutation.value === undefined) this.entries.delete(mutation.key)
            else this.entries.set(mutation.key, canonicalJson(mutation.value))
        }
        this.serialized = undefined
    }

    matches(capture: PluginStorageCapture): boolean {
        if (capture.entries === null) return this.json === capture.json
        const keys = Object.keys(Object.fromEntries(this.entries))
        if (keys.length !== capture.entries.length) return false
        for (let index = 0; index < keys.length; index++) {
            const [key, json] = capture.entries[index]
            if (keys[index] !== key || this.entries.get(key) !== json) return false
        }
        // Share the owned strings after equality is established. Future captures
        // of large immutable entries then compare the same string references.
        for (const [key, json] of capture.entries) this.entries.set(key, json)
        return true
    }

    get json(): string {
        // Object.keys supplies JavaScript's integer-key ordering after insertions.
        return (this.serialized ??= `{${Object.keys(Object.fromEntries(this.entries))
            .map((key) => `${JSON.stringify(key)}:${this.entries.get(key)}`)
            .join(',')}}`)
    }
}

/** Svelte's original property order cannot be changed by delete/reinsert. */
export function orderPluginStorageKeys<T extends Record<string, unknown>>(
    storage: T,
    keys: readonly string[],
): T {
    const current = Object.keys(storage)
    if (
        current.length === keys.length &&
        current.every((key, index) => key === keys[index])
    )
        return storage
    const ordered = {} as T
    for (const key of keys) {
        if (Object.hasOwn(storage, key))
            defineOwnEnumerableProperty(ordered, key, storage[key])
    }
    return ordered
}

/** Applies detached values without cloning unrelated keys in a resident store. */
export function applyPluginStorageMutationsInPlace(
    next: Database['pluginCustomStorage'],
    mutations: readonly PluginStorageMutation[],
    ownerOf?: PluginStorageOwnerResolver,
): void {
    for (const mutation of mutations) {
        if (mutation.type === 'clear') {
            for (const key of Object.keys(next)) {
                if (ownerOf && ownerOf(key) !== mutation.owner) continue
                delete next[key]
            }
        } else if (mutation.type === 'delete') {
            delete next[mutation.key]
        } else if (mutation.value === undefined) {
            delete next[mutation.key]
        } else {
            const value = canonicalClone(mutation.value)
            // Assignment subscribes new keys and wraps nested values in Svelte.
            // Shadow inherited names first to avoid the __proto__ setter.
            if (!Object.hasOwn(next, mutation.key) && mutation.key in next) {
                defineOwnEnumerableProperty(next, mutation.key, value)
            }
            next[mutation.key] = value
        }
    }
}

export function rebaseConcurrentPluginStorage(
    base: Database['pluginCustomStorage'],
    live: Database['pluginCustomStorage'],
    candidate: Database['pluginCustomStorage'],
): Database['pluginCustomStorage'] {
    const rebased = rebaseConcurrentLiveDelta(base, live, candidate)
    const ordered = applyPluginStorageMutations(
        candidate,
        diffPluginStorage(pluginStorageJson(base), live),
    )
    const result: Database['pluginCustomStorage'] = {}
    for (const key of Object.keys(ordered)) {
        if (Object.hasOwn(rebased, key)) {
            defineOwnEnumerableProperty(result, key, rebased[key])
        }
    }
    for (const key of Object.keys(rebased)) {
        if (!Object.hasOwn(result, key)) {
            defineOwnEnumerableProperty(result, key, rebased[key])
        }
    }
    return result
}

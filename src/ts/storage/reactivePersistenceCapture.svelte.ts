import { untrack } from 'svelte'
import type { RootMutation } from './persistentDataStore'
import {
    canonicalJson,
    pluginStorageJson,
    type PluginStorageCapture,
} from './saveCoordinatorHelpers'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { diffRootMutations } from './rootMutation'

/** Production-only: read closures must expose the deeply reactive DBState working set. */
export interface PersistenceCanonicalCapture {
    root(): string
    diffRoot(before: string, after: string): RootMutation[]
    presets(): string | null
    character(): string | null
    pluginStorage(): PluginStorageCapture | null
}

// Applying $state to an existing deep state proxy preserves its identity.
// Raw/plain objects produce a new proxy and must be serialized afresh.
function isDeepState(value: object): boolean {
    const candidate = $state(value)
    return candidate === value
}

const volatileValue = Symbol('volatile persistence value')

function normalize(value: unknown, markVolatile: () => never): unknown {
    if (typeof value === 'function') markVolatile()
    if (!value || typeof value !== 'object') return value
    const prototype = Object.getPrototypeOf(value)
    if (prototype !== Object.prototype && prototype !== Array.prototype) markVolatile()
    if (!isDeepState(value)) markVolatile()
    if (Array.isArray(value)) {
        return value.map((entry, index) => {
            if (Object.getOwnPropertyDescriptor(value, String(index))?.get) markVolatile()
            return normalize(entry, markVolatile)
        })
    }
    const output: Record<string, unknown> = {}
    for (const key of Object.keys(value).sort()) {
        if (Object.getOwnPropertyDescriptor(value, key)?.get) markVolatile()
        const entry = (value as Record<string, unknown>)[key]
        if (entry !== undefined)
            defineOwnEnumerableProperty(output, key, normalize(entry, markVolatile))
    }
    return output
}

function fieldCapture(read: () => unknown) {
    const capture = () => {
        try {
            const value = normalize(read(), () => {
                throw volatileValue
            })
            return { json: JSON.stringify(value) as string | undefined, volatile: false }
        } catch (error) {
            if (error !== volatileValue) throw error
            return { json: undefined, volatile: true }
        }
    }
    const cached = $derived.by(capture)
    return () => cached
}

function objectCapture(
    read: () => Record<string, unknown> | null,
    omit: ReadonlySet<string>,
    ordered: boolean,
) {
    const fields = new Map<string, ReturnType<typeof fieldCapture>>()
    let previous:
        | { volatile: false; json: string; entries: readonly (readonly [string, string])[] }
        | undefined
    const snapshot = () => {
        const source = read()
        if (source === null) {
            fields.clear()
            previous = undefined
            return null
        }
        if (!isDeepState(source)) {
            fields.clear()
            previous = undefined
            return { volatile: true as const }
        }
        const keys = Object.keys(source).filter((key) => !omit.has(key))
        // Object JSON visits integer keys first, even after canonical sorting.
        const order: Record<string, boolean> = {}
        for (const key of ordered ? keys : keys.sort())
            defineOwnEnumerableProperty(order, key, true)
        const currentKeys = Object.keys(order)
        const active = new Set(currentKeys)
        for (const key of fields.keys()) if (!active.has(key)) fields.delete(key)
        const entries: Array<readonly [string, string]> = []
        let volatile = false
        for (const key of currentKeys) {
            let field = fields.get(key)
            if (!field) {
                field = untrack(() =>
                    fieldCapture(() => {
                        const value = read()
                        // Do not evaluate an external getter inside a cached derived.
                        if (value && Object.getOwnPropertyDescriptor(value, key)?.get)
                            throw volatileValue
                        return value?.[key]
                    }),
                )
                fields.set(key, field)
            }
            const captured = field()
            volatile ||= captured.volatile
            if (captured.json !== undefined) entries.push(Object.freeze([key, captured.json]))
        }
        if (volatile) return { volatile: true as const }
        if (
            previous &&
            entries.length === previous.entries.length &&
            entries.every(
                ([key, json], index) =>
                    key === previous!.entries[index][0] && json === previous!.entries[index][1],
            )
        )
            return previous
        previous = {
            volatile: false as const,
            json:
                '{' +
                entries.map(([key, json]) => JSON.stringify(key) + ':' + json).join(',') +
                '}',
            entries: Object.freeze(entries),
        }
        return previous
    }
    return () => {
        const captured = snapshot()
        if (!captured) return null
        if (captured.volatile === false) return captured
        // Plain objects, getters and callable hooks can change without a reactive
        // notification. Preserve their full-object JSON semantics on every read.
        const source = read()
        if (source === null) return null
        const value: Record<string, unknown> = {}
        for (const key of Object.keys(source))
            if (!omit.has(key)) defineOwnEnumerableProperty(value, key, source[key])
        return { json: ordered ? pluginStorageJson(value) : canonicalJson(value), entries: null }
    }
}

export function createPersistenceCanonicalCapture(read: {
    root(): object
    pluginStorage(): Record<string, unknown> | null
    presets(): unknown
    character(): unknown
}): PersistenceCanonicalCapture {
    const captureRoot = objectCapture(
        () => read.root() as Record<string, unknown>,
        new Set(['characters', 'botPresets', 'pluginCustomStorage']),
        false,
    )
    const captureStorage = objectCapture(read.pluginStorage, new Set(), true)
    const presets = fieldCapture(read.presets)
    const character = fieldCapture(read.character)
    const roots = new Map<string, ReadonlyMap<string, string>>()
    return {
        root() {
            const captured = captureRoot()!
            if (captured.entries && !roots.has(captured.json)) {
                roots.set(captured.json, new Map(captured.entries))
                if (roots.size > 2) roots.delete(roots.keys().next().value!)
            }
            return captured.json
        },
        diffRoot(before, after) {
            const previous = roots.get(before)
            const current = roots.get(after)
            if (!previous || !current)
                return diffRootMutations(JSON.parse(before), JSON.parse(after))
            const result: RootMutation[] = []
            for (const key of [...new Set([...previous.keys(), ...current.keys()])].sort()) {
                if (previous.get(key) === current.get(key)) continue
                const json = current.get(key)
                result.push(
                    json === undefined
                        ? { type: 'delete', key }
                        : { type: 'set', key, value: JSON.parse(json) },
                )
            }
            return result
        },
        presets: () => {
            const result = presets()
            const json = result.volatile ? canonicalJson(read.presets()) : result.json
            return json === 'null' || json === undefined ? null : json
        },
        character: () => {
            const result = character()
            const json = result.volatile ? canonicalJson(read.character()) : result.json
            return json === 'null' || json === undefined ? null : json
        },
        pluginStorage() {
            const captured = captureStorage()
            if (!captured) return null
            return {
                json: captured.json,
                entries: captured.entries,
                get value() {
                    return JSON.parse(captured.json)
                },
            }
        },
    }
}

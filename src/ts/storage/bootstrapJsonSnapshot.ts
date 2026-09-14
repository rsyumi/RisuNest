import { canonicalJson } from './saveCoordinatorHelpers'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'

type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue }
const requiresSerialization = Symbol('requiresSerialization')

function copy(value: unknown, sorted: boolean, ancestors: Set<object>): JsonValue | undefined {
    if (value === null) return null
    if (typeof value === 'string' || typeof value === 'boolean') return value
    if (typeof value === 'number') return Number.isFinite(value) ? value || 0 : null
    if (typeof value === 'undefined' || typeof value === 'symbol') return undefined
    if (typeof value !== 'object') throw requiresSerialization
    if (ancestors.has(value)) throw new TypeError('Circular bootstrap database')
    ancestors.add(value)
    try {
        if (Array.isArray(value)) {
            return Array.from(value, (entry) => copy(entry, sorted, ancestors) ?? null)
        }
        const result: { [key: string]: JsonValue } = {}
        const keys = Object.keys(value)
        if (sorted) keys.sort()
        for (const key of keys) {
            const entry = copy((value as Record<string, unknown>)[key], sorted, ancestors)
            if (entry !== undefined) defineOwnEnumerableProperty(result, key, entry)
        }
        return result
    } finally {
        ancestors.delete(value)
    }
}

/** Own the JSON structure while sharing immutable strings instead of encoding the whole DB. */
export function bootstrapJsonSnapshot(value: unknown): JsonValue {
    try {
        const result = copy(value, true, new Set())
        if (result !== undefined) return result
    } catch (error) {
        if (error !== requiresSerialization) throw error
    }
    // Callable JSON hooks retain the existing serializer's semantics.
    return JSON.parse(canonicalJson(value)) as JsonValue
}

export function cloneBootstrapSnapshot<T>(value: JsonValue): T {
    return copy(value, false, new Set()) as T
}

export function equalBootstrapSnapshots(left: JsonValue, right: JsonValue): boolean {
    if (left === right) return true
    if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object') {
        return false
    }
    if (Array.isArray(left) !== Array.isArray(right)) return false
    const leftKeys = Object.keys(left)
    const rightKeys = Object.keys(right)
    if (leftKeys.length !== rightKeys.length) return false
    return leftKeys.every(
        (key, index) =>
            key === rightKeys[index] &&
            equalBootstrapSnapshots(
                (left as Record<string, JsonValue>)[key],
                (right as Record<string, JsonValue>)[key],
            ),
    )
}

import type { PersistentRoot, RootMutation } from './persistentDataStore'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { canonicalClone, canonicalJson } from './saveCoordinatorHelpers'

const separateAreas = new Set([
    'characters',
    'botPresets',
    'pluginCustomStorage',
    'pluginStorageMeta',
])

export function diffRootMutations(before: PersistentRoot, after: PersistentRoot): RootMutation[] {
    const mutations: RootMutation[] = []
    for (const key of [...new Set([...Object.keys(before), ...Object.keys(after)])].sort()) {
        const previous = Object.hasOwn(before, key) ? before[key] : undefined
        const current = Object.hasOwn(after, key) ? after[key] : undefined
        if (Object.is(previous, current)) continue
        if (canonicalJson({ value: previous }) === canonicalJson({ value: current })) continue
        mutations.push(
            current === undefined
                ? { type: 'delete', key }
                : { type: 'set', key, value: canonicalClone(current) },
        )
    }
    return mutations
}

export function applyRootMutations(
    root: PersistentRoot,
    mutations: readonly RootMutation[],
): PersistentRoot {
    const result = canonicalClone(root)
    const keys = new Set<string>()
    for (const mutation of mutations) {
        if (!mutation || typeof mutation.key !== 'string' || separateAreas.has(mutation.key)) {
            throw new TypeError('Invalid persistent root mutation key')
        }
        if (keys.has(mutation.key)) throw new TypeError('Duplicate persistent root mutation key')
        keys.add(mutation.key)
        if (mutation.type === 'delete') delete result[mutation.key]
        else if (mutation.type === 'set') {
            if (mutation.value === undefined)
                throw new TypeError('Root mutation set requires a JSON value')
            defineOwnEnumerableProperty(result, mutation.key, canonicalClone(mutation.value))
        } else throw new TypeError('Invalid persistent root mutation type')
    }
    return result
}

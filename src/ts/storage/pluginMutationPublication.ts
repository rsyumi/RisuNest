import type { PluginStorageMutation } from './persistentDataStore'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import {
    applyPluginStorageMutations,
    canonicalClone,
    diffPluginStorage,
    pluginStorageJson,
    rebaseConcurrentPluginStorage,
} from './saveCoordinatorHelpers'

export interface PluginMutationScope {
    keys: string[]
    values: Record<string, unknown>
}

/** Observe key order separately, without walking unrelated values. */
export function capturePluginMutationScope(
    storage: Record<string, unknown>,
    mutations: readonly PluginStorageMutation[],
): PluginMutationScope {
    const keys = mutations.some((mutation) => mutation.type === 'clear')
        ? Object.keys(storage)
        : new Set(
              mutations.flatMap((mutation) =>
                  mutation.type === 'clear' ? [] : [mutation.key],
              ),
          )
    const captured: Record<string, unknown> = {}
    for (const key of keys) {
        if (Object.hasOwn(storage, key) && storage[key] !== undefined) {
            defineOwnEnumerableProperty(
                captured,
                key,
                canonicalClone(storage[key]),
            )
        }
    }
    return { keys: Object.keys(storage), values: captured }
}

export function rebasePluginMutationPublication(
    mutations: readonly PluginStorageMutation[],
    before: PluginMutationScope,
    after: PluginMutationScope,
): { mutations: PluginStorageMutation[]; keys: string[] } {
    const candidate = applyPluginStorageMutations(before.values, mutations)
    const rebased = rebaseConcurrentPluginStorage(
        before.values,
        after.values,
        candidate,
    )
    // Replay original operations to preserve clear/delete/reinsert order, then
    // apply only later live changes. Unaffected keys and object identities survive.
    const laterChanges = diffPluginStorage(
        pluginStorageJson(candidate),
        rebased,
    ).flatMap((mutation): PluginStorageMutation[] =>
        mutation.type === 'clear'
            ? Object.keys(candidate).map((key) => ({ type: 'delete', key }))
            : [mutation],
    )
    const publication = [...mutations, ...laterChanges]

    // Replay the same ordering delta as the full-store rebase using tiny markers.
    // Value changes matter only when they revive a key deleted by the commit.
    const beforeOrder = Object.fromEntries(before.keys.map((key) => [key, 0]))
    const afterOrder = Object.fromEntries(after.keys.map((key) => [key, 0]))
    for (const mutation of diffPluginStorage(
        pluginStorageJson(before.values),
        after.values,
    )) {
        if (
            mutation.type === 'set' &&
            Object.hasOwn(afterOrder, mutation.key)
        ) {
            defineOwnEnumerableProperty(afterOrder, mutation.key, 1)
        }
    }
    const keyMutations = (
        entries: readonly PluginStorageMutation[],
    ): PluginStorageMutation[] =>
        entries.map((entry) =>
            entry.type === 'set' ? { ...entry, value: 0 } : entry,
        )
    const ordered = applyPluginStorageMutations(
        applyPluginStorageMutations(beforeOrder, keyMutations(mutations)),
        diffPluginStorage(pluginStorageJson(beforeOrder), afterOrder),
    )
    const finalKeys = applyPluginStorageMutations(
        afterOrder,
        keyMutations(publication),
    )
    return {
        mutations: publication,
        keys: [
            ...new Set([...Object.keys(ordered), ...Object.keys(finalKeys)]),
        ].filter((key) => Object.hasOwn(finalKeys, key)),
    }
}

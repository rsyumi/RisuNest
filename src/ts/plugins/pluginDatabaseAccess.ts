import type { Database } from '../storage/database.svelte'
import type { PersistentReplacementOptions } from '../storage/saveCoordinator'
import { getPersistentDataStore } from '../storage/persistentDataStoreFactory'
import { isCatalogCharacterStub } from '../storage/workingSetCatalog'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    DataRevision,
    PersistentDataStore,
    PersistentRevisionLease,
    PluginStorageMutation,
} from '../storage/persistentDataStore'
import { RevisionConflictError } from '../storage/persistentDataStore'
import {
    assertPinnedRevision,
    iteratePinnedCharacters,
    iteratePinnedConversations,
} from '../storage/persistentRecordIterator'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import type { PluginCompatibilityProfile } from './pluginCompatibility'

export const PLUGIN_SUMMARY_QUERY_DEFAULT_LIMIT = 50
export const PLUGIN_SUMMARY_QUERY_MAX_LIMIT = 100
export const PLUGIN_MESSAGE_QUERY_DEFAULT_LIMIT = 128
export const PLUGIN_MESSAGE_QUERY_MAX_LIMIT = 128

export interface PluginCharacterQuery {
    search?: string
    order?: 'configured' | 'recent'
    trash?: boolean
    limit?: number
    cursor?: string
}

export interface PluginConversationQuery {
    characterId: string
    order?: 'configured' | 'recent'
    limit?: number
    cursor?: string
}

export interface PluginConversationMessageQuery {
    characterId: string
    conversationId: string
    limit?: number
    anchorMessageId?: string
    before?: number
    after?: number
}

export interface PluginDatabaseAccessDependencies {
    store: PersistentDataStore
    flushPendingData(reason: string): Promise<void>
    getCompatibilityDatabase(): Database
    getCompatibilityProfile(): PluginCompatibilityProfile
    getNavigationGeneration(): number
    applyCompatibilityDatabaseLite(database: Record<string, unknown>): void
    applyCompatibilityDatabase(database: Record<string, unknown>): Promise<void>
    readPluginStorageSnapshot(): Promise<Record<string, unknown>>
    mutatePluginStorage(mutations: readonly PluginStorageMutation[]): Promise<void>
    invalidatePluginStorage(): void
    materializeDatabaseSnapshot(reason: string): Promise<{
        database: Database
        revision: DataRevision
        mutationGeneration: number
    }>
    replacePersistentDatabase(
        database: Database,
        reason: string,
        options: PersistentReplacementOptions,
    ): Promise<void>
    prepareAuthoritativeDatabaseUpdate?(
        database: Record<string, unknown>,
    ): Promise<Record<string, unknown>>
    snapshot<T>(value: T): T
}

export interface PluginDatabaseAccess {
    queryCharacters(input?: PluginCharacterQuery): Promise<CharacterPage>
    queryConversations(input: PluginConversationQuery): Promise<ConversationPage>
    queryConversationMessages(
        input: PluginConversationMessageQuery,
    ): Promise<ConversationWindow | null>
    getDatabaseSnapshot(
        includeOnly: string[] | 'all',
        allowedKeys: readonly string[],
    ): Promise<Record<string, unknown>>
    setDatabaseLite(
        database: Record<string, unknown>,
        allowedKeys: readonly string[],
    ): void | Promise<void>
    setDatabase(
        database: Record<string, unknown>,
        allowedKeys: readonly string[],
    ): Promise<void>
}

const SCALABLE_CHARACTER_SET_ERROR =
    'Synchronous plugin character updates are unavailable in scalable-v3. Use async setDatabase() or maximum-compatibility.'
const STALE_DATABASE_SET_ERROR =
    'Plugin database update became stale because compatibility or navigation state changed.'
const DANGEROUS_DATABASE_KEYS = new Set(['__proto__', 'prototype', 'constructor'])

function positiveLimit(value: number | undefined, defaultValue: number, maximum: number): number {
    const limit = value ?? defaultValue
    if (!Number.isSafeInteger(limit) || limit <= 0) {
        throw new RangeError('Query limit must be a positive safe integer')
    }
    return Math.min(limit, maximum)
}

function requiredId(value: string, name: string): void {
    if (typeof value !== 'string' || value.trim().length === 0) {
        throw new RangeError(`${name} must be a nonempty string`)
    }
}

function nonnegativeWindow(value: number | undefined, name: string): number {
    const size = value ?? 0
    if (!Number.isSafeInteger(size) || size < 0) {
        throw new RangeError(`${name} must be a nonnegative safe integer`)
    }
    return size
}

function hasCharacterUpdate(database: Record<string, unknown>): boolean {
    return Object.prototype.hasOwnProperty.call(database, 'characters')
}

function pluginStorageMutations(
    update: Record<string, unknown>,
    allowedKeys: readonly string[],
): PluginStorageMutation[] {
    const allowedKeySet = new Set(allowedKeys)
    const hasExplicitStorage =
        allowedKeySet.has('pluginCustomStorage') &&
        Object.prototype.hasOwnProperty.call(update, 'pluginCustomStorage')
    const mutations: PluginStorageMutation[] = []
    if (hasExplicitStorage) {
        mutations.push({ type: 'clear' })
        const storage = { ...(update.pluginCustomStorage as Record<string, unknown>) }
        for (const key of Object.keys(update).filter((key) => !allowedKeySet.has(key)).sort()) {
            storage[key] = update[key]
        }
        for (const key of Object.keys(storage)) {
            mutations.push({ type: 'set', key, value: storage[key] })
        }
        return mutations
    }
    for (const key of Object.keys(update).filter((key) => !allowedKeySet.has(key)).sort()) {
        mutations.push({ type: 'set', key, value: update[key] })
    }
    return mutations
}

function compatibilityOnlyUpdate(
    update: Record<string, unknown>,
    allowedKeys: readonly string[],
): Record<string, unknown> {
    const allowedKeySet = new Set(allowedKeys)
    return Object.fromEntries(Object.entries(update).filter(([key]) =>
        key !== 'pluginCustomStorage' && allowedKeySet.has(key),
    ))
}

function isPlainRecord(value: unknown): value is Record<string, unknown> {
    if (value === null || typeof value !== 'object') return false
    const prototype = Object.getPrototypeOf(value)
    return prototype === Object.prototype || prototype === null
}

function validateSafeKeys(database: Record<string, unknown>): void {
    for (const key of Object.keys(database)) {
        if (DANGEROUS_DATABASE_KEYS.has(key)) {
            throw new TypeError(`Unsafe plugin database key: ${key}`)
        }
    }
}

export function validatePluginDatabaseUpdate(
    database: unknown,
): asserts database is Record<string, unknown> {
    if (!isPlainRecord(database)) {
        throw new TypeError('Plugin database update must be a plain record')
    }
    validateSafeKeys(database)
    if (Object.prototype.hasOwnProperty.call(database, 'pluginCustomStorage')) {
        if (!isPlainRecord(database.pluginCustomStorage)) {
            throw new TypeError('pluginCustomStorage must be a plain record')
        }
        validateSafeKeys(database.pluginCustomStorage)
    }
}

function validateCompleteCharacters(value: unknown): asserts value is Database['characters'] {
    if (!Array.isArray(value)) {
        throw new TypeError('Plugin database characters must be an array')
    }
    const characterIds = new Set<string>()
    for (const character of value) {
        if (!character || typeof character !== 'object') {
            throw new TypeError('Plugin database characters must contain complete characters')
        }
        const record = character as Record<string, unknown>
        if (typeof record.chaId !== 'string' || record.chaId.length === 0) {
            throw new TypeError('Plugin database characters must have nonempty character IDs')
        }
        if (isCatalogCharacterStub(character as Database['characters'][number])) {
            throw new TypeError('Plugin database characters cannot contain catalog working-set stubs')
        }
        if (characterIds.has(record.chaId)) {
            throw new TypeError(`Plugin database contains duplicate character ID ${record.chaId}`)
        }
        characterIds.add(record.chaId)
        if (!Array.isArray(record.chats)) {
            throw new TypeError(`Plugin database character ${record.chaId} is not fully hydrated`)
        }
        for (const conversation of record.chats) {
            if (
                !conversation ||
                typeof conversation !== 'object' ||
                !Array.isArray((conversation as Record<string, unknown>).message)
            ) {
                throw new TypeError(`Plugin database character ${record.chaId} is not fully hydrated`)
            }
        }
    }
}

export function applyPluginDatabaseUpdate(
    candidate: Database,
    update: Record<string, unknown>,
    allowedKeys: readonly string[],
): void {
    validatePluginDatabaseUpdate(update)
    const mutableCandidate = candidate as unknown as Record<string, unknown>
    const allowedKeySet = new Set(allowedKeys)
    const hasExplicitCustomStorage = Object.prototype.hasOwnProperty.call(
        update,
        'pluginCustomStorage',
    ) && allowedKeySet.has('pluginCustomStorage')
    const existingCustomStorage = candidate.pluginCustomStorage ?? {}
    if (!isPlainRecord(existingCustomStorage)) {
        throw new TypeError('Existing pluginCustomStorage must be a plain record')
    }
    const customStorage = hasExplicitCustomStorage
        ? { ...(update.pluginCustomStorage as Record<string, unknown>) }
        : { ...existingCustomStorage }

    for (const key of Object.keys(update).filter((key) => allowedKeySet.has(key)).sort()) {
        if (key !== 'pluginCustomStorage') mutableCandidate[key] = update[key]
    }
    for (const key of Object.keys(update).filter((key) => !allowedKeySet.has(key)).sort()) {
        customStorage[key] = update[key]
    }
    candidate.pluginCustomStorage = customStorage
}

export function createPluginDatabaseAccess(
    dependencies: PluginDatabaseAccessDependencies,
): PluginDatabaseAccess {
    let openPromise: Promise<void> | undefined
    const openStore = () => (openPromise ??= dependencies.store.open())
    const acquireCurrentRevisionReader = async (): Promise<PersistentRevisionLease> => {
        for (let attempt = 0; ; attempt++) {
            const rootRecord = await dependencies.store.readRoot()
            try {
                return await dependencies.store.acquireRevision(rootRecord.revision)
            } catch (error) {
                if (!(error instanceof RevisionConflictError) || attempt >= 2) throw error
            }
        }
    }
    const prepareQuery = async () => {
        await dependencies.flushPendingData('plugin-database-query')
        await openStore()
    }

    return {
        async queryCharacters(input = {}) {
            const limit = positiveLimit(
                input.limit,
                PLUGIN_SUMMARY_QUERY_DEFAULT_LIMIT,
                PLUGIN_SUMMARY_QUERY_MAX_LIMIT,
            )
            await prepareQuery()
            return dependencies.store.queryCharacters({
                search: input.search,
                order: input.order ?? 'configured',
                trash: input.trash ?? false,
                limit,
                cursor: input.cursor,
            })
        },

        async queryConversations(input) {
            requiredId(input.characterId, 'characterId')
            const limit = positiveLimit(
                input.limit,
                PLUGIN_SUMMARY_QUERY_DEFAULT_LIMIT,
                PLUGIN_SUMMARY_QUERY_MAX_LIMIT,
            )
            await prepareQuery()
            return dependencies.store.queryConversations({
                characterId: input.characterId,
                order: input.order ?? 'configured',
                limit,
                cursor: input.cursor,
            })
        },

        async queryConversationMessages(input) {
            requiredId(input.characterId, 'characterId')
            requiredId(input.conversationId, 'conversationId')

            const anchored = input.anchorMessageId !== undefined
            let query
            if (anchored) {
                requiredId(input.anchorMessageId!, 'anchorMessageId')
                if (input.limit !== undefined) {
                    throw new RangeError('Anchored message queries cannot include limit')
                }
                const before = nonnegativeWindow(input.before, 'before')
                const after = nonnegativeWindow(input.after, 'after')
                if (before + 1 + after > PLUGIN_MESSAGE_QUERY_MAX_LIMIT) {
                    throw new RangeError('Anchored message window exceeds the maximum size')
                }
                query = {
                    characterId: input.characterId,
                    conversationId: input.conversationId,
                    anchorMessageId: input.anchorMessageId,
                    before,
                    after,
                }
            } else {
                if (input.before !== undefined || input.after !== undefined) {
                    throw new RangeError('Message window offsets require anchorMessageId')
                }
                query = {
                    characterId: input.characterId,
                    conversationId: input.conversationId,
                    limit: positiveLimit(
                        input.limit,
                        PLUGIN_MESSAGE_QUERY_DEFAULT_LIMIT,
                        PLUGIN_MESSAGE_QUERY_MAX_LIMIT,
                    ),
                }
            }

            await prepareQuery()
            const result = await dependencies.store.readConversationWindow(query)
            return result?.value ?? null
        },

        async getDatabaseSnapshot(includeOnly, allowedKeys) {
            const requestedKeys =
                includeOnly === 'all'
                    ? [...allowedKeys]
                    : allowedKeys.filter((key) => includeOnly.includes(key))
            const needsCharacters = requestedKeys.includes('characters')
            if (!needsCharacters) {
                const compatibilityDatabase = dependencies.getCompatibilityDatabase()
                const result: Record<string, unknown> = {}
                for (const key of requestedKeys) {
                    if (
                        key === 'pluginCustomStorage' &&
                        dependencies.getCompatibilityProfile() === 'scalable-v3'
                    ) {
                        await dependencies.flushPendingData('plugin-storage-snapshot')
                        result[key] = await dependencies.readPluginStorageSnapshot()
                    } else {
                        result[key] = dependencies.snapshot(
                            (compatibilityDatabase as unknown as Record<string, unknown>)[key],
                        )
                    }
                }
                return result
            }
            const compatibilityProfile = dependencies.getCompatibilityProfile()
            if (compatibilityProfile === 'scalable-v3') {
                await dependencies.flushPendingData('plugin-full-database-snapshot')
                await openStore()
                const reader = await acquireCurrentRevisionReader()
                try {
                    const pinnedRoot = await reader.readRoot()
                    assertPinnedRevision(reader.revision, pinnedRoot.revision, 'Root')
                    const result: Record<string, unknown> = {}
                    for (const key of requestedKeys) {
                        if (key === 'characters') {
                            const characters: Database['characters'] = []
                            for await (const character of iteratePinnedCharacters(reader)) {
                                const chats: Database['characters'][number]['chats'] = []
                                for await (const conversation of iteratePinnedConversations(
                                    reader,
                                    character.summary.id,
                                )) {
                                    chats.push(conversation.value)
                                }
                                characters.push(dependencies.snapshot({
                                    ...character.detail,
                                    chats,
                                } as Database['characters'][number]))
                            }
                            result[key] = characters
                            continue
                        }
                        if (key === 'botPresets') {
                            const catalog = await reader.queryPresets()
                            assertPinnedRevision(
                                reader.revision,
                                catalog.revision,
                                'Preset catalog',
                            )
                            const presets: Database['botPresets'] = []
                            for (const summary of catalog.items) {
                                const preset = await reader.readPreset(summary.id)
                                if (!preset) throw new Error(`Missing preset ${summary.id}`)
                                assertPinnedRevision(
                                    reader.revision,
                                    preset.revision,
                                    `Preset ${summary.id}`,
                                )
                                presets.push(dependencies.snapshot(preset.value))
                            }
                            result[key] = presets
                            continue
                        }
                        if (key === 'pluginCustomStorage') {
                            const catalog = await reader.queryPluginStorage()
                            assertPinnedRevision(
                                reader.revision,
                                catalog.revision,
                                'Plugin storage catalog',
                            )
                            const storage: Record<string, unknown> = {}
                            for (const summary of catalog.items) {
                                const value = await reader.readPluginStorage(summary.key)
                                if (!value) {
                                    throw new Error(
                                        `Missing plugin storage value for ${summary.key}`,
                                    )
                                }
                                assertPinnedRevision(
                                    reader.revision,
                                    value.revision,
                                    `Plugin storage value ${summary.key}`,
                                )
                                defineOwnEnumerableProperty(
                                    storage,
                                    summary.key,
                                    dependencies.snapshot(value.value),
                                )
                            }
                            result[key] = storage
                            continue
                        }
                        result[key] = dependencies.snapshot(
                            (pinnedRoot.value as unknown as Record<string, unknown>)[key],
                        )
                    }
                    return result
                } finally {
                    await reader.release()
                }
            }

            const sourceDatabase = dependencies.snapshot(dependencies.getCompatibilityDatabase())
            const result: Record<string, unknown> = {}
            for (const key of requestedKeys) {
                const value = (sourceDatabase as unknown as Record<string, unknown>)[key]
                result[key] = dependencies.snapshot(value)
            }
            return result
        },

        setDatabaseLite(database, allowedKeys) {
            validatePluginDatabaseUpdate(database)
            if (
                dependencies.getCompatibilityProfile() === 'scalable-v3' &&
                hasCharacterUpdate(database)
            ) {
                throw new Error(SCALABLE_CHARACTER_SET_ERROR)
            }
            const prepared = dependencies.snapshot(database)
            if (dependencies.getCompatibilityProfile() !== 'scalable-v3') {
                dependencies.applyCompatibilityDatabaseLite(prepared)
                return
            }
            const compatibilityUpdate = compatibilityOnlyUpdate(prepared, allowedKeys)
            if (Object.keys(compatibilityUpdate).length > 0) {
                dependencies.applyCompatibilityDatabaseLite(compatibilityUpdate)
            }
            const mutations = pluginStorageMutations(prepared, allowedKeys)
            if (mutations.length > 0) return dependencies.mutatePluginStorage(mutations)
        },

        async setDatabase(database, allowedKeys) {
            validatePluginDatabaseUpdate(database)
            const initialProfile = dependencies.getCompatibilityProfile()
            const initialNavigationGeneration = dependencies.getNavigationGeneration()
            if (initialProfile === 'scalable-v3' && hasCharacterUpdate(database)) {
                validateCompleteCharacters(database.characters)
            }
            const detachedUpdate = dependencies.snapshot(database)
            const preparedUpdate = dependencies.prepareAuthoritativeDatabaseUpdate
                ? await dependencies.prepareAuthoritativeDatabaseUpdate(detachedUpdate)
                : detachedUpdate
            validatePluginDatabaseUpdate(preparedUpdate)
            if (
                dependencies.getCompatibilityProfile() !== initialProfile ||
                dependencies.getNavigationGeneration() !== initialNavigationGeneration
            ) {
                throw new Error(STALE_DATABASE_SET_ERROR)
            }
            if (initialProfile === 'maximum-compatibility') {
                await dependencies.applyCompatibilityDatabase(dependencies.snapshot(preparedUpdate))
                return
            }
            if (hasCharacterUpdate(preparedUpdate)) {
                validateCompleteCharacters(preparedUpdate.characters)
            }
            const compatibilityUpdate = compatibilityOnlyUpdate(preparedUpdate, allowedKeys)
            const storageMutations = pluginStorageMutations(preparedUpdate, allowedKeys)
            if (Object.keys(compatibilityUpdate).length === 0) {
                await dependencies.mutatePluginStorage(storageMutations)
                return
            }
            const materialized = await dependencies.materializeDatabaseSnapshot(
                'plugin-database-set',
            )
            if (
                dependencies.getCompatibilityProfile() !== initialProfile ||
                dependencies.getNavigationGeneration() !== initialNavigationGeneration
            ) {
                throw new Error(STALE_DATABASE_SET_ERROR)
            }
            const candidate = dependencies.snapshot(materialized.database)
            applyPluginDatabaseUpdate(
                candidate,
                dependencies.snapshot(preparedUpdate),
                allowedKeys,
            )
            await dependencies.replacePersistentDatabase(candidate, 'plugin-database-set', {
                authoritative: true,
                publishOfficial: true,
                expectedRevision: materialized.revision,
                expectedMutationGeneration: materialized.mutationGeneration,
            })
            dependencies.invalidatePluginStorage()
        },
    }
}

export function createProductionPluginDatabaseAccess(
    dependencies: Omit<PluginDatabaseAccessDependencies, 'store'>,
): PluginDatabaseAccess {
    return createPluginDatabaseAccess({
        ...dependencies,
        store: getPersistentDataStore(),
    })
}

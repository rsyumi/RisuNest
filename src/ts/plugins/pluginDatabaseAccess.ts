import type { Database } from '../storage/database.svelte'
import type {
    CharacterPage,
    ConversationPage,
    ConversationWindow,
    PersistentDataStore,
} from '../storage/persistentDataStore'

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
}

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

export function createPluginDatabaseAccess(
    dependencies: PluginDatabaseAccessDependencies,
): PluginDatabaseAccess {
    let openPromise: Promise<void> | undefined
    const openStore = () => (openPromise ??= dependencies.store.open())
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
            let materializedDatabase: Database | undefined
            if (needsCharacters) {
                await dependencies.flushPendingData('plugin-full-database-snapshot')
                await openStore()
                materializedDatabase = await dependencies.store.materializeDatabase()
            }

            const compatibilityDatabase = dependencies.getCompatibilityDatabase()
            const result: Record<string, unknown> = {}
            for (const key of requestedKeys) {
                const value =
                    key === 'characters'
                        ? materializedDatabase!.characters
                        : (compatibilityDatabase as unknown as Record<string, unknown>)[key]
                result[key] = dependencies.snapshot(value)
            }
            return result
        },
    }
}

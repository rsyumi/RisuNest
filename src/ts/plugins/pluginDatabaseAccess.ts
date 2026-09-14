import type { Chat, Database } from '../storage/database.svelte'
import type {
    PersistentCompleteCharacterMutation,
    PersistentReplacementOptions,
    PersistentScopedReplacementOptions,
} from '../storage/saveCoordinator'
import type {
    CompleteConversationLease,
    SelectedConversationTarget,
} from '../storage/activeWorkingSet.svelte'
import type { ActiveConversationSession } from '../storage/activeConversationSession'
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
import {
    acquireCurrentRevisionWithRetry,
    assertPinnedRevision,
    iteratePinnedCharacterSummaries,
    iteratePinnedCharacters,
    iteratePinnedConversations,
    withPersistentRevisionLease,
} from '../storage/persistentRecordIterator'
import { defineOwnEnumerableProperty } from '../storage/ownEnumerableProperty'
import { isConversationSummaryStub } from '../storage/conversationResidency'
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
    startIndex?: number
    limit?: number
    anchorMessageId?: string
    before?: number
    after?: number
    signal?: AbortSignal
}

export interface PluginConversationWindow extends ConversationWindow {
    revision: DataRevision
}

export type PluginCompleteCharacter = Database['characters'][number]

export interface PluginChatOutputProjectionInput {
    characterId: string
    conversationId: string
    liveCharacter: PluginCompleteCharacter
    liveConversation: Chat
}

export interface PluginChatOutputProjection {
    char: PluginCompleteCharacter
    chat: Chat
}

export type PluginChatOutputProjector = (
    input: PluginChatOutputProjectionInput,
) => Promise<PluginChatOutputProjection>

export interface PluginFullObjectCallContext {
    pluginName: string
    signal: AbortSignal
}

export interface PluginResolvedCharacterTarget {
    revision: DataRevision
    characterId: string
}

export interface PluginResolvedConversationTarget extends PluginResolvedCharacterTarget {
    conversationId: string
}

export type PluginIdentityReplacementOperation =
    | 'setCharacter'
    | 'setCharacterToIndex'
    | 'setChatToIndex'

export interface PluginIdentityReplacementDiagnostic {
    kind: 'plugin-identity-replacement-rejected'
    pluginName: string
    operation: PluginIdentityReplacementOperation
    targetId: string
    attemptedId: string
}

export class PluginIdentityReplacementRejectedError extends Error {
    readonly diagnostic: PluginIdentityReplacementDiagnostic

    constructor(diagnostic: PluginIdentityReplacementDiagnostic) {
        super(`${diagnostic.operation} cannot replace identity ${diagnostic.targetId}`)
        this.name = 'PluginIdentityReplacementRejectedError'
        this.diagnostic = diagnostic
    }
}

export class PluginFullObjectTargetStaleError extends Error {
    constructor(characterId: string, conversationId?: string) {
        super(
            conversationId
                ? `Plugin full-object target became stale: ${characterId}/${conversationId}`
                : `Plugin full-object target became stale: ${characterId}`,
        )
        this.name = 'PluginFullObjectTargetStaleError'
    }
}

export interface PluginDatabaseAccessDependencies {
    store: PersistentDataStore
    flushPendingData(reason: string): Promise<void>
    getCompatibilityDatabase(): Database
    getCompatibilityProfile(): PluginCompatibilityProfile
    getSelectedCharacterId(): string | null
    captureSelectedConversationTarget(): SelectedConversationTarget | null
    acquireCompleteConversation(
        reason: string,
        target?: SelectedConversationTarget | null,
    ): Promise<CompleteConversationLease>
    refreshSelectedConversationAfterReplacement(
        target: SelectedConversationTarget,
        expectedSession: ActiveConversationSession,
    ): boolean
    invalidateActiveConversationSession?(): void
    replacePersistentCompleteCharacter(
        characterId: string,
        reason: string,
        mutate: PersistentCompleteCharacterMutation,
        options?: PersistentScopedReplacementOptions,
    ): Promise<boolean>
    replacePersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
        replacement: Chat,
        options?: PersistentScopedReplacementOptions,
    ): Promise<boolean>
    reportIdentityReplacementRejected(
        diagnostic: PluginIdentityReplacementDiagnostic,
    ): void
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
    getCurrentCharacter(
        context: PluginFullObjectCallContext,
    ): Promise<PluginCompleteCharacter | undefined>
    getCharacterFromIndex(
        index: number,
        context: PluginFullObjectCallContext,
    ): Promise<PluginCompleteCharacter | null>
    getChatFromIndex(
        characterIndex: number,
        chatIndex: number,
        context: PluginFullObjectCallContext,
    ): Promise<Chat | null>
    setCurrentCharacter(
        character: PluginCompleteCharacter,
        context: PluginFullObjectCallContext,
    ): Promise<void>
    setCharacterToIndex(
        index: number,
        character: PluginCompleteCharacter,
        context: PluginFullObjectCallContext,
    ): Promise<void>
    setChatToIndex(
        characterIndex: number,
        chatIndex: number,
        chat: Chat,
        context: PluginFullObjectCallContext,
    ): Promise<void>
    queryCharacters(input?: PluginCharacterQuery): Promise<CharacterPage>
    queryConversations(input: PluginConversationQuery): Promise<ConversationPage>
    queryConversationMessages(
        input: PluginConversationMessageQuery,
    ): Promise<PluginConversationWindow | null>
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

export function linkPluginQueryAbortSignals(
    ...signals: Array<AbortSignal | undefined>
): {
    signal: AbortSignal
    dispose(): void
} {
    const activeSignals = signals.filter((signal): signal is AbortSignal => Boolean(signal))
    if (activeSignals.length === 1) {
        return { signal: activeSignals[0], dispose() {} }
    }

    const controller = new AbortController()
    const listeners = activeSignals.map((signal) => {
        const listener = () => controller.abort(signal.reason)
        if (signal.aborted) listener()
        else signal.addEventListener('abort', listener, { once: true })
        return { signal, listener }
    })
    return {
        signal: controller.signal,
        dispose() {
            for (const { signal, listener } of listeners) {
                signal.removeEventListener('abort', listener)
            }
        },
    }
}

function throwIfQueryAborted(signal: AbortSignal | undefined): void {
    if (!signal?.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

function throwIfFullObjectCallAborted(signal: AbortSignal): void {
    if (!signal.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

async function resolvePinnedCharacterTarget(
    reader: PersistentRevisionLease,
    index: number,
): Promise<PluginResolvedCharacterTarget | null> {
    if (!Number.isSafeInteger(index) || index < 0) return null
    let position = 0
    for await (const summary of iteratePinnedCharacterSummaries(reader)) {
        if (position++ === index) {
            return { revision: reader.revision, characterId: summary.id }
        }
    }
    return null
}

async function resolvePinnedConversationTarget(
    reader: PersistentRevisionLease,
    characterIndex: number,
    chatIndex: number,
): Promise<PluginResolvedConversationTarget | null> {
    if (!Number.isSafeInteger(chatIndex) || chatIndex < 0) return null
    const character = await resolvePinnedCharacterTarget(reader, characterIndex)
    if (!character) return null
    let position = 0
    let cursor: string | undefined
    do {
        const page = await reader.queryConversations({
            characterId: character.characterId,
            order: 'configured',
            limit: 128,
            cursor,
        })
        assertPinnedRevision(
            reader.revision,
            page.revision,
            `Conversation page for ${character.characterId}`,
        )
        for (const summary of page.items) {
            if (position++ === chatIndex) {
                return { ...character, conversationId: summary.id }
            }
        }
        cursor = page.nextCursor
    } while (cursor !== undefined)
    return null
}

async function readPinnedCompleteCharacter(
    reader: PersistentRevisionLease,
    characterId: string,
): Promise<PluginCompleteCharacter | null> {
    const detail = await reader.readCharacter(characterId)
    if (!detail) return null
    assertPinnedRevision(reader.revision, detail.revision, `Character ${characterId}`)
    const chats: Chat[] = []
    for await (const conversation of iteratePinnedConversations(reader, characterId)) {
        chats.push(conversation.value)
    }
    return { ...detail.value, chats } as PluginCompleteCharacter
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

function validatePluginCompleteChat(value: unknown): asserts value is Chat {
    if (!value || typeof value !== 'object') {
        throw new TypeError('Plugin conversation replacement must be a complete object')
    }
    const record = value as Record<string, unknown>
    if (typeof record.id !== 'string' || record.id.length === 0) {
        throw new TypeError('Plugin conversation replacement must have a nonempty ID')
    }
    if (!Array.isArray(record.message)) {
        throw new TypeError('Plugin conversation replacement messages must be an array')
    }
}

function validatePluginCompleteCharacter(
    value: unknown,
): asserts value is PluginCompleteCharacter {
    if (!value || typeof value !== 'object') {
        throw new TypeError('Plugin character replacement must be a complete object')
    }
    const record = value as Record<string, unknown>
    if (typeof record.chaId !== 'string' || record.chaId.length === 0) {
        throw new TypeError('Plugin character replacement must have a nonempty character ID')
    }
    if (isCatalogCharacterStub(value as PluginCompleteCharacter)) {
        throw new TypeError('Plugin database characters cannot contain catalog working-set stubs')
    }
    if (!Array.isArray(record.chats)) {
        throw new TypeError(`Plugin database character ${record.chaId} is not fully hydrated`)
    }
    const conversationIds = new Set<string>()
    for (const conversation of record.chats) {
        if (
            !conversation ||
            typeof conversation !== 'object' ||
            typeof (conversation as Record<string, unknown>).id !== 'string' ||
            (conversation as Record<string, unknown>).id === '' ||
            !Array.isArray((conversation as Record<string, unknown>).message)
        ) {
            throw new TypeError(`Plugin database character ${record.chaId} is not fully hydrated`)
        }
        if (conversationIds.has(conversation.id!)) {
            throw new TypeError(`Plugin character contains duplicate conversation ID ${conversation.id}`)
        }
        conversationIds.add(conversation.id!)
    }
}

function validateCompleteCharacters(value: unknown): asserts value is Database['characters'] {
    if (!Array.isArray(value)) {
        throw new TypeError('Plugin database characters must be an array')
    }
    const characterIds = new Set<string>()
    for (const character of value) {
        validatePluginCompleteCharacter(character)
        if (characterIds.has(character.chaId)) {
            throw new TypeError(`Plugin database contains duplicate character ID ${character.chaId}`)
        }
        characterIds.add(character.chaId)
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
    const openStore = () => (openPromise ??= dependencies.store.open().finally(() => {
        openPromise = undefined
    }))
    const acquireCurrentRevisionReader = (): Promise<PersistentRevisionLease> =>
        acquireCurrentRevisionWithRetry(
            (revision) => dependencies.store.acquireRevision(revision),
            async () => (await dependencies.store.readRoot()).revision,
        )
    const prepareQuery = async (signal?: AbortSignal) => {
        throwIfQueryAborted(signal)
        await dependencies.flushPendingData('plugin-database-query')
        throwIfQueryAborted(signal)
        await openStore()
        throwIfQueryAborted(signal)
    }
    const rejectIdentityReplacement = (
        context: PluginFullObjectCallContext,
        operation: PluginIdentityReplacementOperation,
        targetId: string,
        attemptedId: string,
    ): never => {
        const diagnostic = {
            kind: 'plugin-identity-replacement-rejected',
            pluginName: context.pluginName,
            operation,
            targetId,
            attemptedId,
        } satisfies PluginIdentityReplacementDiagnostic
        dependencies.reportIdentityReplacementRejected(diagnostic)
        throw new PluginIdentityReplacementRejectedError(diagnostic)
    }
    const acquireSelectedLease = async (
        selectedTarget: SelectedConversationTarget | null,
        characterId: string,
        conversationId?: string,
    ): Promise<CompleteConversationLease | null> => {
        if (!selectedTarget || selectedTarget.characterId !== characterId) return null
        if (conversationId !== undefined && selectedTarget.conversationId !== conversationId) {
            return null
        }
        return dependencies.acquireCompleteConversation(
            'plugin-full-object-setter',
            selectedTarget,
        )
    }
    const captureSelectedCallBoundary = () => ({
        characterId: dependencies.getSelectedCharacterId(),
        navigationGeneration: dependencies.getNavigationGeneration(),
        target: dependencies.captureSelectedConversationTarget(),
    })
    const recaptureSelectedTarget = (boundary: ReturnType<
        typeof captureSelectedCallBoundary
    >): SelectedConversationTarget | null => {
        const target = dependencies.captureSelectedConversationTarget()
        if (
            !boundary.target ||
            !target ||
            boundary.characterId !== boundary.target.characterId ||
            boundary.navigationGeneration !== boundary.target.navigationGeneration ||
            dependencies.getSelectedCharacterId() !== boundary.characterId ||
            dependencies.getNavigationGeneration() !== boundary.navigationGeneration ||
            target.navigationGeneration !== boundary.navigationGeneration ||
            target.characterId !== boundary.target.characterId ||
            target.conversationId !== boundary.target.conversationId
        ) return null
        return target
    }
    // A leaseless scoped write is durably fenced by expectedRevision, but if
    // it lands on the currently selected conversation (stale call boundary
    // after navigating away and back), the published working-set replacement
    // detaches any live session from its conversation object. Invalidate the
    // session so it re-establishes against the replaced object.
    const invalidateLeaselessSelectedWrite = (
        characterId: string,
        conversationId?: string,
    ): void => {
        if (dependencies.getSelectedCharacterId() !== characterId) return
        if (conversationId !== undefined) {
            const current = dependencies.captureSelectedConversationTarget()
            if (!current || current.conversationId !== conversationId) return
        }
        dependencies.invalidateActiveConversationSession?.()
    }

    return {
        async getCurrentCharacter(context) {
            const initialProfile = dependencies.getCompatibilityProfile()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-read')
            throwIfFullObjectCallAborted(context.signal)
            const characterId = dependencies.getSelectedCharacterId()
            if (characterId === null) {
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                return undefined
            }
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            return withPersistentRevisionLease(lease, async (reader) => {
                throwIfFullObjectCallAborted(context.signal)
                const value = await readPinnedCompleteCharacter(reader, characterId)
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                return value === null ? undefined : dependencies.snapshot(value)
            })
        },

        async getCharacterFromIndex(index, context) {
            const initialProfile = dependencies.getCompatibilityProfile()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-read')
            throwIfFullObjectCallAborted(context.signal)
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            return withPersistentRevisionLease(lease, async (reader) => {
                throwIfFullObjectCallAborted(context.signal)
                const target = await resolvePinnedCharacterTarget(reader, index)
                const value = target
                    ? await readPinnedCompleteCharacter(reader, target.characterId)
                    : null
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                return value === null ? null : dependencies.snapshot(value)
            })
        },

        async getChatFromIndex(characterIndex, chatIndex, context) {
            const initialProfile = dependencies.getCompatibilityProfile()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-read')
            throwIfFullObjectCallAborted(context.signal)
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            return withPersistentRevisionLease(lease, async (reader) => {
                throwIfFullObjectCallAborted(context.signal)
                const target = await resolvePinnedConversationTarget(
                    reader,
                    characterIndex,
                    chatIndex,
                )
                const found = target
                    ? await reader.readConversation(target.characterId, target.conversationId)
                    : null
                if (found) {
                    assertPinnedRevision(
                        reader.revision,
                        found.revision,
                        `Conversation ${target!.conversationId}`,
                    )
                }
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                return found === null ? null : dependencies.snapshot(found.value)
            })
        },

        async setCurrentCharacter(character, context) {
            validatePluginCompleteCharacter(character)
            const candidate = dependencies.snapshot(character)
            const initialProfile = dependencies.getCompatibilityProfile()
            const selectedBoundary = captureSelectedCallBoundary()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-write')
            throwIfFullObjectCallAborted(context.signal)
            const characterId = selectedBoundary.characterId
            if (characterId === null) return
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            const target = await withPersistentRevisionLease(lease, async (reader) => {
                const found = await reader.readCharacter(characterId)
                if (!found) return null
                assertPinnedRevision(reader.revision, found.revision, `Character ${characterId}`)
                return { revision: reader.revision, characterId }
            })
            if (!target) throw new PluginFullObjectTargetStaleError(characterId)
            throwIfFullObjectCallAborted(context.signal)
            if (dependencies.getCompatibilityProfile() !== initialProfile) {
                throw new Error(STALE_DATABASE_SET_ERROR)
            }
            if (candidate.chaId !== target.characterId) {
                rejectIdentityReplacement(
                    context,
                    'setCharacter',
                    target.characterId,
                    candidate.chaId,
                )
            }
            let completeLease: CompleteConversationLease | null = null
            try {
                completeLease = await acquireSelectedLease(
                    recaptureSelectedTarget(selectedBoundary),
                    target.characterId,
                )
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                const replaced = await dependencies.replacePersistentCompleteCharacter(
                    target.characterId,
                    'plugin-setCharacter',
                    async () => dependencies.snapshot(candidate),
                    { expectedRevision: target.revision },
                )
                if (!replaced) throw new PluginFullObjectTargetStaleError(target.characterId)
                if (!completeLease) invalidateLeaselessSelectedWrite(target.characterId)
            } finally {
                if (completeLease) {
                    completeLease.release()
                    dependencies.refreshSelectedConversationAfterReplacement(
                        completeLease.target,
                        completeLease.session,
                    )
                }
            }
        },

        async setCharacterToIndex(index, character, context) {
            validatePluginCompleteCharacter(character)
            const candidate = dependencies.snapshot(character)
            const initialProfile = dependencies.getCompatibilityProfile()
            const selectedBoundary = captureSelectedCallBoundary()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-write')
            throwIfFullObjectCallAborted(context.signal)
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            const target = await withPersistentRevisionLease(lease, async (reader) =>
                resolvePinnedCharacterTarget(reader, index))
            if (!target) return
            throwIfFullObjectCallAborted(context.signal)
            if (dependencies.getCompatibilityProfile() !== initialProfile) {
                throw new Error(STALE_DATABASE_SET_ERROR)
            }
            if (candidate.chaId !== target.characterId) {
                rejectIdentityReplacement(
                    context,
                    'setCharacterToIndex',
                    target.characterId,
                    candidate.chaId,
                )
            }
            let completeLease: CompleteConversationLease | null = null
            try {
                completeLease = await acquireSelectedLease(
                    recaptureSelectedTarget(selectedBoundary),
                    target.characterId,
                )
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                const replaced = await dependencies.replacePersistentCompleteCharacter(
                    target.characterId,
                    'plugin-setCharacterToIndex',
                    async () => dependencies.snapshot(candidate),
                    { expectedRevision: target.revision },
                )
                if (!replaced) throw new PluginFullObjectTargetStaleError(target.characterId)
                if (!completeLease) invalidateLeaselessSelectedWrite(target.characterId)
            } finally {
                if (completeLease) {
                    completeLease.release()
                    dependencies.refreshSelectedConversationAfterReplacement(
                        completeLease.target,
                        completeLease.session,
                    )
                }
            }
        },

        async setChatToIndex(characterIndex, chatIndex, chat, context) {
            validatePluginCompleteChat(chat)
            const candidate = dependencies.snapshot(chat)
            const initialProfile = dependencies.getCompatibilityProfile()
            const selectedBoundary = captureSelectedCallBoundary()
            throwIfFullObjectCallAborted(context.signal)
            await dependencies.flushPendingData('plugin-full-object-write')
            throwIfFullObjectCallAborted(context.signal)
            await openStore()
            const lease = await acquireCurrentRevisionReader()
            const target = await withPersistentRevisionLease(lease, async (reader) =>
                resolvePinnedConversationTarget(reader, characterIndex, chatIndex))
            if (!target) return
            throwIfFullObjectCallAborted(context.signal)
            if (dependencies.getCompatibilityProfile() !== initialProfile) {
                throw new Error(STALE_DATABASE_SET_ERROR)
            }
            if (candidate.id !== target.conversationId) {
                rejectIdentityReplacement(
                    context,
                    'setChatToIndex',
                    target.conversationId,
                    candidate.id!,
                )
            }
            let completeLease: CompleteConversationLease | null = null
            try {
                completeLease = await acquireSelectedLease(
                    recaptureSelectedTarget(selectedBoundary),
                    target.characterId,
                    target.conversationId,
                )
                throwIfFullObjectCallAborted(context.signal)
                if (dependencies.getCompatibilityProfile() !== initialProfile) {
                    throw new Error(STALE_DATABASE_SET_ERROR)
                }
                const replaced = await dependencies.replacePersistentConversation(
                    target.characterId,
                    target.conversationId,
                    'plugin-setChatToIndex',
                    candidate,
                    { expectedRevision: target.revision },
                )
                if (!replaced) {
                    throw new PluginFullObjectTargetStaleError(
                        target.characterId,
                        target.conversationId,
                    )
                }
                if (!completeLease) {
                    invalidateLeaselessSelectedWrite(
                        target.characterId,
                        target.conversationId,
                    )
                }
            } finally {
                if (completeLease) {
                    completeLease.release()
                    dependencies.refreshSelectedConversationAfterReplacement(
                        completeLease.target,
                        completeLease.session,
                    )
                }
            }
        },

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

            const ranged = input.startIndex !== undefined
            const anchored = input.anchorMessageId !== undefined
            let query
            if (ranged) {
                if (!Number.isSafeInteger(input.startIndex) || input.startIndex! < 0) {
                    throw new RangeError(
                        'Message range startIndex must be a nonnegative safe integer',
                    )
                }
                if (input.limit === undefined) {
                    throw new RangeError('Absolute message ranges require limit')
                }
                if (
                    anchored ||
                    input.before !== undefined ||
                    input.after !== undefined
                ) {
                    throw new RangeError('Absolute message ranges cannot include anchor options')
                }
                query = {
                    characterId: input.characterId,
                    conversationId: input.conversationId,
                    startIndex: input.startIndex,
                    limit: positiveLimit(
                        input.limit,
                        PLUGIN_MESSAGE_QUERY_DEFAULT_LIMIT,
                        PLUGIN_MESSAGE_QUERY_MAX_LIMIT,
                    ),
                }
            } else if (anchored) {
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

            await prepareQuery(input.signal)
            throwIfQueryAborted(input.signal)
            const result = await dependencies.store.readConversationWindow(query)
            throwIfQueryAborted(input.signal)
            return result ? { ...result.value, revision: result.revision } : null
        },

        async getDatabaseSnapshot(includeOnly, allowedKeys) {
            const requestedKeys =
                includeOnly === 'all'
                    ? [...allowedKeys]
                    : allowedKeys.filter((key) => includeOnly.includes(key))
            const compatibilityProfile = dependencies.getCompatibilityProfile()
            const needsCharacters = requestedKeys.includes('characters')
            const needsPersistentPresets =
                compatibilityProfile === 'scalable-v3' && requestedKeys.includes('botPresets')
            if (!needsCharacters && !needsPersistentPresets) {
                const compatibilityDatabase = dependencies.getCompatibilityDatabase()
                const result: Record<string, unknown> = {}
                for (const key of requestedKeys) {
                    if (key === 'pluginCustomStorage' && compatibilityProfile === 'scalable-v3') {
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
            if (compatibilityProfile === 'scalable-v3') {
                await dependencies.flushPendingData('plugin-full-database-snapshot')
                await openStore()
                const reader = await acquireCurrentRevisionReader()
                return withPersistentRevisionLease(reader, async (reader) => {
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
                })
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
                if (hasCharacterUpdate(preparedUpdate)) {
                    await dependencies.applyCompatibilityDatabase(
                        dependencies.snapshot(preparedUpdate),
                    )
                } else {
                    dependencies.applyCompatibilityDatabaseLite(
                        dependencies.snapshot(preparedUpdate),
                    )
                    await dependencies.flushPendingData('plugin-root-update')
                }
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
            if (!hasCharacterUpdate(compatibilityUpdate)) {
                dependencies.applyCompatibilityDatabaseLite(compatibilityUpdate)
                if (storageMutations.length > 0)
                    await dependencies.mutatePluginStorage(storageMutations)
                await dependencies.flushPendingData('plugin-root-update')
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

export function createProductionPluginChatOutputProjector(
    snapshot: <T>(value: T) => T,
): PluginChatOutputProjector {
    let store: ReturnType<typeof getPersistentDataStore> | undefined
    let openPromise: Promise<void> | undefined
    return async (input) => {
        const persistentStore = store ??= getPersistentDataStore()
        await (openPromise ??= persistentStore.open().finally(() => {
            openPromise = undefined
        }))
        const lease = await acquireCurrentRevisionWithRetry(
            (revision) => persistentStore.acquireRevision(revision),
            async () => (await persistentStore.readRoot()).revision,
        )
        return withPersistentRevisionLease(lease, async (reader) => {
            const durable = await readPinnedCompleteCharacter(reader, input.characterId)
            if (!durable) throw new Error(`Missing listener character ${input.characterId}`)

            const liveCompleteChats = new Map(
                input.liveCharacter.chats
                    .filter((chat) => !isConversationSummaryStub(chat) && chat.id)
                    .map((chat) => [chat.id!, snapshot(chat)]),
            )
            liveCompleteChats.set(input.conversationId, snapshot(input.liveConversation))

            const { chats: _liveChats, ...liveDetail } = snapshot(input.liveCharacter)
            const char = {
                ...durable,
                ...liveDetail,
                chats: durable.chats.map(
                    (chat) => liveCompleteChats.get(chat.id!) ?? chat,
                ),
            } as PluginCompleteCharacter
            const chat = char.chats.find(
                (candidate) => candidate.id === input.conversationId,
            )
            if (!chat) throw new Error(`Missing listener conversation ${input.conversationId}`)
            return { char: snapshot(char), chat: snapshot(chat) }
        })
    }
}

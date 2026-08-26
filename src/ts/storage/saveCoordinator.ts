import type { Chat, Database, botPreset, character, groupChat } from './database.svelte'
import type {
    CharacterDetail,
    ConversationMutation,
    DataRevision,
    PersistentDataStore,
    PluginStorageMutation,
    PersistentRoot,
    WorkingSetCommit,
} from './persistentDataStore'
import { RevisionConflictError } from './persistentDataStore'
import { appendCharacterIdToOrder, removeCharacterIdFromOrder } from './characterOrderMutation'
import { isConversationSummaryStub } from './conversationResidency'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'
import { withPersistentRevisionLease } from './persistentRecordIterator'

const SAVE_DEBOUNCE_MS = 500
const PENDING_BYTE_LIMIT = 1_048_576
/** Official publishes upload the full database snapshot, so they are spaced like upstream's save loop. */
const OFFICIAL_PUBLISH_MIN_INTERVAL_MS = 3_000
const CONCURRENT_CHARACTER_COMPENSATION_ATTEMPTS = 3
const CHARACTER_MUTATION_PAGE_SIZE = 100

type CompleteCharacter = character | groupChat
type RootDatabase = PersistentRoot

export interface PinnedPublication {
    publish(): Promise<void>
    dispose(): Promise<void>
}

export interface OfficialRevisionPublisher {
    pin(revision: DataRevision): Promise<PinnedPublication>
}

export interface SaveCoordinatorClock {
    setTimeout(callback: () => void, delay: number): unknown
    clearTimeout(handle: unknown): void
}

export interface SaveCoordinatorDependencies {
    store: PersistentDataStore
    captureRoot(): RootDatabase
    capturePluginStorage?(): Database['pluginCustomStorage'] | null
    publishPluginStorageWorkingSet?(storage: Database['pluginCustomStorage']): void
    capturePresets?(): botPreset[] | null
    captureSelectedCharacter(): CompleteCharacter | null
    captureCharacter(id: string): CompleteCharacter | null
    /** Installs the working copy synchronously and must not throw. */
    replaceDatabase(database: Database): void
    /** Publishes the committed preset state synchronously and must not throw. */
    publishPresetWorkingSet?(state: PersistentPresetMutationResult): void
    /** Publishes one committed character mutation synchronously and must not throw. */
    publishCharacterMutation?(state: PersistentCharacterMutationResult): void
    isIncompleteWorkingSet?(database: Database): boolean
    getNavigationGeneration?(): number
    officialPublisher?: OfficialRevisionPublisher
    clock?: SaveCoordinatorClock
    now?(): number
    onLocalRevision?(revision: DataRevision): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
}

interface CapturedState {
    root: RootDatabase
    rootCanonical: string
    pluginStorage: Database['pluginCustomStorage'] | null
    pluginStorageCanonical: string | null
    presets: botPreset[] | null
    presetsCanonical: string | null
    character: CompleteCharacter | null
    characterCanonical: string | null
    conversationStubIds: ReadonlySet<string>
}

export interface CharacterAdditionRequest {
    characterId: string
    estimatedBytes: number
    install(): void
}

interface ReservedCharacterAddition {
    request: CharacterAdditionRequest | null
    token: object
}

interface PendingCharacterAddition {
    characterId: string
    token: object
    locallyAdded: boolean
    baseline: string | null
}

interface PendingResidentCompensation {
    characterId: string
}

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
    return JSON.stringify(canonicalize(value))
}

function canonicalClone<T>(value: T): T {
    return JSON.parse(canonicalJson(value)) as T
}

function pluginStorageJson(storage: Database['pluginCustomStorage']): string {
    const normalized: Database['pluginCustomStorage'] = {}
    for (const key of Object.keys(storage)) {
        const value = storage[key]
        if (value !== undefined) {
            defineOwnEnumerableProperty(normalized, key, canonicalize(value))
        }
    }
    return JSON.stringify(normalized)
}

function pluginStorageClone(
    storage: Database['pluginCustomStorage'],
): Database['pluginCustomStorage'] {
    return JSON.parse(pluginStorageJson(storage)) as Database['pluginCustomStorage']
}

function canonicalDatabaseClone(database: Database): Database {
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

interface ReplacementRebaseResult {
    database: Database
    compensation: Omit<WorkingSetCommit, 'expectedRevision'> | null
}

function rebaseRootMutation(
    base: RootDatabase,
    mutated: RootDatabase,
    live: RootDatabase,
): RootDatabase {
    const rebased = canonicalClone(live) as RootDatabase & Record<string, unknown>
    const baseRecord = base as RootDatabase & Record<string, unknown>
    const mutatedRecord = mutated as RootDatabase & Record<string, unknown>
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

function messageReplaceRange<T>(
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

function rebaseConcurrentLiveDelta<T>(base: T, live: T, candidate: T): T {
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
                candidateRecord[key] = canonicalClone(liveRecord[key])
                continue
            }
            if (!canonicalValuesEqual(baseRecord[key], liveRecord[key])) {
                candidateRecord[key] = Object.hasOwn(candidateRecord, key)
                    ? rebaseConcurrentLiveDelta(
                        baseRecord[key],
                        liveRecord[key],
                        candidateRecord[key],
                    )
                    : canonicalClone(liveRecord[key])
            }
        }
        return candidateRecord as T
    }
    return canonicalClone(live)
}

function splitDatabase(database: Database): {
    root: RootDatabase
    characters: CompleteCharacter[]
    presets: botPreset[]
    pluginStorage: Database['pluginCustomStorage']
} {
    const { characters, botPresets, pluginCustomStorage, ...root } = database
    return {
        root,
        characters,
        presets: botPresets ?? [],
        pluginStorage: pluginCustomStorage ?? {},
    }
}

function diffPluginStorage(
    baseline: string | null,
    current: Database['pluginCustomStorage'],
): PluginStorageMutation[] {
    const previous = baseline
        ? JSON.parse(baseline) as Database['pluginCustomStorage']
        : {}
    const currentKeys = Object.keys(current)
    if (currentKeys.length === 0 && Object.keys(previous).length > 0) {
        return [{ type: 'clear' }]
    }
    const mutations: PluginStorageMutation[] = []
    const previousKeys = Object.keys(previous)
    for (const key of previousKeys) {
        if (!Object.hasOwn(current, key)) mutations.push({ type: 'delete', key })
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
        if (movedKeys.has(key)) mutations.push({ type: 'delete', key })
    }
    for (const key of currentKeys) {
        if (
            movedKeys.has(key) ||
            !Object.hasOwn(previous, key) ||
            !canonicalValuesEqual(previous[key], current[key])
        ) {
            mutations.push({ type: 'set', key, value: current[key] })
        }
    }
    return mutations
}

function applyPluginStorageMutations(
    storage: Database['pluginCustomStorage'],
    mutations: readonly PluginStorageMutation[],
): Database['pluginCustomStorage'] {
    const next = pluginStorageClone(storage)
    for (const mutation of mutations) {
        if (mutation.type === 'clear') {
            for (const key of Object.keys(next)) delete next[key]
        } else if (mutation.type === 'delete') {
            delete next[mutation.key]
        } else {
            defineOwnEnumerableProperty(
                next,
                mutation.key,
                canonicalClone(mutation.value),
            )
        }
    }
    return next
}

function rebaseConcurrentPluginStorage(
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

export interface PersistentReplacementOptions {
    publishOfficial?: boolean
    authoritative?: boolean
    expectedRevision?: DataRevision
    expectedMutationGeneration?: number
}

export interface PersistentPresetMutationState {
    root: RootDatabase
    presets: botPreset[]
}

export interface PersistentPresetMutationResult extends PersistentPresetMutationState {
    revision: DataRevision
}

export type PersistentPresetMutation = (
    state: PersistentPresetMutationState,
) => void | Promise<void>

export interface PersistentCharacterMutationState {
    root: RootDatabase
    character: CharacterDetail
}

export interface PersistentCharacterMutationResult {
    revision: DataRevision
    root: RootDatabase
    characterId: string
    kind: 'detail' | 'replace' | 'add' | 'delete'
    character: CharacterDetail | CompleteCharacter | null
    relatedCharacters?: CharacterDetail[]
}

export interface PersistentMutationToken {
    revision: DataRevision
    mutationGeneration: number
}

export class PersistentMutationFencedError extends Error {
    constructor() {
        super('A destructive persistent replacement is active')
        this.name = 'PersistentMutationFencedError'
    }
}

export interface PersistentDatabaseSnapshot extends PersistentMutationToken {
    database: Database
}

export interface PersistentSelectedConversation {
    character: CharacterDetail
    conversation: Chat | null
}

export type PersistentCharacterDetailMutation = (
    state: PersistentCharacterMutationState,
) => void | { delete: true } | Promise<void | { delete: true }>

export type PersistentCompleteCharacterMutation = (
    character: CompleteCharacter,
) => CompleteCharacter | Promise<CompleteCharacter>

export type PersistentCompleteCharacterUpsert = (
    character: CompleteCharacter | null,
) => CompleteCharacter | Promise<CompleteCharacter>

export interface PersistentCompleteCharacterUpsertOptions {
    includeInCharacterOrder?: boolean
}

function defaultClock(): SaveCoordinatorClock {
    return {
        setTimeout: (callback, delay) => globalThis.setTimeout(callback, delay),
        clearTimeout: (handle) => globalThis.clearTimeout(handle as ReturnType<typeof setTimeout>),
    }
}

export class SaveCoordinator {
    private readonly clock: SaveCoordinatorClock
    private currentRevision: DataRevision | null = null
    private rootBaseline: string | null = null
    private pluginStorageBaseline: string | null = null
    private presetsBaseline: string | null = null
    private characterBaseline: string | null = null
    private characterBaselineId: string | null = null
    private dirtyGeneration = 0
    private pendingByteCount = 0
    private debounceHandle: unknown
    private operationTail: Promise<void> = Promise.resolve()
    private flushPromise: Promise<void> | null = null
    private localFlushPromise: Promise<void> | null = null
    private localFlushDuringPublicationPromise: Promise<void> | null = null
    private queuedOperationCount = 0
    private publicationInProgress = false
    private readonly operationStateWaiters = new Set<() => void>()
    private additionPromise: Promise<void> | null = null
    private lastReportedFlushPromise: Promise<void> | null = null
    private pendingPublication: PinnedPublication | null = null
    private pendingPublicationRevision: DataRevision | null = null
    private deferredPublicationRevision: DataRevision | null = null
    private readonly pendingPublicationCleanup = new Set<PinnedPublication>()
    private lastOfficialPublishAttemptAt: number | null = null
    private officialPublishRetryHandle: unknown
    private pendingCharacterAddition: PendingCharacterAddition | null = null
    private reservedCharacterAddition: ReservedCharacterAddition | null = null
    private pendingResidentCompensations: PendingResidentCompensation[] = []
    private lastBackgroundErrorMessage: string | null = null
    private destructiveReplacementFence: {
        owner: symbol
        state: 'acquiring' | 'held'
        acceptsPostPublicationDirty: boolean
        queuedPostPublicationDirty: boolean
        blockedPrePublicationDirty: boolean
    } | null = null

    constructor(private readonly dependencies: SaveCoordinatorDependencies) {
        this.clock = dependencies.clock ?? defaultClock()
    }

    get revision(): DataRevision {
        if (this.currentRevision === null) throw new Error('Save coordinator is not initialized')
        return this.currentRevision
    }

    get pendingBytes(): number {
        return this.pendingByteCount
    }

    get mutationGeneration(): number {
        return this.dirtyGeneration
    }

    get hasDestructiveReplacementFence(): boolean {
        return this.destructiveReplacementFence !== null
    }

    initialize(revision: DataRevision, database?: Database): void {
        this.cancelDebounce()
        this.cancelOfficialPublishRetry()
        const captured = database ? this.captureDatabase(database) : this.capture()
        this.currentRevision = revision
        this.rootBaseline = captured.rootCanonical
        this.pluginStorageBaseline = captured.pluginStorageCanonical
        this.presetsBaseline = captured.presetsCanonical
        this.setCharacterBaseline(captured)
        this.dirtyGeneration = 0
        this.pendingByteCount = 0
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        this.deferredPublicationRevision = null
        this.lastOfficialPublishAttemptAt = null
        this.pendingCharacterAddition = null
        this.reservedCharacterAddition = null
        this.pendingResidentCompensations = []
        this.lastBackgroundErrorMessage = null
        if (this.destructiveReplacementFence?.state === 'held') {
            this.destructiveReplacementFence.acceptsPostPublicationDirty = true
            this.destructiveReplacementFence.queuedPostPublicationDirty = false
            this.destructiveReplacementFence.blockedPrePublicationDirty = false
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    adoptHydratedCharacter(
        revision: DataRevision,
        mutationGeneration: number,
        character: CompleteCharacter,
    ): boolean {
        if (
            this.destructiveReplacementFence !== null ||
            this.currentRevision !== revision ||
            this.dirtyGeneration !== mutationGeneration
        ) return false
        this.characterBaseline = canonicalJson(character)
        this.characterBaselineId = character.chaId
        return true
    }

    adoptMaterializedDatabase(
        revision: DataRevision,
        mutationGeneration: number,
        database: Database,
    ): boolean {
        if (
            this.destructiveReplacementFence !== null ||
            this.currentRevision !== revision ||
            this.dirtyGeneration !== mutationGeneration
        ) return false
        const captured = this.captureDatabase(database)
        this.rootBaseline = captured.rootCanonical
        this.pluginStorageBaseline = captured.pluginStorageCanonical
        this.presetsBaseline = captured.presetsCanonical
        this.setCharacterBaseline(captured)
        return true
    }

    markPersistentDataDirty(estimatedBytes: number): void {
        this.assertInitialized()
        const fence = this.destructiveReplacementFence
        if (fence?.state === 'held') {
            if (!fence.acceptsPostPublicationDirty) {
                if (!this.captureMatchesBaseline()) {
                    fence.blockedPrePublicationDirty = true
                }
                throw new PersistentMutationFencedError()
            }
            if (this.captureMatchesBaseline()) return
            fence.queuedPostPublicationDirty = true
        }
        this.dirtyGeneration++
        const bytes = Number.isFinite(estimatedBytes) && estimatedBytes > 0 ? estimatedBytes : 0
        const previousBytes = this.pendingByteCount
        this.pendingByteCount = Math.max(previousBytes, bytes)
        this.cancelDebounce()
        if (fence) return
        if (this.pendingByteCount >= PENDING_BYTE_LIMIT && previousBytes < PENDING_BYTE_LIMIT) {
            this.startBackgroundFlush('byte-limit')
            return
        }
        if (!this.flushPromise) this.armDebounce()
    }

    flushPendingData(reason: string): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        if (this.additionPromise) return this.additionPromise
        if (this.flushPromise) return this.flushPromise
        const promise = this.enqueue(() => this.flushIterations(reason, true))
        this.flushPromise = promise
        this.reportActivePromise()
        void promise.then(
            () => {
                if (this.flushPromise === promise) {
                    this.flushPromise = null
                    this.reportActivePromise()
                }
            },
            () => {
                if (this.flushPromise === promise) {
                    this.flushPromise = null
                    this.reportActivePromise()
                }
            },
        )
        return promise
    }

    flushPendingDataLocally(reason: string): Promise<void> {
        this.assertInitialized()
        this.cancelDebounce()
        if (this.localFlushPromise) return this.localFlushPromise
        const promise = this.runLocalFlush(reason)
        this.localFlushPromise = promise
        this.reportActivePromise()
        void promise.then(
            () => {
                if (this.localFlushPromise === promise) {
                    this.localFlushPromise = null
                    this.reportActivePromise()
                }
            },
            () => {
                if (this.localFlushPromise === promise) {
                    this.localFlushPromise = null
                    this.reportActivePromise()
                }
            },
        )
        return promise
    }

    private async runLocalFlush(reason: string): Promise<void> {
        while (this.queuedOperationCount > 0 && !this.publicationInProgress) {
            await this.waitForOperationStateChange()
        }
        if (this.publicationInProgress) {
            const promise = this.flushIterations(reason, false)
            this.localFlushDuringPublicationPromise = promise
            try {
                await promise
                return
            } finally {
                if (this.localFlushDuringPublicationPromise === promise) {
                    this.localFlushDuringPublicationPromise = null
                }
            }
        }
        await this.enqueue(() => this.flushIterations(reason, false))
    }

    replacePersistentDatabase(
        database: Database,
        reason: string,
        options: PersistentReplacementOptions = {},
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const expectationError = this.replacementExpectationError(options)
        if (expectationError) return Promise.reject(expectationError)
        if (!options.authoritative && this.dependencies.isIncompleteWorkingSet?.(database)) {
            return Promise.reject(
                new Error('Cannot replace persistent data from an incomplete persistent working set'),
            )
        }
        const candidate = canonicalDatabaseClone(database)
        const before = this.capture()
        const capturedGeneration = this.dirtyGeneration
        const supersededAdditionToken = (
            this.pendingCharacterAddition ?? this.reservedCharacterAddition
        )?.token ?? null
        const hadPendingDebounce = this.debounceHandle !== undefined
        this.cancelDebounce()
        const replacement = this.enqueue(() => {
            const queuedExpectationError = this.replacementExpectationError(options)
            if (queuedExpectationError) throw queuedExpectationError
            return this.runReplacement(
                candidate,
                before,
                capturedGeneration,
                supersededAdditionToken,
                reason,
                options,
            )
        })
        return replacement.catch((error) => {
            this.rearmDebounceAfterReplacementFailure(capturedGeneration, hadPendingDebounce)
            throw error
        })
    }

    replacePreparedPersistentDatabase(
        prepare: () => Promise<Database>,
        reason: string,
        options: PersistentReplacementOptions = {},
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        const expectationError = this.replacementExpectationError(options)
        if (expectationError) return Promise.reject(expectationError)
        const before = this.capture()
        const capturedGeneration = this.dirtyGeneration
        const supersededAdditionToken = (
            this.pendingCharacterAddition ?? this.reservedCharacterAddition
        )?.token ?? null
        const hadPendingDebounce = this.debounceHandle !== undefined
        this.cancelDebounce()
        const preparation = Promise.resolve().then(prepare)
        const replacement = this.enqueue(async () => {
            const queuedExpectationError = this.replacementExpectationError(options)
            if (queuedExpectationError) throw queuedExpectationError
            let candidate: Database
            try {
                const prepared = await preparation
                const preparedExpectationError = this.replacementExpectationError(options)
                if (preparedExpectationError) throw preparedExpectationError
                if (!options.authoritative && this.dependencies.isIncompleteWorkingSet?.(prepared)) {
                    throw new Error(
                        'Cannot replace persistent data from an incomplete persistent working set',
                    )
                }
                candidate = canonicalDatabaseClone(prepared)
            } catch (error) {
                throw error
            }
            await this.runReplacement(
                candidate,
                before,
                capturedGeneration,
                supersededAdditionToken,
                reason,
                options,
            )
        })
        return replacement.catch((error) => {
            this.rearmDebounceAfterReplacementFailure(capturedGeneration, hadPendingDebounce)
            throw error
        })
    }

    mutatePersistentPresets(
        reason: string,
        mutate: PersistentPresetMutation,
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const operationStart = this.capture()
            const livePresetsBefore = operationStart.presetsCanonical
            const revision = this.revision
            const [rootValue, catalog] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.queryPresets(),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            this.assertReadRevision(revision, catalog.revision)

            const ordered = [...catalog.items].sort(
                (left, right) => left.configuredIndex - right.configuredIndex,
            )
            const presets = await Promise.all(ordered.map(async (summary) => {
                const value = await this.dependencies.store.readPreset(summary.id)
                if (!value) throw new Error(`Preset ${summary.id} was not found`)
                this.assertReadRevision(revision, value.revision)
                return canonicalClone(value.value)
            }))
            const state: PersistentPresetMutationState = {
                root: canonicalClone(rootValue.value),
                presets,
            }
            await mutate(state)

            const liveBeforeCommit = this.capture()
            const mutatedRoot = rebaseRootMutation(
                rootValue.value,
                state.root,
                operationStart.root,
            )
            const committedRoot = rebaseConcurrentLiveDelta(
                operationStart.root,
                liveBeforeCommit.root,
                mutatedRoot,
            )
            const committedPresets = canonicalClone(state.presets)
            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                root: committedRoot,
                replacePresets: committedPresets,
            })
            const liveAfterCommit = this.capture()
            if (
                livePresetsBefore !== null &&
                liveAfterCommit.presetsCanonical !== livePresetsBefore
            ) {
                this.currentRevision = committed.revision
                this.rootBaseline = canonicalJson(committedRoot)
                this.presetsBaseline = canonicalJson(committedPresets)
                this.dependencies.onLocalRevision?.(committed.revision)
                await this.flushIterations(`${reason}-concurrent-live-presets`, true)
                throw new Error('Persistent working set changed during preset mutation')
            }
            this.currentRevision = committed.revision
            this.dirtyGeneration++
            this.rootBaseline = canonicalJson(committedRoot)
            this.presetsBaseline = null
            const publishedRoot = rebaseRootMutation(
                liveBeforeCommit.root,
                liveAfterCommit.root,
                committedRoot,
            )
            this.dependencies.publishPresetWorkingSet?.({
                revision: committed.revision,
                root: publishedRoot,
                presets: committedPresets,
            })
            const published = this.capture()
            this.presetsBaseline = published.presetsCanonical
            this.dependencies.onLocalRevision?.(committed.revision)
            if (this.dependencies.officialPublisher) {
                await this.stagePublication(committed.revision)
                const delay = this.officialPublishDelayMs()
                if (delay <= 0) await this.publishPendingRevision()
                else this.armOfficialPublishRetry(delay)
            }
            this.pendingByteCount = 0
            this.lastBackgroundErrorMessage = null
        })
    }

    mutatePersistentPluginStorage(
        reason: string,
        mutations: readonly PluginStorageMutation[],
    ): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            if (mutations.length === 0) return
            const revision = this.revision
            const baseline = this.pluginStorageBaseline === null
                ? null
                : JSON.parse(this.pluginStorageBaseline) as Database['pluginCustomStorage']
            const committedStorage = applyPluginStorageMutations(baseline ?? {}, mutations)
            const liveBeforeCommit = this.capture().pluginStorage
            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                pluginStorage: canonicalClone(mutations) as PluginStorageMutation[],
            })
            this.currentRevision = committed.revision
            this.dirtyGeneration++
            if (baseline !== null) {
                this.pluginStorageBaseline = pluginStorageJson(committedStorage)
                const liveAfterCommit = this.capture().pluginStorage
                const publishedStorage =
                    liveBeforeCommit !== null && liveAfterCommit !== null
                        ? rebaseConcurrentPluginStorage(
                            liveBeforeCommit,
                            liveAfterCommit,
                            committedStorage,
                        )
                        : committedStorage
                this.dependencies.publishPluginStorageWorkingSet?.(
                    publishedStorage,
                )
            }
            this.dependencies.onLocalRevision?.(committed.revision)
            this.pendingByteCount = 0
            this.lastBackgroundErrorMessage = null
            await this.finishExplicitCommit(committed.revision)
        })
    }

    mutatePersistentCharacterDetail(
        characterId: string,
        reason: string,
        mutate: PersistentCharacterDetailMutation,
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (!characterValue) return false
            this.assertReadRevision(revision, characterValue.revision)
            if (characterValue.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }

            const state: PersistentCharacterMutationState = {
                root: canonicalClone(rootValue.value),
                character: canonicalClone(characterValue.value),
            }
            const outcome = await mutate(state)
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            const deleting = typeof outcome === 'object' && outcome?.delete === true
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                state.root,
                liveBeforeCommit.root,
            )
            const rootChanged = canonicalJson(committedRoot) !== canonicalJson(rootValue.value)
            const commit: WorkingSetCommit = { expectedRevision: revision }
            if (rootChanged) commit.root = committedRoot
            if (deleting) commit.deleteCharacterId = characterId
            else commit.character = canonicalClone(state.character)

            const committed = await this.dependencies.store.commit(commit)
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                const committedCharacter = deleting
                    ? null
                    : this.mergeCommittedDetailWithResident(
                        state.character,
                        residentBefore?.character ?? null,
                    )
                return this.compensateConcurrentResidentCharacter({
                    committedRevision: committed.revision,
                    committedRoot,
                    committedCharacter,
                    deleting,
                    characterId,
                    residentAfterCommit,
                    reason,
                })
            }
            const liveAfterCommit = this.capture()
            this.finishCharacterMutation({
                revision: committed.revision,
                root: rebaseRootMutation(
                    liveBeforeCommit.root,
                    liveAfterCommit.root,
                    committedRoot,
                ),
                characterId,
                kind: deleting ? 'delete' : 'detail',
                character: deleting ? null : canonicalClone(state.character),
            }, committedRoot)
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    deletePersistentCharacterWithGroupReferences(
        characterId: string,
        reason: string,
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const mutationGeneration = this.dirtyGeneration
            const lease = await this.dependencies.store.acquireRevision(revision)
            let rootValue: { revision: DataRevision; value: RootDatabase } | undefined
            const relatedCharacters: CharacterDetail[] = []
            const relatedResidentsBefore = new Map<
                string,
                ReturnType<SaveCoordinator['captureResidentCharacter']>
            >()
            const found = await withPersistentRevisionLease(lease, async (reader) => {
                this.assertReadRevision(revision, reader.revision)
                rootValue = await reader.readRoot()
                this.assertReadRevision(revision, rootValue.revision)
                const targetValue = await reader.readCharacter(characterId)
                if (!targetValue) return false
                this.assertReadRevision(revision, targetValue.revision)
                if (targetValue.value.chaId !== characterId) {
                    throw new Error(`Character ${characterId} returned mismatched detail`)
                }

                for (const trash of [false, true]) {
                    let cursor: string | undefined
                    do {
                        const page = await reader.queryCharacters({
                            order: 'configured',
                            trash,
                            limit: CHARACTER_MUTATION_PAGE_SIZE,
                            cursor,
                        })
                        this.assertReadRevision(revision, page.revision)
                        for (const summary of page.items) {
                            if (summary.id === characterId || summary.type !== 'group') continue
                            const value = await reader.readCharacter(summary.id)
                            if (!value) throw new Error(`Character ${summary.id} was not found`)
                            this.assertReadRevision(revision, value.revision)
                            if (value.value.chaId !== summary.id || value.value.type !== 'group') {
                                throw new Error(`Character ${summary.id} returned mismatched detail`)
                            }
                            const group = canonicalClone(value.value) as Omit<groupChat, 'chats'>
                            if (!this.removeGroupCharacterReference(group, characterId)) continue
                            relatedResidentsBefore.set(
                                summary.id,
                                this.captureResidentCharacter(summary.id),
                            )
                            relatedCharacters.push(group)
                        }
                        cursor = page.nextCursor
                    } while (cursor)
                }
                return true
            })
            if (!found) return false
            if (!rootValue) return false
            if (this.dirtyGeneration !== mutationGeneration) {
                throw new Error(`Persistent data changed during character deletion: ${characterId}`)
            }
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            for (const [relatedId, before] of relatedResidentsBefore) {
                this.assertResidentCharacterUnchanged(relatedId, before)
            }

            const mutatedRoot = canonicalClone(rootValue.value)
            removeCharacterIdFromOrder(mutatedRoot, characterId)
            const liveBeforeCommit = this.capture()
            const committedRoot = rebaseRootMutation(
                rootValue.value,
                mutatedRoot,
                liveBeforeCommit.root,
            )
            const commit: WorkingSetCommit = {
                expectedRevision: revision,
                deleteCharacterId: characterId,
                characterDetails: canonicalClone(relatedCharacters),
            }
            if (canonicalJson(committedRoot) !== canonicalJson(rootValue.value)) {
                commit.root = committedRoot
            }
            const committed = await this.dependencies.store.commit(commit)
            const changedDuringCommit = this.dirtyGeneration !== mutationGeneration
            const relatedRaces = new Map<
                string,
                NonNullable<ReturnType<SaveCoordinator['captureResidentCharacter']>>
            >()
            for (const [relatedId, before] of relatedResidentsBefore) {
                const after = this.captureResidentCharacter(relatedId)
                if (!this.residentCharactersMatch(before, after) && after) {
                    relatedRaces.set(relatedId, after)
                }
            }
            const publishedRelatedCharacters = relatedCharacters.map((detail) => {
                const raced = relatedRaces.get(detail.chaId)
                if (!raced) return canonicalClone(detail)
                const resident = canonicalClone(raced.character)
                this.removeGroupCharacterReference(resident, characterId)
                const { chats: _chats, ...residentDetail } = resident
                return residentDetail as CharacterDetail
            })
            const liveAfterCommit = this.capture()
            this.finishCharacterMutation({
                revision: committed.revision,
                root: rebaseRootMutation(
                    liveBeforeCommit.root,
                    liveAfterCommit.root,
                    committedRoot,
                ),
                characterId,
                kind: 'delete',
                character: null,
                relatedCharacters: publishedRelatedCharacters,
            }, committedRoot, {
                preservePendingWork: changedDuringCommit || relatedRaces.size > 0,
            })
            for (const relatedId of relatedRaces.keys()) {
                this.enqueueResidentCompensation(relatedId)
            }
            if (changedDuringCommit || relatedRaces.size > 0) {
                if (this.dependencies.officialPublisher) {
                    await this.stagePublication(committed.revision)
                }
                await this.flushIterations(reason, true)
            } else {
                await this.finishExplicitCommit(committed.revision)
            }
            return true
        })
    }

    replacePersistentCompleteCharacter(
        characterId: string,
        reason: string,
        mutate: PersistentCompleteCharacterMutation,
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (!characterValue) return false
            this.assertReadRevision(revision, characterValue.revision)
            if (characterValue.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            const current = await this.readCompleteCharacter(
                characterId,
                revision,
                characterValue.value,
            )
            const replacement = canonicalClone(await mutate(current))
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            if (replacement.chaId !== characterId) {
                throw new Error(`Replacement character ID must remain ${characterId}`)
            }

            const committed = await this.dependencies.store.commit({
                expectedRevision: revision,
                replaceCharacter: replacement,
            })
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                return this.compensateConcurrentResidentCharacter({
                    committedRevision: committed.revision,
                    committedRoot: rootValue.value,
                    committedCharacter: replacement,
                    deleting: false,
                    characterId,
                    residentAfterCommit,
                    reason,
                })
            }
            this.finishCharacterMutation({
                revision: committed.revision,
                root: this.capture().root,
                characterId,
                kind: 'replace',
                character: replacement,
            }, rootValue.value)
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    upsertPersistentCompleteCharacter(
        characterId: string,
        reason: string,
        createOrMutate: PersistentCompleteCharacterUpsert,
        options: PersistentCompleteCharacterUpsertOptions = {},
    ): Promise<boolean> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const residentBefore = this.captureResidentCharacter(characterId)
            const revision = this.revision
            const [rootValue, characterValue] = await Promise.all([
                this.dependencies.store.readRoot(),
                this.dependencies.store.readCharacter(characterId),
            ])
            this.assertReadRevision(revision, rootValue.revision)
            if (characterValue) {
                this.assertReadRevision(revision, characterValue.revision)
                if (characterValue.value.chaId !== characterId) {
                    throw new Error(`Character ${characterId} returned mismatched detail`)
                }
            }
            const current = characterValue
                ? await this.readCompleteCharacter(characterId, revision, characterValue.value)
                : null
            const replacement = canonicalClone(await createOrMutate(current))
            this.assertResidentCharacterUnchanged(characterId, residentBefore)
            if (replacement.chaId !== characterId) {
                throw new Error(`Upserted character ID must remain ${characterId}`)
            }

            const commit: WorkingSetCommit = { expectedRevision: revision }
            let committedRoot = canonicalClone(rootValue.value)
            const mutatedRoot = canonicalClone(rootValue.value)
            if (current) {
                commit.replaceCharacter = replacement
            } else {
                if (options.includeInCharacterOrder !== false) {
                    appendCharacterIdToOrder(mutatedRoot, characterId)
                    committedRoot = rebaseRootMutation(
                        rootValue.value,
                        mutatedRoot,
                        this.capture().root,
                    )
                    commit.root = committedRoot
                }
                commit.addCharacter = replacement
            }
            const committed = await this.dependencies.store.commit(commit)
            const residentAfterCommit = this.captureResidentCharacter(characterId)
            if (!this.residentCharactersMatch(residentBefore, residentAfterCommit)) {
                return this.compensateConcurrentResidentCharacter({
                    committedRevision: committed.revision,
                    committedRoot,
                    committedCharacter: replacement,
                    deleting: false,
                    characterId,
                    residentAfterCommit,
                    reason,
                })
            }
            this.finishCharacterMutation({
                revision: committed.revision,
                root: current || options.includeInCharacterOrder === false
                    ? this.capture().root
                    : rebaseRootMutation(
                        rootValue.value,
                        mutatedRoot,
                        this.capture().root,
                    ),
                characterId,
                kind: current ? 'replace' : 'add',
                character: replacement,
            }, committedRoot)
            await this.finishExplicitCommit(committed.revision)
            return true
        })
    }

    readPersistentCharacterDetail(
        characterId: string,
        reason: string,
    ): Promise<CharacterDetail | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readCharacter(characterId)
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            return canonicalClone(value.value)
        })
    }

    readPersistentCompleteCharacter(
        characterId: string,
        reason: string,
    ): Promise<CompleteCharacter | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readCharacter(characterId)
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            return this.readCompleteCharacter(characterId, revision, value.value)
        })
    }

    readPersistentConversation(
        characterId: string,
        conversationId: string,
        reason: string,
    ): Promise<Chat | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const value = await this.dependencies.store.readConversation(
                characterId,
                conversationId,
            )
            if (!value) return null
            this.assertReadRevision(revision, value.revision)
            if (value.value.id !== conversationId) {
                throw new Error(`Conversation ${conversationId} returned mismatched content`)
            }
            return canonicalClone(value.value)
        })
    }

    readPersistentConversationAt(
        characterId: string,
        orderedPosition: number,
        reason: string,
    ): Promise<Chat | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!Number.isInteger(orderedPosition) || orderedPosition < 0) {
            return Promise.reject(new RangeError('Conversation position must be a nonnegative integer'))
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            return this.readConversationAtRevision(
                characterId,
                orderedPosition,
                this.revision,
            )
        })
    }

    readPersistentSelectedConversation(
        characterId: string,
        reason: string,
    ): Promise<PersistentSelectedConversation | null> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const character = await this.dependencies.store.readCharacter(characterId)
            if (!character) return null
            this.assertReadRevision(revision, character.revision)
            if (character.value.chaId !== characterId) {
                throw new Error(`Character ${characterId} returned mismatched detail`)
            }
            const orderedPosition = character.value.chatPage ?? 0
            if (!Number.isInteger(orderedPosition) || orderedPosition < 0) {
                throw new RangeError('Selected conversation position must be a nonnegative integer')
            }
            const conversation = await this.readConversationAtRevision(
                characterId,
                orderedPosition,
                revision,
            )
            return {
                character: canonicalClone(character.value),
                conversation,
            }
        })
    }

    materializePersistentDatabaseSnapshot(reason: string): Promise<Database> {
        return this.materializePersistentDatabaseSnapshotWithRevision(reason).then(
            (snapshot) => snapshot.database,
        )
    }

    capturePersistentMutationToken(reason: string): Promise<PersistentMutationToken> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            return {
                revision: this.revision,
                mutationGeneration: this.dirtyGeneration,
            }
        })
    }

    acquireDestructiveReplacementFence(expected: PersistentMutationToken): Promise<symbol> {
        this.assertInitialized()
        if (this.destructiveReplacementFence) throw new PersistentMutationFencedError()
        const owner = Symbol('destructive-persistent-replacement')
        this.destructiveReplacementFence = {
            owner,
            state: 'acquiring',
            acceptsPostPublicationDirty: false,
            queuedPostPublicationDirty: false,
            blockedPrePublicationDirty: false,
        }
        this.cancelDebounce()
        return this.enqueue(async () => {
            try {
                await this.flushIterations('destructive-persistent-replacement', true)
                if (this.revision !== expected.revision) {
                    throw new RevisionConflictError(expected.revision, this.revision)
                }
                if (this.dirtyGeneration !== expected.mutationGeneration) {
                    throw new Error(
                        `Expected mutation generation ${expected.mutationGeneration}, ` +
                        `but current generation is ${this.dirtyGeneration}`,
                    )
                }
                if (this.destructiveReplacementFence?.owner !== owner) {
                    throw new Error('Destructive persistent replacement fence ownership changed')
                }
                this.destructiveReplacementFence.state = 'held'
                return owner
            } catch (error) {
                if (this.destructiveReplacementFence?.owner === owner) {
                    this.destructiveReplacementFence = null
                }
                throw error
            }
        })
    }

    assertDestructiveReplacementFence(owner: symbol): void {
        if (
            this.destructiveReplacementFence?.owner !== owner ||
            this.destructiveReplacementFence.state !== 'held'
        ) {
            throw new Error('Destructive persistent replacement fence is not held')
        }
        if (this.destructiveReplacementFence.blockedPrePublicationDirty) {
            throw new PersistentMutationFencedError()
        }
        if (
            !this.destructiveReplacementFence.acceptsPostPublicationDirty
            && !this.captureMatchesBaseline()
        ) {
            this.destructiveReplacementFence.blockedPrePublicationDirty = true
            throw new PersistentMutationFencedError()
        }
    }

    releaseDestructiveReplacementFence(owner: symbol): void {
        if (
            this.destructiveReplacementFence?.owner !== owner
            || this.destructiveReplacementFence.state !== 'held'
        ) {
            throw new Error('Destructive persistent replacement fence is not held')
        }
        const queuedPostPublicationDirty =
            this.destructiveReplacementFence.queuedPostPublicationDirty
        this.destructiveReplacementFence = null
        if (queuedPostPublicationDirty && !this.flushPromise && !this.additionPromise) {
            this.armDebounce()
        }
    }

    materializePersistentDatabaseSnapshotWithRevision(
        reason: string,
    ): Promise<PersistentDatabaseSnapshot> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        this.cancelDebounce()
        return this.enqueue(async () => {
            await this.flushIterations(reason, true)
            const revision = this.revision
            const generation = this.dirtyGeneration
            const navigationGeneration = this.dependencies.getNavigationGeneration?.()
            const database = await this.dependencies.store.materializeDatabase(revision)
            if (
                this.revision !== revision ||
                this.dirtyGeneration !== generation ||
                this.dependencies.getNavigationGeneration?.() !== navigationGeneration
            ) {
                throw new Error('Working set changed during persistent database materialization')
            }
            return {
                database: canonicalDatabaseClone(database),
                revision,
                mutationGeneration: generation,
            }
        })
    }

    publishCurrentOfficialRevision(): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!this.dependencies.officialPublisher) return Promise.resolve()
        return this.enqueue(async () => {
            await this.applyDeferredPublication()
            this.pendingPublicationRevision ??= this.revision
            await this.publishPendingRevision()
        })
    }

    get hasPendingOfficialPublication(): boolean {
        return this.pendingPublicationRevision !== null || this.deferredPublicationRevision !== null
    }

    commitCharacterAddition(request: CharacterAdditionRequest, reason: string): Promise<void> {
        this.assertInitialized()
        this.assertPersistentMutationAllowed()
        if (!request.characterId) {
            throw new Error('Character addition requires a nonempty character ID')
        }
        const inFlight = this.additionPromise
        if (inFlight) {
            // Two imports can overlap, so the later one waits instead of failing.
            return inFlight
                .catch(() => undefined)
                .then(() => this.commitCharacterAddition(request, reason))
        }
        if (this.pendingCharacterAddition) {
            // A previous addition failed and left its work pending; retry it before this import.
            return this.flushPendingData(reason)
                .then(() => this.commitCharacterAddition(request, reason))
        }
        if (this.reservedCharacterAddition) {
            throw new Error('A character addition is already pending')
        }
        const reserved: ReservedCharacterAddition = {
            request,
            token: {},
        }
        this.reservedCharacterAddition = reserved
        const promise = this.enqueue(async () => {
            if (this.reservedCharacterAddition === reserved) {
                this.beginReservedAddition(reserved)
            }
            if (this.pendingCharacterAddition?.token !== reserved.token) return
            await this.flushIterations(reason, true)
        })
        this.additionPromise = promise
        this.reportActivePromise()
        void promise.then(
            () => {
                if (this.additionPromise === promise) {
                    this.additionPromise = null
                    this.reportActivePromise()
                }
            },
            () => {
                if (this.additionPromise === promise) {
                    this.additionPromise = null
                    this.reportActivePromise()
                }
            },
        )
        return promise
    }

    private enqueue<T>(operation: () => Promise<T>): Promise<T> {
        this.queuedOperationCount += 1
        this.notifyOperationStateChange()
        const run = async (): Promise<T> => {
            try {
                return await operation()
            } finally {
                this.queuedOperationCount -= 1
                this.notifyOperationStateChange()
            }
        }
        const result = this.operationTail.then(run, run)
        this.operationTail = result.then(
            () => undefined,
            () => undefined,
        )
        return result
    }

    private waitForOperationStateChange(): Promise<void> {
        return new Promise((resolve) => this.operationStateWaiters.add(resolve))
    }

    private notifyOperationStateChange(): void {
        const waiters = [...this.operationStateWaiters]
        this.operationStateWaiters.clear()
        for (const resolve of waiters) resolve()
    }

    private async flushIterations(_reason: string, publishOfficial: boolean): Promise<void> {
        if (publishOfficial && this.deferredPublicationRevision !== null) {
            await this.applyDeferredPublication()
        }
        if (publishOfficial && this.pendingPublicationCleanup.size > 0) {
            await this.retryPublicationCleanup()
        }
        if (this.pendingResidentCompensations.length > 0) {
            await this.retryPendingResidentCompensations(publishOfficial)
        }
        while (true) {
            const generation = this.dirtyGeneration
            const captured = this.capture()
            const selectionSwitched =
                captured.character !== null &&
                this.characterBaselineId !== null &&
                captured.character.chaId !== this.characterBaselineId
            const detached =
                !captured.character || selectionSwitched ? this.captureDetachedCharacter() : null
            const addition = this.capturePendingAddition()
            const commit: WorkingSetCommit = { expectedRevision: this.revision }
            if (captured.rootCanonical !== this.rootBaseline) commit.root = captured.root
            if (
                captured.pluginStorage !== null &&
                captured.pluginStorageCanonical !== this.pluginStorageBaseline
            ) {
                commit.pluginStorage = diffPluginStorage(
                    this.pluginStorageBaseline,
                    captured.pluginStorage,
                )
            }
            if (
                captured.presetsCanonical !== null &&
                captured.presetsCanonical !== this.presetsBaseline
            ) {
                commit.replacePresets = captured.presets
            }
            if (detached) {
                commit.replaceCharacter = detached.character
            } else if (
                captured.character &&
                captured.characterCanonical !== this.characterBaseline
            ) {
                const conversations = this.diffSelectedConversations(captured)
                if (conversations) commit.conversations = conversations
                else commit.replaceCharacter = captured.conversationStubIds.size > 0
                    ? await this.reconstructCapturedCharacter(captured)
                    : captured.character
            }

            let replacementIsAddition = false
            if (addition) {
                if (!addition.pending.locallyAdded) {
                    commit.addCharacter = addition.character
                } else if (
                    addition.canonical !== addition.pending.baseline &&
                    !commit.replaceCharacter &&
                    !commit.conversations
                ) {
                    commit.replaceCharacter = addition.character
                    replacementIsAddition = true
                }
            }

            if (
                commit.root ||
                commit.pluginStorage ||
                commit.replacePresets ||
                commit.replaceCharacter ||
                commit.addCharacter ||
                commit.conversations
            ) {
                const committed = await this.dependencies.store.commit(commit)
                this.currentRevision = committed.revision
                if (commit.root) this.rootBaseline = captured.rootCanonical
                if (commit.pluginStorage) {
                    this.pluginStorageBaseline = captured.pluginStorageCanonical
                }
                if (commit.replacePresets) this.presetsBaseline = captured.presetsCanonical
                if (commit.replaceCharacter && !replacementIsAddition) {
                    if (detached && captured.character) {
                        this.characterBaseline = detached.canonical
                        this.characterBaselineId = detached.character.chaId
                    } else {
                        this.setCharacterBaseline(captured)
                    }
                    if (
                        addition &&
                        commit.replaceCharacter.chaId === addition.pending.characterId
                    ) {
                        addition.pending.baseline = detached
                            ? detached.canonical
                            : captured.characterCanonical!
                    }
                }
                if (commit.conversations && captured.character) {
                    this.setCharacterBaseline(captured)
                    if (addition && captured.character.chaId === addition.pending.characterId) {
                        addition.pending.baseline = captured.characterCanonical!
                    }
                }
                if (replacementIsAddition && addition) {
                    addition.pending.baseline = addition.canonical
                }
                if (commit.addCharacter && addition) {
                    addition.pending.locallyAdded = true
                    addition.pending.baseline = addition.canonical
                }
                this.dependencies.onLocalRevision?.(committed.revision)
                if (this.dependencies.officialPublisher) {
                    if (publishOfficial) await this.stagePublication(committed.revision)
                    else this.deferPublication(committed.revision)
                }
            }

            if (!captured.character && !commit.replaceCharacter) this.setCharacterBaseline(captured)

            const current = this.capture()
            const currentAddition = this.capturePendingAddition()
            if (
                generation === this.dirtyGeneration &&
                current.rootCanonical === this.rootBaseline &&
                (current.pluginStorageCanonical === null ||
                    current.pluginStorageCanonical === this.pluginStorageBaseline) &&
                (current.presetsCanonical === null ||
                    current.presetsCanonical === this.presetsBaseline) &&
                current.characterCanonical === this.characterBaseline &&
                (!currentAddition ||
                    (currentAddition.pending.locallyAdded &&
                        currentAddition.canonical === currentAddition.pending.baseline))
            ) {
                if (publishOfficial && this.pendingPublicationRevision !== null) {
                    const delay = this.officialPublishDelayMs()
                    if (delay <= 0) {
                        await this.publishPendingRevision()
                        continue
                    }
                    this.armOfficialPublishRetry(delay)
                    this.pendingCharacterAddition = null
                    this.pendingByteCount = 0
                    return
                }
                if (!publishOfficial && this.hasPendingOfficialPublication && !this.publicationInProgress) {
                    this.armOfficialPublishRetry(this.officialPublishDelayMs())
                }
                this.pendingCharacterAddition = null
                this.pendingByteCount = 0
                this.lastBackgroundErrorMessage = null
                return
            }
        }
    }

    /** Marks a committed revision for official publication, superseding any stale pinned one. */
    private async stagePublication(revision: DataRevision): Promise<void> {
        if (this.pendingPublicationRevision !== revision && this.pendingPublication) {
            const stale = this.pendingPublication
            this.pendingPublication = null
            await this.disposeOrQueuePublication(stale)
        }
        this.pendingPublicationRevision = revision
    }

    private deferPublication(revision: DataRevision): void {
        if (!this.publicationInProgress && !this.pendingPublication) {
            this.pendingPublicationRevision = revision
            this.deferredPublicationRevision = null
            return
        }
        this.deferredPublicationRevision = revision
    }

    private async applyDeferredPublication(): Promise<void> {
        const revision = this.deferredPublicationRevision
        if (revision === null || this.publicationInProgress) return
        this.deferredPublicationRevision = null
        await this.stagePublication(revision)
    }

    private async runReplacement(
        candidate: Database,
        before: CapturedState,
        capturedGeneration: number,
        supersededAdditionToken: object | null,
        reason: string,
        options: PersistentReplacementOptions,
    ): Promise<void> {
        await this.retryPendingResidentCompensations(true)
        const pendingExpectationError = this.replacementExpectationError(options)
        if (pendingExpectationError) throw pendingExpectationError
        this.rebaseReplacementPublication(candidate, before, this.capture(), false)
        const replaced = await this.dependencies.store.replaceFromDatabase(
            candidate,
            options.expectedRevision ?? this.revision,
        )
        const live = this.capture()
        const stalePublication = options.publishOfficial ? null : this.pendingPublication
        if (!options.publishOfficial) {
            this.pendingPublication = null
            this.pendingPublicationRevision = null
            this.cancelOfficialPublishRetry()
        }
        if (this.pendingCharacterAddition?.token === supersededAdditionToken) {
            this.pendingCharacterAddition = null
        }
        if (this.reservedCharacterAddition?.token === supersededAdditionToken) {
            this.reservedCharacterAddition = null
        }
        this.currentRevision = replaced.revision
        const candidateCapture = this.captureDatabase(candidate)
        this.rootBaseline = candidateCapture.rootCanonical
        this.pluginStorageBaseline = candidateCapture.pluginStorageCanonical
        this.presetsBaseline = candidateCapture.presetsCanonical
        this.setCharacterBaseline(candidateCapture)

        const rebased = this.rebaseReplacementPublication(candidate, before, live, true)
        const published = rebased.database
        const compensation = rebased.compensation
            ? canonicalClone(rebased.compensation)
            : null

        this.dependencies.replaceDatabase(published)
        if (stalePublication) {
            await this.disposeOrQueuePublication(stalePublication)
        }
        if (compensation) {
            this.dirtyGeneration++
            const compensated = await this.dependencies.store.commit({
                expectedRevision: replaced.revision,
                ...compensation,
            })
            this.currentRevision = compensated.revision
            if (compensation.root) {
                this.rootBaseline = canonicalJson(compensation.root)
            }
            if (compensation.pluginStorage) {
                this.pluginStorageBaseline = pluginStorageJson(
                    published.pluginCustomStorage ?? {},
                )
            }
            if (compensation.replacePresets) {
                this.presetsBaseline = canonicalJson(compensation.replacePresets)
            }
            if (
                compensation.replaceCharacter &&
                this.capture().character?.chaId === compensation.replaceCharacter.chaId
            ) {
                this.characterBaseline = canonicalJson(compensation.replaceCharacter)
                this.characterBaselineId = compensation.replaceCharacter.chaId
            }
            this.dependencies.onLocalRevision?.(compensated.revision)
            if (!this.flushPromise && !this.additionPromise) this.armDebounce()
            if (options.publishOfficial && this.dependencies.officialPublisher) {
                await this.stagePublication(compensated.revision)
                this.armOfficialPublishRetry(this.officialPublishDelayMs())
            }
            throw new Error(`Concurrent live changes conflicted with replacement: ${reason}`)
        }

        this.dependencies.onLocalRevision?.(replaced.revision)
        if (options.publishOfficial && this.dependencies.officialPublisher) {
            await this.stagePublication(replaced.revision)
            const delay = this.officialPublishDelayMs()
            if (delay <= 0) await this.publishPendingRevision()
            else this.armOfficialPublishRetry(delay)
        }

        if (this.dirtyGeneration === capturedGeneration) {
            this.cancelDebounce()
            this.pendingByteCount = 0
        } else if (!this.flushPromise && !this.additionPromise) {
            this.armDebounce()
        }
    }

    private async readCompleteCharacter(
        characterId: string,
        revision: DataRevision,
        detail: CharacterDetail,
    ): Promise<CompleteCharacter> {
        const conversations: Chat[] = []
        let cursor: string | undefined
        do {
            const page = await this.dependencies.store.queryConversations({
                characterId,
                order: 'configured',
                limit: 100,
                cursor,
            })
            this.assertReadRevision(revision, page.revision)
            for (const summary of page.items) {
                if (summary.characterId !== characterId) {
                    throw new Error(`Conversation ${summary.id} belongs to another character`)
                }
                const value = await this.dependencies.store.readConversation(
                    characterId,
                    summary.id,
                )
                if (!value) throw new Error(`Conversation ${summary.id} was not found`)
                this.assertReadRevision(revision, value.revision)
                if (value.value.id !== summary.id) {
                    throw new Error(`Conversation ${summary.id} returned mismatched content`)
                }
                conversations.push(canonicalClone(value.value))
            }
            cursor = page.nextCursor
        } while (cursor)
        return {
            ...canonicalClone(detail),
            chats: conversations,
        } as CompleteCharacter
    }

    private async readConversationAtRevision(
        characterId: string,
        orderedPosition: number,
        revision: DataRevision,
    ): Promise<Chat | null> {
        const page = await this.dependencies.store.queryConversations({
            characterId,
            order: 'configured',
            limit: 1,
            cursor: orderedPosition === 0 ? undefined : String(orderedPosition),
        })
        this.assertReadRevision(revision, page.revision)
        const summary = page.items[0]
        if (!summary) return null
        if (summary.characterId !== characterId) {
            throw new Error(
                `Conversation position ${orderedPosition} returned mismatched content`,
            )
        }
        const value = await this.dependencies.store.readConversation(characterId, summary.id)
        if (!value) throw new Error(`Conversation ${summary.id} was not found`)
        this.assertReadRevision(revision, value.revision)
        if (value.value.id !== summary.id) {
            throw new Error(`Conversation ${summary.id} returned mismatched content`)
        }
        return canonicalClone(value.value)
    }

    private captureResidentCharacter(characterId: string): {
        character: CompleteCharacter
        canonical: string
        conversationStubIds: ReadonlySet<string>
    } | null {
        const character = this.dependencies.captureCharacter(characterId)
        if (!character) return null
        const conversationStubIds = new Set(
            character.chats
                .filter(isConversationSummaryStub)
                .map((conversation) => conversation.id)
                .filter((id): id is string => Boolean(id)),
        )
        const canonical = canonicalJson(character)
        return {
            character: JSON.parse(canonical) as CompleteCharacter,
            canonical,
            conversationStubIds,
        }
    }

    private residentCharactersMatch(
        left: { canonical: string } | null,
        right: { canonical: string } | null,
    ): boolean {
        return left?.canonical === right?.canonical
    }

    private assertResidentCharacterUnchanged(
        characterId: string,
        before: { canonical: string } | null,
    ): void {
        if (!this.residentCharactersMatch(before, this.captureResidentCharacter(characterId))) {
            throw new Error(`Resident character changed during persistent mutation: ${characterId}`)
        }
    }

    private mergeCommittedDetailWithResident(
        detail: CharacterDetail,
        resident: CompleteCharacter | null,
    ): CompleteCharacter {
        return {
            ...canonicalClone(detail),
            chats: canonicalClone(resident?.chats ?? []),
        } as CompleteCharacter
    }

    private async compensateConcurrentResidentCharacter(options: {
        committedRevision: DataRevision
        committedRoot: RootDatabase
        committedCharacter: CompleteCharacter | null
        deleting: boolean
        characterId: string
        residentAfterCommit: { character: CompleteCharacter; canonical: string } | null
        reason: string
    }): Promise<never> {
        this.currentRevision = options.committedRevision
        this.dirtyGeneration++
        this.rootBaseline = canonicalJson(options.committedRoot)
        const selected = this.capture().character
        if (selected?.chaId === options.characterId) {
            this.characterBaseline = options.committedCharacter
                ? canonicalJson(options.committedCharacter)
                : null
            this.characterBaselineId = options.committedCharacter?.chaId ?? null
        }
        this.dependencies.onLocalRevision?.(options.committedRevision)

        let resident = options.residentAfterCommit
        let addCharacter = options.deleting
        let targetSettled = resident === null
        for (
            let attempt = 0;
            resident && attempt < CONCURRENT_CHARACTER_COMPENSATION_ATTEMPTS;
            attempt++
        ) {
            const root = this.capture().root
            const compensation: WorkingSetCommit = {
                expectedRevision: this.revision,
                ...(addCharacter
                    ? { addCharacter: resident.character }
                    : { replaceCharacter: resident.character }),
            }
            if (canonicalJson(root) !== this.rootBaseline) compensation.root = root
            const compensated = await this.dependencies.store.commit(compensation)
            this.currentRevision = compensated.revision
            this.rootBaseline = canonicalJson(root)
            this.dependencies.onLocalRevision?.(compensated.revision)
            const next = this.captureResidentCharacter(options.characterId)
            if (this.residentCharactersMatch(resident, next)) {
                targetSettled = true
                if (this.capture().character?.chaId === options.characterId) {
                    this.characterBaseline = resident.canonical
                    this.characterBaselineId = options.characterId
                }
                break
            }
            resident = next
            targetSettled = resident === null
            addCharacter = false
        }

        if (!targetSettled && resident) {
            this.enqueueResidentCompensation(options.characterId)
        }

        const current = this.capture()
        const currentAddition = this.capturePendingAddition()
        const isClean = targetSettled &&
            current.rootCanonical === this.rootBaseline &&
            (current.pluginStorageCanonical === null ||
                current.pluginStorageCanonical === this.pluginStorageBaseline) &&
            (current.presetsCanonical === null ||
                current.presetsCanonical === this.presetsBaseline) &&
            current.characterCanonical === this.characterBaseline &&
            (!currentAddition ||
                (currentAddition.pending.locallyAdded &&
                    currentAddition.canonical === currentAddition.pending.baseline))
        if (isClean) {
            this.cancelDebounce()
            this.pendingByteCount = 0
        } else if (!this.flushPromise && !this.additionPromise) {
            this.armDebounce()
        }
        await this.finishExplicitCommit(this.revision)
        throw new Error(
            `Resident character changed during persistent mutation: ${options.characterId}`,
        )
    }

    private async retryPendingResidentCompensations(publishOfficial: boolean): Promise<void> {
        while (this.pendingResidentCompensations.length > 0) {
            const pending = this.pendingResidentCompensations[0]
            const resident = this.captureResidentCharacter(pending.characterId)
            if (!resident) {
                if (this.pendingResidentCompensations[0] === pending) {
                    this.pendingResidentCompensations.shift()
                }
                continue
            }

            let committed: { revision: DataRevision }
            try {
                committed = await this.dependencies.store.commit({
                    expectedRevision: this.revision,
                    replaceCharacter: await this.reconstructResidentCharacter(resident),
                })
            } catch (error) {
                this.armDebounce()
                throw error
            }
            this.currentRevision = committed.revision
            this.dependencies.onLocalRevision?.(committed.revision)
            if (this.dependencies.officialPublisher) {
                if (publishOfficial) await this.stagePublication(committed.revision)
                else this.deferPublication(committed.revision)
            }

            const current = this.captureResidentCharacter(pending.characterId)
            if (!this.residentCharactersMatch(resident, current)) {
                this.armDebounce()
                throw new Error(
                    `Resident character changed during deferred compensation: ${pending.characterId}`,
                )
            }
            if (this.pendingResidentCompensations[0] === pending) {
                this.pendingResidentCompensations.shift()
            }
            if (this.capture().character?.chaId === pending.characterId) {
                this.characterBaseline = resident.canonical
                this.characterBaselineId = pending.characterId
            }
        }
    }

    private finishCharacterMutation(
        result: PersistentCharacterMutationResult,
        committedRoot: RootDatabase,
        options: { preservePendingWork?: boolean } = {},
    ): void {
        this.currentRevision = result.revision
        this.dirtyGeneration++
        this.rootBaseline = canonicalJson(committedRoot)
        this.dependencies.publishCharacterMutation?.(result)
        const published = this.capture()
        if (
            published.character?.chaId === result.characterId ||
            (result.kind === 'delete' && !published.character) ||
            (this.dependencies.publishCharacterMutation !== undefined &&
                result.relatedCharacters?.some(
                    (detail) => detail.chaId === published.character?.chaId,
                ))
        ) {
            this.setCharacterBaseline(published)
        }
        this.dependencies.onLocalRevision?.(result.revision)
        if (!options.preservePendingWork) this.pendingByteCount = 0
        this.lastBackgroundErrorMessage = null
    }

    private enqueueResidentCompensation(characterId: string): void {
        if (this.pendingResidentCompensations.some(
            (pending) => pending.characterId === characterId,
        )) return
        this.pendingResidentCompensations.push({ characterId })
    }

    private removeGroupCharacterReference(
        character: CharacterDetail | CompleteCharacter,
        characterId: string,
    ): boolean {
        if (character.type !== 'group') return false
        const group = character as Omit<groupChat, 'chats'> | groupChat
        const retainedIndices = group.characters
            .map((id, index) => ({ id, index }))
            .filter(({ id }) => id !== characterId)
        if (retainedIndices.length === group.characters.length) return false
        group.characters = retainedIndices.map(({ id }) => id)
        group.characterTalks = retainedIndices.map(
            ({ index }) => group.characterTalks?.[index] ?? 1 / 6 * 4,
        )
        group.characterActive = retainedIndices.map(
            ({ index }) => group.characterActive?.[index] ?? true,
        )
        return true
    }

    private async finishExplicitCommit(revision: DataRevision): Promise<void> {
        if (!this.dependencies.officialPublisher) return
        await this.stagePublication(revision)
        const delay = this.officialPublishDelayMs()
        if (delay <= 0) await this.publishPendingRevision()
        else this.armOfficialPublishRetry(delay)
    }

    private setCharacterBaseline(captured: CapturedState): void {
        this.characterBaseline = captured.characterCanonical
        this.characterBaselineId = captured.character?.chaId ?? null
    }

    /**
     * Builds conversation-level mutations when only chat content changed for the tracked
     * selected character. Returns null whenever a full character replacement is required:
     * detail changes, added/removed/reordered chats, or chats the mutation channel cannot
     * address safely (missing or duplicate ids).
     */
    private diffSelectedConversations(captured: CapturedState): ConversationMutation[] | null {
        const character = captured.character
        if (!character || this.characterBaseline === null) return null
        if (this.characterBaselineId !== character.chaId) return null
        const baseline = JSON.parse(this.characterBaseline) as CompleteCharacter
        const capturedChats = character.chats
        const baselineChats = baseline.chats
        if (!Array.isArray(capturedChats) || !Array.isArray(baselineChats)) return null
        if (capturedChats.length !== baselineChats.length) return null
        const { chats: _capturedChats, ...capturedDetail } = character
        const { chats: _baselineChats, ...baselineDetail } = baseline
        if (JSON.stringify(capturedDetail) !== JSON.stringify(baselineDetail)) return null

        const mutations: ConversationMutation[] = []
        const seenIds = new Set<string>()
        for (let index = 0; index < capturedChats.length; index++) {
            const capturedChat = capturedChats[index]
            const baselineChat = baselineChats[index]
            const conversationId = capturedChat?.id
            if (!conversationId || conversationId !== baselineChat?.id) return null
            if (seenIds.has(conversationId)) return null
            seenIds.add(conversationId)
            if (JSON.stringify(capturedChat) === JSON.stringify(baselineChat)) continue
            if (captured.conversationStubIds.has(conversationId)) return null
            if (!Array.isArray(capturedChat.message) || !Array.isArray(baselineChat.message)) {
                return null
            }
            const { message, ...conversation } = capturedChat
            const range = messageReplaceRange(baselineChat.message, message)
            mutations.push({
                type: 'replace-range',
                characterId: character.chaId,
                conversationId,
                ...range,
                conversation,
            })
        }
        return mutations.length > 0 ? mutations : null
    }

    /** Returns the last tracked character when the selection moved away before its edits were committed. */
    private captureDetachedCharacter(): { character: CompleteCharacter; canonical: string } | null {
        if (this.characterBaseline === null || this.characterBaselineId === null) return null
        const retained = this.dependencies.captureCharacter(this.characterBaselineId)
        if (!retained) return null
        const canonical = canonicalJson(retained)
        if (canonical === this.characterBaseline) return null
        return { character: JSON.parse(canonical) as CompleteCharacter, canonical }
    }

    private capture(): CapturedState {
        const capturedRoot = this.dependencies.captureRoot() as RootDatabase & {
            characters?: Database['characters']
            botPresets?: botPreset[]
            pluginCustomStorage?: Database['pluginCustomStorage']
        }
        const {
            characters: _characters,
            botPresets: legacyPresets,
            pluginCustomStorage: legacyPluginStorage,
            ...rootValue
        } = capturedRoot
        const rootCanonical = canonicalJson(rootValue)
        const pluginStorageValue = this.dependencies.capturePluginStorage
            ? this.dependencies.capturePluginStorage()
            : legacyPluginStorage ?? null
        const pluginStorageCanonical = pluginStorageValue === null
            ? null
            : pluginStorageJson(pluginStorageValue)
        const presetsValue = this.dependencies.capturePresets
            ? this.dependencies.capturePresets()
            : legacyPresets ?? []
        const presetsCanonical = presetsValue === null ? null : canonicalJson(presetsValue)
        const characterValue = this.dependencies.captureSelectedCharacter()
        const conversationStubIds = new Set(
            characterValue?.chats
                .filter(isConversationSummaryStub)
                .map((conversation) => conversation.id)
                .filter((id): id is string => Boolean(id)) ?? [],
        )
        const characterCanonical = characterValue ? canonicalJson(characterValue) : null
        return {
            root: JSON.parse(rootCanonical) as RootDatabase,
            rootCanonical,
            pluginStorage: pluginStorageCanonical === null
                ? null
                : JSON.parse(pluginStorageCanonical) as Database['pluginCustomStorage'],
            pluginStorageCanonical,
            presets: presetsCanonical === null
                ? null
                : JSON.parse(presetsCanonical) as botPreset[],
            presetsCanonical,
            character: characterCanonical
                ? (JSON.parse(characterCanonical) as CompleteCharacter)
                : null,
            characterCanonical,
            conversationStubIds,
        }
    }

    private captureDatabase(database: Database): CapturedState {
        const {
            characters,
            botPresets,
            pluginCustomStorage,
            ...rootValue
        } = database
        const rootCanonical = canonicalJson(rootValue)
        const presetsCanonical = canonicalJson(botPresets ?? [])
        const pluginStorageUnavailable =
            !Object.prototype.hasOwnProperty.call(database, 'pluginCustomStorage') &&
            this.dependencies.isIncompleteWorkingSet?.(database) === true
        const pluginStorageCanonical = pluginStorageUnavailable
            ? null
            : pluginStorageJson(pluginCustomStorage ?? {})
        const selectedId = this.dependencies.captureSelectedCharacter()?.chaId
        const character = selectedId ? characters.find((candidate) => candidate.chaId === selectedId) ?? null : null
        const characterCanonical = character ? canonicalJson(character) : null
        return {
            root: JSON.parse(rootCanonical) as RootDatabase,
            rootCanonical,
            pluginStorage: pluginStorageCanonical === null
                ? null
                : JSON.parse(pluginStorageCanonical) as Database['pluginCustomStorage'],
            pluginStorageCanonical,
            presets: JSON.parse(presetsCanonical) as botPreset[],
            presetsCanonical,
            character: characterCanonical
                ? JSON.parse(characterCanonical) as CompleteCharacter
                : null,
            characterCanonical,
            conversationStubIds: new Set(),
        }
    }

    private async reconstructCapturedCharacter(captured: CapturedState): Promise<CompleteCharacter> {
        return this.reconstructCharacterWithStubBodies(
            captured.character!,
            captured.conversationStubIds,
        )
    }

    private async reconstructResidentCharacter(
        resident: NonNullable<ReturnType<SaveCoordinator['captureResidentCharacter']>>,
    ): Promise<CompleteCharacter> {
        if (resident.conversationStubIds.size === 0) return resident.character
        return this.reconstructCharacterWithStubBodies(
            resident.character,
            resident.conversationStubIds,
        )
    }

    private async reconstructCharacterWithStubBodies(
        character: CompleteCharacter,
        conversationStubIds: ReadonlySet<string>,
    ): Promise<CompleteCharacter> {
        const { chats: _chats, ...detail } = character
        const authoritative = await this.readCompleteCharacter(
            character.chaId,
            this.revision,
            detail,
        )
        const authoritativeById = new Map(
            authoritative.chats.map((conversation) => [conversation.id, conversation]),
        )
        return {
            ...character,
            chats: character.chats.map((conversation) => {
                if (!conversation.id || !conversationStubIds.has(conversation.id)) {
                    return conversation
                }
                const full = authoritativeById.get(conversation.id)
                if (!full) {
                    throw new Error(
                        `Conversation ${conversation.id} was not found during resident reconstruction`,
                    )
                }
                return {
                    ...full,
                    id: conversation.id,
                    name: conversation.name,
                    folderId: conversation.folderId,
                    bindedPersona: conversation.bindedPersona,
                    lastDate: conversation.lastDate,
                }
            }),
        } as CompleteCharacter
    }

    private capturePendingAddition(): {
        pending: PendingCharacterAddition
        character: CompleteCharacter
        canonical: string
    } | null {
        const pending = this.pendingCharacterAddition
        if (!pending) return null
        const value = this.dependencies.captureCharacter(pending.characterId)
        if (!value || value.chaId !== pending.characterId) {
            throw new Error(`Installed character ${pending.characterId} is not available`)
        }
        const canonical = canonicalJson(value)
        return { pending, character: JSON.parse(canonical) as CompleteCharacter, canonical }
    }

    private beginReservedAddition(reserved: ReservedCharacterAddition): void {
        const request = reserved.request
        if (!request) return
        try {
            request.install()
        } finally {
            reserved.request = null
            if (this.reservedCharacterAddition === reserved) this.reservedCharacterAddition = null
        }
        this.pendingCharacterAddition = {
            characterId: request.characterId,
            token: reserved.token,
            locallyAdded: false,
            baseline: null,
        }
        this.dirtyGeneration++
        const bytes = Number.isFinite(request.estimatedBytes) && request.estimatedBytes > 0
            ? request.estimatedBytes
            : 0
        this.pendingByteCount += bytes
    }

    private armDebounce(): void {
        if (this.debounceHandle !== undefined) return
        this.debounceHandle = this.clock.setTimeout(() => {
            this.debounceHandle = undefined
            this.startBackgroundFlush('debounce')
        }, SAVE_DEBOUNCE_MS)
    }

    private startBackgroundFlush(reason: string): void {
        void this.flushPendingData(reason).catch((error) => this.reportBackgroundError(error))
    }

    private reportBackgroundError(error: unknown): void {
        const message = error instanceof Error ? error.message : String(error)
        if (message === this.lastBackgroundErrorMessage) return
        this.lastBackgroundErrorMessage = message
        this.dependencies.onBackgroundError?.(error)
    }

    private reportActivePromise(): void {
        const active = this.additionPromise ?? this.flushPromise ?? this.localFlushPromise
        if (active === this.lastReportedFlushPromise) return
        this.lastReportedFlushPromise = active
        this.dependencies.onFlushPromise?.(active)
    }

    private async publishPendingRevision(): Promise<void> {
        this.publicationInProgress = true
        this.notifyOperationStateChange()
        const revision = this.pendingPublicationRevision
        if (revision === null || !this.dependencies.officialPublisher) {
            this.publicationInProgress = false
            this.notifyOperationStateChange()
            return
        }
        let publication = this.pendingPublication
        let failure: unknown = null
        try {
            if (!publication) {
                publication = await this.dependencies.officialPublisher.pin(revision)
                this.pendingPublication = publication
            }
            await publication.publish()
        } catch (error) {
            if (publication) this.lastOfficialPublishAttemptAt = this.currentTime()
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            failure = error
        } finally {
            this.publicationInProgress = false
            this.notifyOperationStateChange()
        }
        if (this.localFlushDuringPublicationPromise) {
            await this.localFlushDuringPublicationPromise.catch(() => undefined)
        }
        if (failure !== null) {
            await this.applyDeferredPublication()
            if (this.pendingPublicationRevision !== null) {
                this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
            }
            throw failure
        }
        this.lastOfficialPublishAttemptAt = this.currentTime()
        this.cancelOfficialPublishRetry()
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        await this.disposeOrQueuePublication(publication)
        await this.applyDeferredPublication()
        if (this.pendingPublicationRevision !== null) {
            this.armOfficialPublishRetry(this.officialPublishDelayMs())
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    private officialPublishDelayMs(): number {
        if (this.lastOfficialPublishAttemptAt === null) return 0
        const elapsed = this.currentTime() - this.lastOfficialPublishAttemptAt
        return Math.max(0, OFFICIAL_PUBLISH_MIN_INTERVAL_MS - elapsed)
    }

    private armOfficialPublishRetry(delay: number): void {
        if (this.officialPublishRetryHandle !== undefined) return
        this.officialPublishRetryHandle = this.clock.setTimeout(() => {
            this.officialPublishRetryHandle = undefined
            this.startBackgroundFlush('official-publish-interval')
        }, delay)
    }

    private cancelOfficialPublishRetry(): void {
        if (this.officialPublishRetryHandle === undefined) return
        this.clock.clearTimeout(this.officialPublishRetryHandle)
        this.officialPublishRetryHandle = undefined
    }

    private currentTime(): number {
        return this.dependencies.now?.() ?? Date.now()
    }

    private async disposeOrQueuePublication(publication: PinnedPublication): Promise<void> {
        try {
            await publication.dispose()
            this.pendingPublicationCleanup.delete(publication)
        } catch (error) {
            this.pendingPublicationCleanup.add(publication)
            this.reportBackgroundError(error)
            this.armPublicationCleanupRetryIfNeeded()
        }
    }

    private async retryPublicationCleanup(): Promise<void> {
        for (const publication of [...this.pendingPublicationCleanup]) {
            try {
                await publication.dispose()
                this.pendingPublicationCleanup.delete(publication)
            } catch (error) {
                this.reportBackgroundError(error)
            }
        }
        this.armPublicationCleanupRetryIfNeeded()
    }

    private armPublicationCleanupRetryIfNeeded(): void {
        if (this.pendingPublicationCleanup.size > 0) {
            this.armOfficialPublishRetry(OFFICIAL_PUBLISH_MIN_INTERVAL_MS)
        }
    }

    private cancelDebounce(): void {
        if (this.debounceHandle === undefined) return
        this.clock.clearTimeout(this.debounceHandle)
        this.debounceHandle = undefined
    }

    private rearmDebounceAfterReplacementFailure(
        capturedGeneration: number,
        hadPendingDebounce: boolean,
    ): void {
        if (
            (hadPendingDebounce ||
                this.dirtyGeneration !== capturedGeneration ||
                this.pendingByteCount > 0) &&
            !this.flushPromise &&
            !this.additionPromise
        ) {
            this.armDebounce()
        }
    }

    private rebaseReplacementPublication(
        candidate: Database,
        before: CapturedState,
        live: CapturedState,
        preserveConflicts: boolean,
    ): ReplacementRebaseResult {
        const publishedParts = splitDatabase(canonicalDatabaseClone(candidate))
        const compensation: Omit<WorkingSetCommit, 'expectedRevision'> = {}
        let publishedRoot: RootDatabase
        try {
            publishedRoot = rebaseConcurrentLiveDelta(
                before.root,
                live.root,
                publishedParts.root,
            )
        } catch (error) {
            if (!preserveConflicts) throw error
            publishedRoot = canonicalClone(live.root)
            compensation.root = publishedRoot
        }

        let publishedPresets = publishedParts.presets
        if (
            live.presets !== null &&
            before.presets !== null &&
            live.presetsCanonical !== before.presetsCanonical
        ) {
            try {
                publishedPresets = rebaseConcurrentLiveDelta(
                    before.presets,
                    live.presets,
                    publishedParts.presets,
                )
            } catch (error) {
                if (!preserveConflicts) throw error
                publishedPresets = canonicalClone(live.presets)
                compensation.replacePresets = publishedPresets
            }
        }

        let publishedPluginStorage = publishedParts.pluginStorage
        if (
            live.pluginStorage !== null &&
            before.pluginStorage !== null &&
            live.pluginStorageCanonical !== before.pluginStorageCanonical
        ) {
            try {
                publishedPluginStorage = rebaseConcurrentPluginStorage(
                    before.pluginStorage,
                    live.pluginStorage,
                    publishedParts.pluginStorage,
                )
            } catch (error) {
                if (!preserveConflicts) throw error
                publishedPluginStorage = pluginStorageClone(live.pluginStorage)
                compensation.pluginStorage = diffPluginStorage(
                    pluginStorageJson(publishedParts.pluginStorage),
                    publishedPluginStorage,
                )
            }
        }
        if (
            before.character &&
            live.character?.chaId === before.character.chaId &&
            live.characterCanonical !== before.characterCanonical
        ) {
            const index = publishedParts.characters.findIndex(
                (characterValue) => characterValue.chaId === live.character!.chaId,
            )
            // A character absent from the replacement was removed by it; do not resurrect it.
            if (index >= 0) {
                try {
                    publishedParts.characters[index] = rebaseConcurrentLiveDelta(
                        before.character,
                        live.character,
                        publishedParts.characters[index],
                    )
                } catch (error) {
                    if (!preserveConflicts) throw error
                    publishedParts.characters[index] = canonicalClone(live.character)
                    compensation.replaceCharacter = publishedParts.characters[index]
                }
            }
        }

        const includesPluginStorage = Object.prototype.hasOwnProperty.call(
            candidate,
            'pluginCustomStorage',
        ) || live.pluginStorage !== null
        return {
            database: {
                ...publishedRoot,
                ...(includesPluginStorage
                    ? { pluginCustomStorage: publishedPluginStorage }
                    : {}),
                characters: publishedParts.characters,
                botPresets: publishedPresets,
            } as Database,
            compensation: Object.keys(compensation).length > 0 ? compensation : null,
        }
    }

    private replacementExpectationError(
        options: PersistentReplacementOptions,
    ): Error | null {
        if (
            options.expectedRevision !== undefined &&
            options.expectedRevision !== this.revision
        ) {
            return new RevisionConflictError(options.expectedRevision, this.revision)
        }
        if (
            options.expectedMutationGeneration !== undefined &&
            options.expectedMutationGeneration !== this.dirtyGeneration
        ) {
            return new Error(
                `Expected mutation generation ${options.expectedMutationGeneration}, ` +
                `but current generation is ${this.dirtyGeneration}`,
            )
        }
        return null
    }

    private assertReadRevision(expected: DataRevision, actual: DataRevision): void {
        if (actual !== expected) throw new RevisionConflictError(expected, actual)
    }

    private assertInitialized(): void {
        if (this.currentRevision === null) throw new Error('Save coordinator is not initialized')
    }

    private assertPersistentMutationAllowed(): void {
        if (this.destructiveReplacementFence) {
            throw new PersistentMutationFencedError()
        }
    }

    private captureMatchesBaseline(): boolean {
        const captured = this.capture()
        return captured.rootCanonical === this.rootBaseline
            && (
                captured.pluginStorageCanonical === null
                || captured.pluginStorageCanonical === this.pluginStorageBaseline
            )
            && (
                captured.presetsCanonical === null
                || captured.presetsCanonical === this.presetsBaseline
            )
            && captured.characterCanonical === this.characterBaseline
    }
}

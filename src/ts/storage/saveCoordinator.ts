import type { Database, character, groupChat } from './database.svelte'
import type {
    ConversationMutation,
    DataRevision,
    PersistentDataStore,
    WorkingSetCommit,
} from './persistentDataStore'

const SAVE_DEBOUNCE_MS = 500
const PENDING_BYTE_LIMIT = 1_048_576

type CompleteCharacter = character | groupChat
type RootDatabase = Omit<Database, 'characters'>

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
    captureSelectedCharacter(): CompleteCharacter | null
    captureCharacter(id: string): CompleteCharacter | null
    /** Installs the working copy synchronously and must not throw. */
    replaceDatabase(database: Database): void
    officialPublisher?: OfficialRevisionPublisher
    clock?: SaveCoordinatorClock
    onLocalRevision?(revision: DataRevision): void
    onFlushPromise?(promise: Promise<void> | null): void
    onBackgroundError?(error: unknown): void
}

interface CapturedState {
    root: RootDatabase
    rootCanonical: string
    character: CompleteCharacter | null
    characterCanonical: string | null
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

function canonicalize(value: unknown): unknown {
    if (Array.isArray(value)) return value.map(canonicalize)
    if (value && typeof value === 'object') {
        const result: Record<string, unknown> = {}
        for (const key of Object.keys(value).sort()) {
            const entry = (value as Record<string, unknown>)[key]
            if (entry !== undefined) result[key] = canonicalize(entry)
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

function splitDatabase(database: Database): { root: RootDatabase; characters: CompleteCharacter[] } {
    const { characters, ...root } = database
    return { root, characters }
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
    private characterBaseline: string | null = null
    private characterBaselineId: string | null = null
    private dirtyGeneration = 0
    private pendingByteCount = 0
    private debounceHandle: unknown
    private operationTail: Promise<void> = Promise.resolve()
    private flushPromise: Promise<void> | null = null
    private additionPromise: Promise<void> | null = null
    private lastReportedFlushPromise: Promise<void> | null = null
    private pendingPublication: PinnedPublication | null = null
    private pendingPublicationRevision: DataRevision | null = null
    private pendingCharacterAddition: PendingCharacterAddition | null = null
    private reservedCharacterAddition: ReservedCharacterAddition | null = null
    private lastBackgroundErrorMessage: string | null = null

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

    initialize(revision: DataRevision, database?: Database): void {
        this.cancelDebounce()
        const captured = database ? this.captureDatabase(database) : this.capture()
        this.currentRevision = revision
        this.rootBaseline = captured.rootCanonical
        this.setCharacterBaseline(captured)
        this.dirtyGeneration = 0
        this.pendingByteCount = 0
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        this.pendingCharacterAddition = null
        this.reservedCharacterAddition = null
        this.lastBackgroundErrorMessage = null
    }

    adoptHydratedCharacter(revision: DataRevision, character: CompleteCharacter): boolean {
        if (this.currentRevision !== revision) return false
        this.characterBaseline = canonicalJson(character)
        this.characterBaselineId = character.chaId
        return true
    }

    markPersistentDataDirty(estimatedBytes: number): void {
        this.assertInitialized()
        this.dirtyGeneration++
        const bytes = Number.isFinite(estimatedBytes) && estimatedBytes > 0 ? estimatedBytes : 0
        const previousBytes = this.pendingByteCount
        this.pendingByteCount = Math.max(previousBytes, bytes)
        this.cancelDebounce()
        if (this.pendingByteCount >= PENDING_BYTE_LIMIT && previousBytes < PENDING_BYTE_LIMIT) {
            this.startBackgroundFlush('byte-limit')
            return
        }
        if (!this.flushPromise) this.armDebounce()
    }

    flushPendingData(reason: string): Promise<void> {
        this.assertInitialized()
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

    replacePersistentDatabase(database: Database, reason: string): Promise<void> {
        this.assertInitialized()
        const candidate = canonicalClone(database)
        const before = this.capture()
        const capturedGeneration = this.dirtyGeneration
        const supersededAdditionToken = (
            this.pendingCharacterAddition ?? this.reservedCharacterAddition
        )?.token ?? null
        this.cancelDebounce()
        return this.enqueue(() =>
            this.runReplacement(
                candidate,
                before,
                capturedGeneration,
                supersededAdditionToken,
                reason,
            ),
        )
    }

    publishCurrentOfficialRevision(): Promise<void> {
        this.assertInitialized()
        if (!this.dependencies.officialPublisher) return Promise.resolve()
        return this.enqueue(async () => {
            this.pendingPublicationRevision ??= this.revision
            await this.publishPendingRevision()
        })
    }

    commitCharacterAddition(request: CharacterAdditionRequest, reason: string): Promise<void> {
        this.assertInitialized()
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
        const result = this.operationTail.then(operation, operation)
        this.operationTail = result.then(
            () => undefined,
            () => undefined,
        )
        return result
    }

    private async flushIterations(_reason: string, publishOfficial: boolean): Promise<void> {
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
            if (detached) {
                commit.replaceCharacter = detached.character
            } else if (
                captured.character &&
                captured.characterCanonical !== this.characterBaseline
            ) {
                const conversations = this.diffSelectedConversations(captured)
                if (conversations) commit.conversations = conversations
                else commit.replaceCharacter = captured.character
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

            if (commit.root || commit.replaceCharacter || commit.addCharacter || commit.conversations) {
                const committed = await this.dependencies.store.commit(commit)
                this.currentRevision = committed.revision
                if (commit.root) this.rootBaseline = captured.rootCanonical
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
                if (publishOfficial && this.dependencies.officialPublisher) {
                    await this.stagePublication(committed.revision)
                }
            }

            if (!captured.character && !commit.replaceCharacter) this.setCharacterBaseline(captured)

            const current = this.capture()
            const currentAddition = this.capturePendingAddition()
            if (
                generation === this.dirtyGeneration &&
                current.rootCanonical === this.rootBaseline &&
                current.characterCanonical === this.characterBaseline &&
                (!currentAddition ||
                    (currentAddition.pending.locallyAdded &&
                        currentAddition.canonical === currentAddition.pending.baseline))
            ) {
                if (publishOfficial && this.pendingPublicationRevision !== null) {
                    await this.publishPendingRevision()
                    continue
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
            await this.disposePublication(stale)
        }
        this.pendingPublicationRevision = revision
    }

    private async runReplacement(
        candidate: Database,
        before: CapturedState,
        capturedGeneration: number,
        supersededAdditionToken: object | null,
        _reason: string,
    ): Promise<void> {
        const replaced = await this.dependencies.store.replaceFromDatabase(candidate, this.revision)
        const live = this.capture()
        const stalePublication = this.pendingPublication
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        if (this.pendingCharacterAddition?.token === supersededAdditionToken) {
            this.pendingCharacterAddition = null
        }
        if (this.reservedCharacterAddition?.token === supersededAdditionToken) {
            this.reservedCharacterAddition = null
        }
        this.currentRevision = replaced.revision
        const candidateCapture = this.captureDatabase(candidate)
        this.rootBaseline = candidateCapture.rootCanonical
        this.setCharacterBaseline(candidateCapture)

        const published = canonicalClone(candidate)
        const publishedParts = splitDatabase(published)
        if (live.rootCanonical !== before.rootCanonical) {
            Object.assign(published, canonicalClone(live.root))
        }
        if (live.character && live.characterCanonical !== before.characterCanonical) {
            const index = publishedParts.characters.findIndex(
                (characterValue) => characterValue.chaId === live.character!.chaId,
            )
            if (index >= 0) published.characters[index] = canonicalClone(live.character)
            else published.characters.push(canonicalClone(live.character))
        }

        this.dependencies.replaceDatabase(published)
        this.dependencies.onLocalRevision?.(replaced.revision)
        if (stalePublication) {
            await this.disposePublication(stalePublication)
        }

        if (this.dirtyGeneration === capturedGeneration) {
            this.cancelDebounce()
            this.pendingByteCount = 0
        } else if (!this.flushPromise && !this.additionPromise) {
            this.armDebounce()
        }
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
            if (!Array.isArray(capturedChat.message) || !Array.isArray(baselineChat.message)) {
                return null
            }
            const { message, ...conversation } = capturedChat
            mutations.push({
                type: 'replace-range',
                characterId: character.chaId,
                conversationId,
                start: 0,
                deleteCount: baselineChat.message.length,
                messages: message,
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
        const rootCanonical = canonicalJson(this.dependencies.captureRoot())
        const characterValue = this.dependencies.captureSelectedCharacter()
        const characterCanonical = characterValue ? canonicalJson(characterValue) : null
        return {
            root: JSON.parse(rootCanonical) as RootDatabase,
            rootCanonical,
            character: characterCanonical
                ? (JSON.parse(characterCanonical) as CompleteCharacter)
                : null,
            characterCanonical,
        }
    }

    private captureDatabase(database: Database): CapturedState {
        const cloned = canonicalClone(database)
        const { root, characters } = splitDatabase(cloned)
        const selectedId = this.dependencies.captureSelectedCharacter()?.chaId
        const character = selectedId ? characters.find((candidate) => candidate.chaId === selectedId) ?? null : null
        return {
            root,
            rootCanonical: JSON.stringify(root),
            character,
            characterCanonical: character ? JSON.stringify(character) : null,
        }
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
        const active = this.additionPromise ?? this.flushPromise
        if (active === this.lastReportedFlushPromise) return
        this.lastReportedFlushPromise = active
        this.dependencies.onFlushPromise?.(active)
    }

    private async publishPendingRevision(): Promise<void> {
        const revision = this.pendingPublicationRevision
        if (revision === null || !this.dependencies.officialPublisher) return
        if (!this.pendingPublication) {
            this.pendingPublication = await this.dependencies.officialPublisher.pin(revision)
        }
        const publication = this.pendingPublication
        await publication.publish()
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        await this.disposePublication(publication)
    }

    private async disposePublication(publication: PinnedPublication): Promise<void> {
        try {
            await publication.dispose()
        } catch (error) {
            this.dependencies.onBackgroundError?.(error)
        }
    }

    private cancelDebounce(): void {
        if (this.debounceHandle === undefined) return
        this.clock.clearTimeout(this.debounceHandle)
        this.debounceHandle = undefined
    }

    private assertInitialized(): void {
        if (this.currentRevision === null) throw new Error('Save coordinator is not initialized')
    }
}

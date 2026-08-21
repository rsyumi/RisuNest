import type { Database, character, groupChat } from './database.svelte'
import type { DataRevision, PersistentDataStore, WorkingSetCommit } from './persistentDataStore'

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
    /** Installs the working copy synchronously and must not throw. */
    replaceDatabase(database: Database): void
    officialPublisher?: OfficialRevisionPublisher
    clock?: SaveCoordinatorClock
    onLocalRevision?(revision: DataRevision): void
    onBackgroundError?(error: unknown): void
}

interface CapturedState {
    root: RootDatabase
    rootCanonical: string
    character: CompleteCharacter | null
    characterCanonical: string | null
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
    private dirtyGeneration = 0
    private pendingByteCount = 0
    private debounceHandle: unknown
    private operationTail: Promise<void> = Promise.resolve()
    private flushPromise: Promise<void> | null = null
    private pendingPublication: PinnedPublication | null = null
    private pendingPublicationRevision: DataRevision | null = null

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
        this.characterBaseline = captured.characterCanonical
        this.dirtyGeneration = 0
        this.pendingByteCount = 0
        this.pendingPublication = null
        this.pendingPublicationRevision = null
    }

    adoptHydratedCharacter(revision: DataRevision, character: CompleteCharacter): boolean {
        if (this.currentRevision !== revision) return false
        this.characterBaseline = canonicalJson(character)
        return true
    }

    markPersistentDataDirty(estimatedBytes: number): void {
        this.assertInitialized()
        this.dirtyGeneration++
        const bytes = Number.isFinite(estimatedBytes) && estimatedBytes > 0 ? estimatedBytes : 0
        this.pendingByteCount += bytes
        this.cancelDebounce()
        if (this.pendingByteCount >= PENDING_BYTE_LIMIT) {
            this.startBackgroundFlush('byte-limit')
            return
        }
        if (!this.flushPromise) {
            this.debounceHandle = this.clock.setTimeout(() => {
                this.debounceHandle = undefined
                this.startBackgroundFlush('debounce')
            }, SAVE_DEBOUNCE_MS)
        }
    }

    flushPendingData(reason: string): Promise<void> {
        this.assertInitialized()
        this.cancelDebounce()
        if (this.flushPromise) return this.flushPromise
        const promise = this.enqueue(() => this.flushIterations(reason, true))
        this.flushPromise = promise
        void promise.then(
            () => {
                if (this.flushPromise === promise) this.flushPromise = null
            },
            () => {
                if (this.flushPromise === promise) this.flushPromise = null
            },
        )
        return promise
    }

    replacePersistentDatabase(database: Database, reason: string): Promise<void> {
        this.assertInitialized()
        const candidate = canonicalClone(database)
        const before = this.capture()
        const capturedGeneration = this.dirtyGeneration
        this.cancelDebounce()
        return this.enqueue(() =>
            this.runReplacement(candidate, before, capturedGeneration, reason),
        )
    }

    private enqueue(operation: () => Promise<void>): Promise<void> {
        const result = this.operationTail.then(operation, operation)
        this.operationTail = result.then(
            () => undefined,
            () => undefined,
        )
        return result
    }

    private async flushIterations(_reason: string, publishOfficial: boolean): Promise<void> {
        if (this.pendingPublicationRevision !== null) {
            await this.publishPendingRevision()
        }

        while (true) {
            const generation = this.dirtyGeneration
            const captured = this.capture()
            const commit: WorkingSetCommit = { expectedRevision: this.revision }
            if (captured.rootCanonical !== this.rootBaseline) commit.root = captured.root
            if (captured.characterCanonical !== this.characterBaseline && captured.character) {
                commit.replaceCharacter = captured.character
            }

            if (commit.root || commit.replaceCharacter) {
                const committed = await this.dependencies.store.commit(commit)
                this.currentRevision = committed.revision
                if (commit.root) this.rootBaseline = captured.rootCanonical
                if (commit.replaceCharacter) this.characterBaseline = captured.characterCanonical
                this.dependencies.onLocalRevision?.(committed.revision)
                if (publishOfficial && this.dependencies.officialPublisher) {
                    this.pendingPublicationRevision = committed.revision
                    await this.publishPendingRevision()
                }
            }

            const current = this.capture()
            if (
                generation === this.dirtyGeneration &&
                current.rootCanonical === this.rootBaseline &&
                current.characterCanonical === this.characterBaseline
            ) {
                this.pendingByteCount = 0
                return
            }
        }
    }

    private async runReplacement(
        candidate: Database,
        before: CapturedState,
        capturedGeneration: number,
        _reason: string,
    ): Promise<void> {
        const replaced = await this.dependencies.store.replaceFromDatabase(candidate, this.revision)
        const live = this.capture()
        const stalePublication = this.pendingPublication
        this.pendingPublication = null
        this.pendingPublicationRevision = null
        this.currentRevision = replaced.revision
        const candidateCapture = this.captureDatabase(candidate)
        this.rootBaseline = candidateCapture.rootCanonical
        this.characterBaseline = candidateCapture.characterCanonical

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
        if (stalePublication) {
            await this.disposePublication(stalePublication)
        }

        if (this.dirtyGeneration === capturedGeneration) {
            this.cancelDebounce()
            this.pendingByteCount = 0
        }
    }

    private capture(): CapturedState {
        const root = canonicalClone(this.dependencies.captureRoot())
        const characterValue = this.dependencies.captureSelectedCharacter()
        const character = characterValue ? canonicalClone(characterValue) : null
        return {
            root,
            rootCanonical: canonicalJson(root),
            character,
            characterCanonical: character ? canonicalJson(character) : null,
        }
    }

    private captureDatabase(database: Database): CapturedState {
        const cloned = canonicalClone(database)
        const { root, characters } = splitDatabase(cloned)
        const selectedId = this.dependencies.captureSelectedCharacter()?.chaId
        const character = selectedId ? characters.find((candidate) => candidate.chaId === selectedId) ?? null : null
        return {
            root,
            rootCanonical: canonicalJson(root),
            character,
            characterCanonical: character ? canonicalJson(character) : null,
        }
    }

    private startBackgroundFlush(reason: string): void {
        void this.flushPendingData(reason).catch((error) => this.dependencies.onBackgroundError?.(error))
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

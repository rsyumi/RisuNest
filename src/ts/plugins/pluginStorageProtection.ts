import type { PluginStorageMutation } from '../storage/persistentDataStore'

export class StalePluginStorageError extends Error {
    readonly code = 'PLUGIN_STORAGE_STALE'
    constructor() {
        super('PLUGIN_STORAGE_STALE: Plugin storage changed during synchronization. Read it again before saving.')
        this.name = 'StalePluginStorageError'
    }
}

export interface PluginStorageWriteCheck {
    validate(): void
    committed(writtenKeys?: readonly { owner: string; key: string }[]): void
    saveFailed?(error: unknown): void
}

const sequences = new Map<string, number>()
const instances = new Set<PluginStorageObservation>()
const remoteListeners = new Set<(keys: readonly string[]) => boolean>()
export function observePluginRemoteChanges(listener: (keys: readonly string[]) => boolean): () => void {
    remoteListeners.add(listener)
    return () => { remoteListeners.delete(listener) }
}
let requestReload: () => void = () => {}
const identity = (owner: string, key: string) => JSON.stringify([owner, key])

export function requestPluginStorageReload(): void { requestReload() }

export function setPluginStorageReloadHandler(handler: () => void): void {
    requestReload = handler
}

/** Called in the receive queue item, before projecting the committed values. */
export function advancePluginStorageRemoteChanges(unitKeys: readonly string[]): void {
    let reload = false
    for (const unitKey of new Set(unitKeys)) {
        const [kind, owner, key] = JSON.parse(unitKey) as string[]
        if (kind !== 'plugin') continue
        const id = identity(owner, key)
        sequences.set(id, (sequences.get(id) ?? 0) + 1)
        for (const instance of instances) {
            if (instance.owner === owner && instance.hasObserved(key)) reload = true
        }
    }
    for (const listener of remoteListeners) if (listener(unitKeys)) reload = true
    if (reload) requestReload()
}

/** A failed deferred drain can have committed keys from earlier staged receives. */
export function fenceUnknownPluginStorageDrain(): void {
    const keys = new Set<string>()
    for (const instance of instances) {
        for (const key of instance.observedKeys()) keys.add(JSON.stringify(['plugin', instance.owner, key]))
    }
    advancePluginStorageRemoteChanges([...keys])
}

export class PluginStorageObservation {
    private readonly observed = new Map<string, number>()
    constructor(readonly owner: string) { instances.add(this) }
    dispose(): void { instances.delete(this) }
    observedKeys(): Iterable<string> { return this.observed.keys() }
    hasObserved(key: string): boolean { return this.observed.has(key) }
    private sequence(key: string): number { return sequences.get(identity(this.owner, key)) ?? 0 }

    observe(key: string): void { this.observed.set(key, this.sequence(key)) }

    // Capture before acquiring a read lease; a receive during a read is conservative.
    beginSnapshot(): (keys: Iterable<string>) => void {
        const captured = new Map(sequences)
        const knownKeys = new Set(this.observed.keys())
        for (const id of captured.keys()) {
            const [owner, key] = JSON.parse(id) as string[]
            if (owner === this.owner) knownKeys.add(key)
        }
        return keys => {
            let stale = false
            for (const key of new Set([...knownKeys, ...keys])) {
                const sequence = captured.get(identity(this.owner, key)) ?? 0
                this.observed.set(key, sequence)
                stale ||= sequence !== this.sequence(key)
            }
            knownKeys.clear()
            if (stale && instances.has(this)) requestReload()
        }
    }

    captureWrite(): (mutations: readonly PluginStorageMutation[]) => PluginStorageWriteCheck {
        const observed = new Map(this.observed)
        return mutations => this.prepareWrite(mutations, observed)
    }

    prepareWrite(mutations: readonly PluginStorageMutation[], observed = this.observed): PluginStorageWriteCheck {
        const keys = new Set(mutations.flatMap(mutation => mutation.type === 'clear'
            ? [...observed.keys()] : [mutation.key]))
        const expected = new Map([...keys].flatMap(key => observed.has(key)
            ? [[key, observed.get(key)!] as const] : []))
        return {
            validate: () => {
                for (const [key, sequence] of expected) {
                    if (this.sequence(key) !== sequence) throw new StalePluginStorageError()
                }
            },
            committed: (writtenKeys = []) => {
                for (const item of writtenKeys) if (item.owner === this.owner) keys.add(item.key)
                for (const key of keys) this.observe(key)
            },
        }
    }
}

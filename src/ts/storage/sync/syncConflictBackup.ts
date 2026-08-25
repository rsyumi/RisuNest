import localforage from 'localforage'
import { isTauri } from '../../platform'
import { createNativeAppKv } from '../nativeAppKv'
import { getBlobStore } from '../platformBlobStore'

export type SyncConflictBackupSide = 'local' | 'remote'

export interface SyncConflictBackupEntry {
    id: string
    createdAt: number
    side: SyncConflictBackupSide
    characterCount: number
    byteLength: number
    scope: 'database-only'
}

export interface SyncConflictBackupKv {
    getItem(key: string): Promise<unknown>
    setItem(key: string, value: unknown): Promise<unknown>
    removeItem(key: string): Promise<void>
}

export interface SyncConflictBackupPayloadStore {
    write(id: string, bytes: Uint8Array): Promise<void>
    read(id: string): Promise<unknown>
    remove(id: string): Promise<void>
}

const indexKey = 'index'
const maxBackups = 5

function payloadKey(id: string): string {
    return `payload:${id}`
}

function normalizeEntry(value: unknown): SyncConflictBackupEntry | null {
    if (!value || typeof value !== 'object') return null
    const entry = value as Partial<SyncConflictBackupEntry>
    if (!(typeof entry.id === 'string'
        && typeof entry.createdAt === 'number'
        && (entry.side === 'local' || entry.side === 'remote')
        && typeof entry.characterCount === 'number'
        && typeof entry.byteLength === 'number'
        && (entry.scope === undefined || entry.scope === 'database-only'))) {
        return null
    }
    return { ...entry, scope: 'database-only' } as SyncConflictBackupEntry
}

function createKvPayloadStore(kv: SyncConflictBackupKv): SyncConflictBackupPayloadStore {
    return {
        write: async (id, bytes) => {
            await kv.setItem(payloadKey(id), bytes)
        },
        read: (id) => kv.getItem(payloadKey(id)),
        remove: (id) => kv.removeItem(payloadKey(id)),
    }
}

export class SyncConflictBackupStore {
    private readonly payloads: SyncConflictBackupPayloadStore
    private operationTail: Promise<void> = Promise.resolve()

    constructor(
        private readonly kv: SyncConflictBackupKv,
        private readonly now: () => number = Date.now,
        payloads?: SyncConflictBackupPayloadStore,
    ) {
        this.payloads = payloads ?? createKvPayloadStore(kv)
    }

    private enqueueMutation<T>(mutation: () => Promise<T>): Promise<T> {
        const result = this.operationTail.then(mutation)
        this.operationTail = result.then(
            () => undefined,
            () => undefined,
        )
        return result
    }

    async list(): Promise<SyncConflictBackupEntry[]> {
        const raw = await this.kv.getItem(indexKey)
        if (!Array.isArray(raw)) return []
        const entries = raw.map(normalizeEntry)
        if (entries.some((entry) => entry === null)) return []
        return (entries as SyncConflictBackupEntry[])
            .sort((left, right) => right.createdAt - left.createdAt)
    }

    async save(input: {
        side: SyncConflictBackupSide
        bytes: Uint8Array
        characterCount: number
    }): Promise<SyncConflictBackupEntry> {
        return this.enqueueMutation(async () => {
            const entry: SyncConflictBackupEntry = {
                id: globalThis.crypto.randomUUID(),
                createdAt: this.now(),
                side: input.side,
                characterCount: input.characterCount,
                byteLength: input.bytes.byteLength,
                scope: 'database-only',
            }
            await this.payloads.write(entry.id, input.bytes)
            const entries = [entry, ...await this.list()]
            try {
                await this.kv.setItem(indexKey, entries.slice(0, maxBackups))
            } catch (error) {
                await this.payloads.remove(entry.id).catch(() => undefined)
                throw error
            }
            await Promise.allSettled(
                entries.slice(maxBackups).map((dropped) => this.payloads.remove(dropped.id)),
            )
            return entry
        })
    }

    async read(id: string): Promise<Uint8Array | null> {
        const value = await this.payloads.read(id)
        if (value instanceof Uint8Array) return value
        if (value instanceof ArrayBuffer) return new Uint8Array(value)
        return null
    }

    async remove(id: string): Promise<void> {
        return this.enqueueMutation(async () => {
            const remaining = (await this.list()).filter((entry) => entry.id !== id)
            await this.kv.setItem(indexKey, remaining)
            await this.payloads.remove(id).catch(() => undefined)
        })
    }
}

let sharedStore: SyncConflictBackupStore | null = null

const nativeIndexKey = 'sync-conflict-backups.index.v1'

function nativePayloadKey(id: string): string {
    return `sync-conflict-backups/${id}.risudat`
}

export function getSyncConflictBackupStore(): SyncConflictBackupStore {
    if (sharedStore) return sharedStore
    if (isTauri) {
        const appKv = createNativeAppKv()
        const blobStore = getBlobStore()
        sharedStore = new SyncConflictBackupStore({
            getItem: () => appKv.get(nativeIndexKey),
            async setItem(_key, value) {
                await appKv.set(nativeIndexKey, value)
                return value
            },
            removeItem: () => appKv.remove(nativeIndexKey),
        }, Date.now, {
            async write(id, bytes) {
                await blobStore.put(nativePayloadKey(id), bytes, {
                    kind: 'asset',
                    mime: 'application/octet-stream',
                    name: `${id}.risudat`,
                    ext: 'risudat',
                })
            },
            read: (id) => blobStore.read(nativePayloadKey(id)),
            remove: (id) => blobStore.remove(nativePayloadKey(id)),
        })
    } else {
        sharedStore = new SyncConflictBackupStore(
            localforage.createInstance({ name: 'risuaiSyncConflictBackup' }),
        )
    }
    return sharedStore
}

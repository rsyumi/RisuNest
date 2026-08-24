import localforage from 'localforage'

export type SyncConflictBackupSide = 'local' | 'remote'

export interface SyncConflictBackupEntry {
    id: string
    createdAt: number
    side: SyncConflictBackupSide
    characterCount: number
    byteLength: number
}

export interface SyncConflictBackupKv {
    getItem(key: string): Promise<unknown>
    setItem(key: string, value: unknown): Promise<unknown>
    removeItem(key: string): Promise<void>
}

const indexKey = 'index'
const maxBackups = 5

function payloadKey(id: string): string {
    return `payload:${id}`
}

function isEntry(value: unknown): value is SyncConflictBackupEntry {
    if (!value || typeof value !== 'object') return false
    const entry = value as Partial<SyncConflictBackupEntry>
    return typeof entry.id === 'string'
        && typeof entry.createdAt === 'number'
        && (entry.side === 'local' || entry.side === 'remote')
        && typeof entry.characterCount === 'number'
        && typeof entry.byteLength === 'number'
}

export class SyncConflictBackupStore {
    constructor(
        private readonly kv: SyncConflictBackupKv,
        private readonly now: () => number = Date.now,
    ) {}

    async list(): Promise<SyncConflictBackupEntry[]> {
        const raw = await this.kv.getItem(indexKey)
        if (!Array.isArray(raw) || !raw.every(isEntry)) return []
        return [...raw].sort((left, right) => right.createdAt - left.createdAt)
    }

    async save(input: {
        side: SyncConflictBackupSide
        bytes: Uint8Array
        characterCount: number
    }): Promise<SyncConflictBackupEntry> {
        const entry: SyncConflictBackupEntry = {
            id: globalThis.crypto.randomUUID(),
            createdAt: this.now(),
            side: input.side,
            characterCount: input.characterCount,
            byteLength: input.bytes.byteLength,
        }
        await this.kv.setItem(payloadKey(entry.id), input.bytes)
        const entries = [entry, ...await this.list()]
        await this.kv.setItem(indexKey, entries.slice(0, maxBackups))
        for (const dropped of entries.slice(maxBackups)) {
            await this.kv.removeItem(payloadKey(dropped.id))
        }
        return entry
    }

    async read(id: string): Promise<Uint8Array | null> {
        const value = await this.kv.getItem(payloadKey(id))
        if (value instanceof Uint8Array) return value
        if (value instanceof ArrayBuffer) return new Uint8Array(value)
        return null
    }

    async remove(id: string): Promise<void> {
        await this.kv.removeItem(payloadKey(id))
        const remaining = (await this.list()).filter((entry) => entry.id !== id)
        await this.kv.setItem(indexKey, remaining)
    }
}

let sharedStore: SyncConflictBackupStore | null = null

export function getSyncConflictBackupStore(): SyncConflictBackupStore {
    sharedStore ??= new SyncConflictBackupStore(
        localforage.createInstance({ name: 'risuaiSyncConflictBackup' }),
    )
    return sharedStore
}

export interface StorageMutationGate {
    runWrite<T>(operation: () => Promise<T>): Promise<T>
    runMigration<T>(operation: () => Promise<T>): Promise<T>
}

export interface StorageLockManager {
    request<T>(
        name: string,
        options: { mode: 'shared' | 'exclusive' },
        operation: () => Promise<T>,
    ): Promise<T>
}

type QueuedOperation<T = unknown> = {
    mode: 'shared' | 'exclusive'
    operation: () => Promise<T>
    resolve(value: T): void
    reject(error: unknown): void
}

export function createInRealmStorageLockManager(): StorageLockManager {
    const queue: QueuedOperation[] = []
    let sharedCount = 0
    let exclusive = false

    const finish = (job: QueuedOperation, succeeded: boolean, value: unknown) => {
        if (job.mode === 'shared') sharedCount--
        else exclusive = false
        if (succeeded) job.resolve(value)
        else job.reject(value)
        pump()
    }

    const start = (job: QueuedOperation) => {
        if (job.mode === 'shared') sharedCount++
        else exclusive = true
        void job.operation().then(
            (value) => finish(job, true, value),
            (error) => finish(job, false, error),
        )
    }

    const pump = () => {
        if (exclusive || queue.length === 0) return
        if (queue[0].mode === 'exclusive') {
            if (sharedCount === 0) start(queue.shift()!)
            return
        }
        while (queue[0]?.mode === 'shared' && !exclusive) start(queue.shift()!)
    }

    return {
        request<T>(_name: string, options: { mode: 'shared' | 'exclusive' }, operation: () => Promise<T>) {
            return new Promise<T>((resolve, reject) => {
                queue.push({ mode: options.mode, operation, resolve, reject } as QueuedOperation)
                pump()
            })
        },
    }
}

export class StorageMigrationUnsupportedError extends Error {
    constructor() {
        super('Persistent migration requires Web Locks in a multi-document browser')
        this.name = 'StorageMigrationUnsupportedError'
    }
}

const fallbackLocks = createInRealmStorageLockManager()

function browserLocks(): StorageLockManager | null {
    if (typeof navigator === 'undefined' || !navigator.locks) return null
    return navigator.locks as unknown as StorageLockManager
}

export function createStorageMutationGate(options: {
    locks?: StorageLockManager
    allowInRealmMigration?: boolean
} = {}): StorageMutationGate {
    const nativeLocks = options.locks ?? browserLocks()
    const locks = nativeLocks ?? fallbackLocks
    const migrationSupported = nativeLocks !== null
        || options.allowInRealmMigration === true
        || typeof document === 'undefined'
    const run = <T>(mode: 'shared' | 'exclusive', operation: () => Promise<T>) =>
        locks.request('risuai-persistent-migration', { mode }, operation)

    return {
        runWrite: (operation) => run('shared', operation),
        runMigration(operation) {
            if (!migrationSupported) return Promise.reject(new StorageMigrationUnsupportedError())
            return run('exclusive', operation)
        },
    }
}

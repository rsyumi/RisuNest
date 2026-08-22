export interface StorageMutationGate {
    runWrite<T>(operation: () => Promise<T>): Promise<T>
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
        void Promise.resolve().then(job.operation).then(
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

const fallbackLocks = createInRealmStorageLockManager()

function browserLocks(): StorageLockManager | null {
    if (typeof navigator === 'undefined' || !navigator.locks) return null
    return navigator.locks as unknown as StorageLockManager
}

export function createStorageMutationGate(options: {
    locks?: StorageLockManager
} = {}): StorageMutationGate {
    const locks = options.locks ?? browserLocks() ?? fallbackLocks

    return {
        runWrite: (operation) =>
            locks.request('risuai-persistent-storage', { mode: 'shared' }, operation),
    }
}

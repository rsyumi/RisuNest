export interface StorageMutationGate {
    runWrite<T>(operation: () => Promise<T>): Promise<T>
    runKeyedWrite<T>(key: string, operation: () => Promise<T>): Promise<T>
    runTransition<T>(operation: () => Promise<T>): Promise<T>
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
    type LockState = { queue: QueuedOperation[]; sharedCount: number; exclusive: boolean }
    const states = new Map<string, LockState>()

    const finish = (name: string, state: LockState, job: QueuedOperation, succeeded: boolean, value: unknown) => {
        if (job.mode === 'shared') state.sharedCount--
        else state.exclusive = false
        if (succeeded) job.resolve(value)
        else job.reject(value)
        pump(name, state)
        if (!state.exclusive && state.sharedCount === 0 && state.queue.length === 0) states.delete(name)
    }

    const start = (name: string, state: LockState, job: QueuedOperation) => {
        if (job.mode === 'shared') state.sharedCount++
        else state.exclusive = true
        void Promise.resolve().then(job.operation).then(
            (value) => finish(name, state, job, true, value),
            (error) => finish(name, state, job, false, error),
        )
    }

    const pump = (name: string, state: LockState) => {
        if (state.exclusive || state.queue.length === 0) return
        if (state.queue[0].mode === 'exclusive') {
            if (state.sharedCount === 0) start(name, state, state.queue.shift()!)
            return
        }
        while (state.queue[0]?.mode === 'shared' && !state.exclusive) start(name, state, state.queue.shift()!)
    }

    return {
        request<T>(name: string, options: { mode: 'shared' | 'exclusive' }, operation: () => Promise<T>) {
            return new Promise<T>((resolve, reject) => {
                const state = states.get(name) ?? { queue: [], sharedCount: 0, exclusive: false }
                states.set(name, state)
                state.queue.push({ mode: options.mode, operation, resolve, reject } as QueuedOperation)
                pump(name, state)
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
            locks.request('risunest-persistent-storage', { mode: 'shared' }, operation),
        runKeyedWrite: (key, operation) =>
            locks.request('risunest-persistent-storage', { mode: 'shared' }, () =>
                locks.request(`risunest-blob:${key}`, { mode: 'exclusive' }, operation)),
        runTransition: (operation) =>
            locks.request('risunest-persistent-storage', { mode: 'exclusive' }, operation),
    }
}

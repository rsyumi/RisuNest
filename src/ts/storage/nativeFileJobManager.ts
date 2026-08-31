import { Mutex } from '../mutex'
import { writable } from 'svelte/store'

import type { NativeFileJobStatus } from './nativeFileJobs'

export interface NativeFileOperationState {
    kind: 'import' | 'export'
    status?: NativeFileJobStatus
    blocking: boolean
}

export interface SharedNativeFileOperationContext {
    signal: AbortSignal
    onStatus(status: NativeFileJobStatus): void
    setBlocking(blocking: boolean): void
}

export const nativeFileOperation = writable<NativeFileOperationState | null>(null)

let activeOperation: Promise<unknown> | null = null
let activeOperationKey: string | null = null
let activeController: AbortController | null = null
const externalAndroidOperationMutex = new Mutex()

export class NativeFileOperationBusyError extends Error {
    constructor() {
        super('Another native file operation is already running')
        this.name = 'NativeFileOperationBusyError'
    }
}

export function runSharedNativeFileOperation<T>(
    kind: NativeFileOperationState['kind'],
    operationKey: string,
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
): Promise<T> {
    if (activeOperation) {
        return activeOperationKey === operationKey
            ? activeOperation as Promise<T>
            : Promise.reject(new NativeFileOperationBusyError())
    }

    const controller = new AbortController()
    activeController = controller
    activeOperationKey = operationKey
    nativeFileOperation.set({ kind, blocking: false })
    const update = (patch: Partial<NativeFileOperationState>) => {
        nativeFileOperation.update((current) => current ? { ...current, ...patch } : current)
    }

    const promise = operation({
        signal: controller.signal,
        onStatus: (status) => update({ status }),
        setBlocking: (blocking) => update({ blocking }),
    }).finally(() => {
        if (activeOperation !== promise) return
        activeOperation = null
        activeOperationKey = null
        activeController = null
        nativeFileOperation.set(null)
    })
    activeOperation = promise
    return promise
}

export function runExternalAndroidNativeFileOperation<T>(
    kind: NativeFileOperationState['kind'],
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
): Promise<T> {
    return externalAndroidOperationMutex.runExclusive(async () => {
        while (activeOperation) {
            try {
                await activeOperation
            }
            catch {}
        }
        return await runSharedNativeFileOperation(
            kind,
            `external-android:${kind}`,
            operation,
        )
    })
}

export function cancelActiveNativeFileOperation(): void {
    activeController?.abort()
}

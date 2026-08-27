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
let activeController: AbortController | null = null
let externalAndroidOperationTail: Promise<void> = Promise.resolve()

export function runSharedNativeFileOperation<T>(
    kind: NativeFileOperationState['kind'],
    operation: (context: SharedNativeFileOperationContext) => Promise<T>,
): Promise<T> {
    if (activeOperation) return activeOperation as Promise<T>

    const controller = new AbortController()
    activeController = controller
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
    const queued = externalAndroidOperationTail.then(async () => {
        while (activeOperation) {
            try {
                await activeOperation
            }
            catch {}
        }
        return await runSharedNativeFileOperation(kind, operation)
    })
    externalAndroidOperationTail = queued.then(
        () => undefined,
        () => undefined,
    )
    return queued
}

export function cancelActiveNativeFileOperation(): void {
    activeController?.abort()
}

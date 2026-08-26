import {
    consumeAndroidSpoolBatch,
    type AndroidSpoolBatch,
    type AndroidSpoolFailure,
    type AndroidSpoolReady,
} from './androidSafBridge'
import type { NativeFileJobSource } from './nativeFileJobs'

export interface AndroidRisuSaveRestoreInput {
    source: NativeFileJobSource
    displayName: string
}

export interface AndroidRisuSaveSpoolRouteDependencies {
    confirmRestore(source: AndroidSpoolReady): Promise<boolean>
    restore(input: AndroidRisuSaveRestoreInput): Promise<void>
    unsupported(source: AndroidSpoolReady): void
    failed(failure: AndroidSpoolFailure): void
    onError(source: AndroidSpoolReady, error: unknown): void
}

export interface AndroidRisuSaveSpoolRoute {
    enqueue(batch: AndroidSpoolBatch): Promise<void>
}

export function createAndroidRisuSaveSpoolRoute(
    dependencies: AndroidRisuSaveSpoolRouteDependencies,
): AndroidRisuSaveSpoolRoute {
    const handledTokens = new Set<string>()
    let queue = Promise.resolve()

    return {
        enqueue(batch) {
            queue = queue.then(async () => {
                const ready = batch.ready.filter((source) => {
                    if (handledTokens.has(source.token)) return false
                    handledTokens.add(source.token)
                    return true
                })
                await consumeAndroidSpoolBatch(
                    { ...batch, ready },
                    {
                        failed: dependencies.failed,
                        unsupported: dependencies.unsupported,
                        restore: async (input) => {
                            const token = input.source.type === 'androidSpool'
                                ? input.source.token
                                : null
                            if (!token) return
                            const source = ready.find((item) => item.token === token)
                            if (!source || !await dependencies.confirmRestore(source)) return
                            try {
                                await dependencies.restore(input)
                            }
                            catch (error) {
                                dependencies.onError(source, error)
                            }
                        },
                    },
                )
            })
            return queue
        },
    }
}

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
    discard(source: AndroidSpoolReady): void
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
                        unsupported: (source) => {
                            dependencies.discard(source)
                            dependencies.unsupported(source)
                        },
                        restore: async (input) => {
                            const token = input.source.type === 'androidSpool'
                                ? input.source.token
                                : null
                            if (!token) return
                            const source = ready.find((item) => item.token === token)
                            if (!source) return
                            if (!await dependencies.confirmRestore(source)) {
                                dependencies.discard(source)
                                return
                            }
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

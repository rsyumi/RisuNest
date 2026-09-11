import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    syntheticNativeFileJobStatus,
    type NativeFileJobOptions,
    type NativeFileJobStatus,
} from './nativeFileJobs'

/** Keep preparation and the revisioned store commit inside one visible operation. */
export function runContentImport<T>(
    name: string,
    options: NativeFileJobOptions,
    operation: (options: NativeFileJobOptions) => Promise<T>,
): Promise<T> {
    if (options.onStatus) return operation(options)
    return runSharedNativeFileOperation(
        'import',
        `content:${name}`,
        async (context) => {
            context.setSource({ name })
            const signal = options.signal
                ? AbortSignal.any([options.signal, context.signal])
                : context.signal
            return operation({ ...options, signal, onStatus: context.onStatus })
        },
        { presentation: 'dialog', format: 'content' },
    )
}

export function contentMappingStatus(
    status: NativeFileJobStatus,
): NativeFileJobStatus {
    return syntheticNativeFileJobStatus(status, 'finalizing-staging')
}

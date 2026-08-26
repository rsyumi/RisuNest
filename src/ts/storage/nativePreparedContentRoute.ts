import {
    prepareNativeContentImport,
    type NativeFileJobOptions,
    type NativeFileJobSource,
    type PreparedNativeContent,
    type PreparedNativeContentReceipt,
} from './nativeFileJobs'

export interface NativePreparedContentRouteDependencies<TMapped, TResult> {
    prepare(
        source: NativeFileJobSource,
        displayName: string,
        options?: NativeFileJobOptions,
    ): Promise<PreparedNativeContentReceipt>
    map(content: PreparedNativeContent): Promise<TMapped>
    activate(mapped: TMapped): Promise<TResult>
    onCleanupWarning?(error: unknown): void
}

const defaultPrepare: NativePreparedContentRouteDependencies<unknown, unknown>['prepare'] = (
    source,
    displayName,
    options,
) => prepareNativeContentImport(source, displayName, options)

export async function runNativePreparedContentRoute<TMapped, TResult>(
    source: NativeFileJobSource,
    displayName: string,
    dependencies: Omit<NativePreparedContentRouteDependencies<TMapped, TResult>, 'prepare'> & {
        prepare?: NativePreparedContentRouteDependencies<TMapped, TResult>['prepare']
    },
    options: NativeFileJobOptions = {},
): Promise<TResult> {
    const receipt = await (dependencies.prepare ?? defaultPrepare)(source, displayName, options)
    let result: TResult
    try {
        if (options.signal?.aborted) throw new DOMException('Native file job was cancelled', 'AbortError')
        const mapped = await dependencies.map(receipt.content)
        if (options.signal?.aborted) throw new DOMException('Native file job was cancelled', 'AbortError')
        result = await dependencies.activate(mapped)
    }
    catch (error) {
        try {
            await receipt.cancel()
        }
        catch {}
        throw error
    }
    try {
        await receipt.confirmActivated()
    }
    catch (error) {
        try {
            dependencies.onCleanupWarning?.(error)
        }
        catch {}
    }
    return result
}

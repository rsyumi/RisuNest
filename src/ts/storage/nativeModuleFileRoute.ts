import type { NativeFileJobOptions, NativeFileJobSource } from './nativeFileJobs'

export type NativeModuleFileRouteResult<T> =
    | { kind: 'declined' }
    | { kind: 'imported'; value: T }

export type NativeModulePathImporter<T> = (
    input: {
        source: NativeFileJobSource
        displayName: string
    },
    options?: NativeFileJobOptions,
) => Promise<{ kind: 'declined' } | { kind: 'imported'; value: T }>

function fileNameFromPath(path: string): string {
    return path.split(/[\\/]/).at(-1) || path
}

export async function importDesktopNativeModulePath<T>(
    path: string,
    nativeImport: NativeModulePathImporter<T>,
    options: NativeFileJobOptions = {},
): Promise<NativeModuleFileRouteResult<T>> {
    const displayName = fileNameFromPath(path)
    return nativeImport({
        source: { type: 'desktopPath', path },
        displayName,
    }, options)
}

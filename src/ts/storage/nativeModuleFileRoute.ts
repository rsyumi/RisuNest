import type { NativeFileJobSource } from './nativeFileJobs'

export type NativeModuleFileRouteResult<T> =
    | { kind: 'declined' }
    | { kind: 'imported'; mode: 'native' | 'legacy'; value: T }

export interface NativeModuleFileRouteDependencies<T> {
    readDesktopPath(path: string): Promise<Uint8Array>
    nativeImport(input: {
        source: NativeFileJobSource
        displayName: string
    }): Promise<{ kind: 'declined' } | { kind: 'imported'; value: T }>
    legacyImport(input: { name: string; data: Uint8Array }): Promise<T | null>
}

function fileNameFromPath(path: string): string {
    return path.split(/[\\/]/).at(-1) || path
}

export async function importDesktopNativeModulePath<T>(
    path: string,
    dependencies: NativeModuleFileRouteDependencies<T>,
): Promise<NativeModuleFileRouteResult<T>> {
    const displayName = fileNameFromPath(path)
    const extension = displayName.split('.').at(-1)?.toLocaleLowerCase('en-US')
    if (extension === 'risum') {
        const result = await dependencies.nativeImport({
            source: { type: 'desktopPath', path },
            displayName,
        })
        return result.kind === 'declined' ? result : {
            kind: 'imported',
            mode: 'native',
            value: result.value,
        }
    }
    const value = await dependencies.legacyImport({
        name: displayName,
        data: await dependencies.readDesktopPath(path),
    })
    return value === null ? { kind: 'declined' } : {
        kind: 'imported',
        mode: 'legacy',
        value,
    }
}

import {
    NativeFileJobError,
    type NativeFileJobSource,
} from './nativeFileJobs'

export type NativeCharacterFileRouteResult<T> =
    | { kind: 'cancelled' }
    | { kind: 'declined' }
    | { kind: 'destination-required' }
    | { kind: 'imported'; mode: 'native' | 'legacy'; value: T }

export type NativeCharacterImportResult<T> =
    | { kind: 'declined' }
    | { kind: 'imported'; value: T }

export type NativeAndroidCharacterSpoolResult<T> =
    | NativeCharacterImportResult<T>
    | { kind: 'destination-required' }
    | { kind: 'capability-unavailable' }

export interface NativeCharacterFileRouteDependencies<T> {
    chooseDesktopPath(): Promise<string | null>
    readDesktopPath(path: string): Promise<Uint8Array>
    nativeEnabled(): boolean
    nativeImport(input: {
        source: NativeFileJobSource
        displayName: string
    }): Promise<NativeCharacterImportResult<T>>
    legacyImport(input: { name: string; data: Uint8Array }): Promise<T | null>
}

export type NativeCharacterFileRoutePathDependencies<T> = Omit<
    NativeCharacterFileRouteDependencies<T>,
    'chooseDesktopPath'
>

function fileNameFromPath(path: string): string {
    return path.split(/[\\/]/).at(-1) || path
}

function isNativeCharacterCandidate(displayName: string): boolean {
    const extension = displayName.split('.').at(-1)?.toLocaleLowerCase('en-US')
    return extension === 'json'
        || extension === 'png'
        || extension === 'charx'
        || extension === 'jpg'
        || extension === 'jpeg'
}

function nativeErrorCode(error: unknown): string | null {
    if (error instanceof NativeFileJobError) return error.code
    if (
        typeof error === 'object'
        && error !== null
        && 'code' in error
        && typeof error.code === 'string'
    ) {
        return error.code
    }
    return null
}

function mayUseLegacyFallback(error: unknown): boolean {
    const code = nativeErrorCode(error)
    if (error instanceof NativeFileJobError) {
        return code === 'capability-unavailable'
            || code === 'unsupported-format'
            || code === 'native-limit'
    }
    return code === 'unsupported-character-card'
        && error instanceof Error
        && error.name === 'UnsupportedPreparedNativeCharacterCardError'
}

function isDestinationRequired(error: unknown): boolean {
    return error instanceof NativeFileJobError
        && (
            nativeErrorCode(error) === 'destination-required'
            || nativeErrorCode(error) === 'unsupported-without-destination'
        )
}

function isCapabilityUnavailable(error: unknown): boolean {
    return error instanceof NativeFileJobError
        && nativeErrorCode(error) === 'capability-unavailable'
}

async function importLegacy<T>(
    path: string,
    displayName: string,
    dependencies: NativeCharacterFileRoutePathDependencies<T>,
): Promise<NativeCharacterFileRouteResult<T>> {
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

export async function importDesktopNativeCharacterFromPicker<T>(
    dependencies: NativeCharacterFileRouteDependencies<T>,
): Promise<NativeCharacterFileRouteResult<T>> {
    const path = await dependencies.chooseDesktopPath()
    if (!path) return { kind: 'cancelled' }

    return importDesktopNativeCharacterPath(path, dependencies)
}

export async function importDesktopNativeCharacterPath<T>(
    path: string,
    dependencies: NativeCharacterFileRoutePathDependencies<T>,
): Promise<NativeCharacterFileRouteResult<T>> {
    const displayName = fileNameFromPath(path)
    if (!isNativeCharacterCandidate(displayName) || !dependencies.nativeEnabled()) {
        return importLegacy(path, displayName, dependencies)
    }

    try {
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
    catch (error) {
        if (isDestinationRequired(error)) return { kind: 'destination-required' }
        if (mayUseLegacyFallback(error)) {
            return importLegacy(path, displayName, dependencies)
        }
        throw error
    }
}

export async function importAndroidNativeCharacterSpool<T>(
    input: { token: string; displayName: string },
    dependencies: NativeCharacterFileRouteDependencies<T>,
): Promise<NativeAndroidCharacterSpoolResult<T>> {
    if (!dependencies.nativeEnabled()) return { kind: 'capability-unavailable' }
    try {
        return await dependencies.nativeImport({
            source: { type: 'androidSpool', token: input.token },
            displayName: input.displayName,
        })
    }
    catch (error) {
        if (isDestinationRequired(error)) return { kind: 'destination-required' }
        if (isCapabilityUnavailable(error)) return { kind: 'capability-unavailable' }
        throw error
    }
}

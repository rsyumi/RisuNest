import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'
import type { PersistentRevisionLease } from './persistentDataStore'

export const nativePersistentRevisionLease: unique symbol = Symbol(
    'nativePersistentRevisionLease',
)

export interface NativePersistentRevisionLease extends PersistentRevisionLease {
    readonly [nativePersistentRevisionLease]: string
}
export interface NativePersistentExportOptions {
    omitAccount?: boolean
}

export interface NativePersistentExportDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
}

export interface NativePersistentExportFile {
    excludedArchivedCharacterCount: number
    excludedCollidingPluginValueCount: number
    path: string
    bytes: number
}

const productionDependencies: NativePersistentExportDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => args === undefined
        ? invoke(command)
        : invoke(command, args),
}

export function hasNativePersistentRevisionLease(
    lease: PersistentRevisionLease,
): lease is NativePersistentRevisionLease {
    return typeof (lease as Partial<NativePersistentRevisionLease>)[
        nativePersistentRevisionLease
    ] === 'string'
}

async function withNativePersistentRisuSaveFile<T>(
    lease: string,
    options: NativePersistentExportOptions,
    callback: (file: NativePersistentExportFile) => Promise<T>,
    dependencies: NativePersistentExportDependencies,
): Promise<T> {
    let exported: NativePersistentExportFile | undefined
    let result: T | undefined
    let primaryError: unknown
    let failed = false

    try {
        exported = await dependencies.invoke(
            'pds_export_risu_save',
            {
                lease,
                omitAccount: options.omitAccount ?? false,
            },
        ) as NativePersistentExportFile
        result = await callback(exported)
    } catch (error) {
        failed = true
        primaryError = error
    }

    let cleanupError: unknown
    if (exported) {
        try {
            await dependencies.invoke('pds_export_risu_save_cleanup', {
                path: exported.path,
            })
        } catch (error) {
            cleanupError = error
        }
    }

    if (failed) throw primaryError
    if (cleanupError !== undefined) throw cleanupError
    return result as T
}

export function withPinnedNativePersistentRisuSaveFile<T>(
    lease: PersistentRevisionLease,
    options: NativePersistentExportOptions,
    callback: (file: NativePersistentExportFile) => Promise<T>,
    dependencies: NativePersistentExportDependencies = productionDependencies,
): Promise<T> {
    if (!dependencies.isTauri()) {
        throw new Error('Native persistent export requires Tauri')
    }
    if (!hasNativePersistentRevisionLease(lease)) {
        throw new Error('Persistent revision lease does not support native export')
    }
    return withNativePersistentRisuSaveFile(
        lease[nativePersistentRevisionLease],
        options,
        callback,
        dependencies,
    )
}

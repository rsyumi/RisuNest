import { invoke } from '@tauri-apps/api/core'
import { open as openFile } from '@tauri-apps/plugin-fs'

import { isTauri } from '../platform'
import type { DataRevision, PersistentRevisionLease } from './persistentDataStore'
import { retryPersistentRevisionRelease } from './persistentRecordIterator'

export const nativePersistentRevisionLease: unique symbol = Symbol(
    'nativePersistentRevisionLease',
)

export interface NativePersistentRevisionLease extends PersistentRevisionLease {
    readonly [nativePersistentRevisionLease]: string
}

export interface NativePersistentExportRuntime {
    readonly revision: DataRevision
    flushPendingData(reason: string): Promise<void>
}

export interface NativePersistentExportOptions {
    omitAccount?: boolean
}

export interface NativePersistentExportResult {
    revision: DataRevision
    bytes: number
}

export interface NativePersistentExportDependencies {
    isTauri(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    open(
        path: string,
        options: NativePersistentOpenOptions,
    ): Promise<NativePersistentFileHandle>
}

export interface NativePersistentOpenOptions {
    read?: boolean
    write?: boolean
    create?: boolean
    truncate?: boolean
}

export interface NativePersistentFileHandle {
    read(buffer: Uint8Array): Promise<number | null>
    write(data: Uint8Array): Promise<number>
    close(): Promise<void>
}

export interface NativePersistentExportFile {
    path: string
    bytes: number
}

const productionDependencies: NativePersistentExportDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => args === undefined
        ? invoke(command)
        : invoke(command, args),
    open: (path, options) => openFile(path, options),
}

const COPY_BUFFER_BYTES = 1024 * 1024

async function copyNativePersistentExport(
    sourcePath: string,
    destinationPath: string,
    dependencies: NativePersistentExportDependencies,
): Promise<void> {
    let source: NativePersistentFileHandle | undefined
    let destination: NativePersistentFileHandle | undefined
    let primaryError: unknown
    let failed = false

    try {
        source = await dependencies.open(sourcePath, { read: true })
        destination = await dependencies.open(destinationPath, {
            write: true,
            create: true,
            truncate: true,
        })
        const buffer = new Uint8Array(COPY_BUFFER_BYTES)
        while (true) {
            const bytesRead = await source.read(buffer)
            if (bytesRead === null) break
            if (bytesRead <= 0 || bytesRead > buffer.byteLength) {
                throw new Error('Native export source returned an invalid read length')
            }
            let offset = 0
            while (offset < bytesRead) {
                const bytesWritten = await destination.write(
                    buffer.subarray(offset, bytesRead),
                )
                if (bytesWritten <= 0 || bytesWritten > bytesRead - offset) {
                    throw new Error('Native export destination returned an invalid write length')
                }
                offset += bytesWritten
            }
        }
    } catch (error) {
        failed = true
        primaryError = error
    }

    let closeError: unknown
    if (destination) {
        try {
            await destination.close()
        } catch (error) {
            closeError = error
        }
    }
    if (source) {
        try {
            await source.close()
        } catch (error) {
            if (closeError === undefined) closeError = error
        }
    }

    if (failed) throw primaryError
    if (closeError !== undefined) throw closeError
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

export async function exportNativePersistentRisuSave(
    runtime: NativePersistentExportRuntime,
    destination: string,
    options: NativePersistentExportOptions = {},
    dependencies: NativePersistentExportDependencies = productionDependencies,
): Promise<NativePersistentExportResult> {
    if (!dependencies.isTauri()) {
        throw new Error('Native persistent export requires Tauri')
    }

    await runtime.flushPendingData('native-risu-save-export')
    const revision = runtime.revision
    const { lease } = await dependencies.invoke('pds_acquire_revision', {
        revision,
    }) as { lease: string }
    let result: NativePersistentExportResult | undefined
    let primaryError: unknown
    let failed = false

    try {
        result = await withNativePersistentRisuSaveFile(
            lease,
            options,
            async (exported) => {
                await copyNativePersistentExport(
                    exported.path,
                    destination,
                    dependencies,
                )
                return { revision, bytes: exported.bytes }
            },
            dependencies,
        )
    } catch (error) {
        failed = true
        primaryError = error
    }

    let releaseError: unknown
    try {
        await retryPersistentRevisionRelease(async () => {
            await dependencies.invoke('pds_release_revision', { lease })
        })
    } catch (error) {
        releaseError = error
    }

    if (failed) throw primaryError
    if (releaseError !== undefined) throw releaseError
    if (result === undefined) throw new Error('Native persistent export produced no result')
    return result
}

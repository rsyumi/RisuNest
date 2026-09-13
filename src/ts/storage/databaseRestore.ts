import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'

type PluginRestoreDependencies = {
    replaceDatabase: (database: Database, reason: string) => Promise<void>
    loadPlugins: () => void | Promise<void>
}

async function installPluginRestore(
    database: Database,
    reason: string,
    dependencies: PluginRestoreDependencies,
): Promise<void> {
    await dependencies.replaceDatabase(database, reason)
    await dependencies.loadPlugins()
}

export const installAccountBackup = (database: Database, dependencies: PluginRestoreDependencies) =>
    installPluginRestore(database, 'account-backup', dependencies)

export const installRisuKeiBackup = (database: Database, dependencies: PluginRestoreDependencies) =>
    installPluginRestore(database, 'risu-kei-backup', dependencies)

interface AccountUnmigrationResourceDependencies {
    coldKeys: Iterable<string>
    collectAssetKeys(selectedCold: ReadonlyMap<string, unknown>): Iterable<string>
    isValidCold(value: unknown): boolean
    readLocalAsset(key: string): Promise<Uint8Array | null>
    readRemoteAsset(key: string): Promise<Uint8Array | null>
    writeLocalAsset(key: string, bytes: Uint8Array): Promise<void>
    readLocalCold(key: string): Promise<unknown | null>
    readRemoteCold(key: string): Promise<unknown | null>
    writeLocalCold(key: string, value: unknown): Promise<void>
    onProgress?(stage: 'cold' | 'assets', completed: number, total: number): void
}

function equalBytes(left: Uint8Array | null, right: Uint8Array): boolean {
    if (!left || left.byteLength !== right.byteLength) return false
    return left.every((value, index) => value === right[index])
}

export async function materializeAccountUnmigrationResources(
    dependencies: AccountUnmigrationResourceDependencies,
): Promise<void> {
    const selectedCold = new Map<string, unknown>()
    const coldKeys = [...new Set(dependencies.coldKeys)]
    let completed = 0
    dependencies.onProgress?.('cold', completed, coldKeys.length)
    for (const key of coldKeys) {
        const local = await dependencies.readLocalCold(key)
        if (local !== null) {
            if (!dependencies.isValidCold(local)) {
                throw new Error(`Invalid local cold payload: ${key}`)
            }
            selectedCold.set(key, local)
            dependencies.onProgress?.('cold', ++completed, coldKeys.length)
            continue
        }
        const remote = await dependencies.readRemoteCold(key)
        if (remote === null) throw new Error(`Missing account cold payload: ${key}`)
        if (!dependencies.isValidCold(remote)) {
            throw new Error(`Invalid account cold payload: ${key}`)
        }
        await dependencies.writeLocalCold(key, remote)
        const verified = await dependencies.readLocalCold(key)
        if (verified === null || JSON.stringify(verified) !== JSON.stringify(remote)) {
            throw new Error(`Failed to verify local cold payload: ${key}`)
        }
        selectedCold.set(key, verified)
        dependencies.onProgress?.('cold', ++completed, coldKeys.length)
    }

    const assetKeys = [...new Set(dependencies.collectAssetKeys(selectedCold))]
    completed = 0
    dependencies.onProgress?.('assets', completed, assetKeys.length)
    for (const key of assetKeys) {
        if (await dependencies.readLocalAsset(key)) {
            dependencies.onProgress?.('assets', ++completed, assetKeys.length)
            continue
        }
        const remote = await dependencies.readRemoteAsset(key)
        if (!remote) throw new Error(`Missing account asset: ${key}`)
        await dependencies.writeLocalAsset(key, remote)
        if (!equalBytes(await dependencies.readLocalAsset(key), remote)) {
            throw new Error(`Failed to verify local asset: ${key}`)
        }
        dependencies.onProgress?.('assets', ++completed, assetKeys.length)
    }
}

export async function installLocalBackup(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        publishAcceptedRevision: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, 'local-backup')
    await dependencies.publishAcceptedRevision()
    await dependencies.relaunch()
}

export async function installDriveRestore(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        publishAcceptedRevision: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, 'drive-restore')
    await dependencies.publishAcceptedRevision()
    await dependencies.relaunch()
}

export async function completeAccountUnmigration(
    database: Database,
    dependencies: {
        prepareResources: () => Promise<void>
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        finalize: () => void
    },
): Promise<void> {
    const candidate = safeStructuredClone(database)
    candidate.account = null

    await dependencies.prepareResources()
    await dependencies.replaceDatabase(candidate, 'account-unmigration')
    dependencies.finalize()
}

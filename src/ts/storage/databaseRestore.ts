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

export const installInternalBackup = (database: Database, dependencies: PluginRestoreDependencies) =>
    installPluginRestore(database, 'internal-backup', dependencies)

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
}

function equalBytes(left: Uint8Array | null, right: Uint8Array): boolean {
    if (!left || left.byteLength !== right.byteLength) return false
    return left.every((value, index) => value === right[index])
}

export async function materializeAccountUnmigrationResources(
    dependencies: AccountUnmigrationResourceDependencies,
): Promise<void> {
    const selectedCold = new Map<string, unknown>()
    for (const key of dependencies.coldKeys) {
        const local = await dependencies.readLocalCold(key)
        if (local !== null) {
            if (!dependencies.isValidCold(local)) {
                throw new Error(`Invalid local cold payload: ${key}`)
            }
            selectedCold.set(key, local)
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
    }

    for (const key of dependencies.collectAssetKeys(selectedCold)) {
        if (await dependencies.readLocalAsset(key)) continue
        const remote = await dependencies.readRemoteAsset(key)
        if (!remote) throw new Error(`Missing account asset: ${key}`)
        await dependencies.writeLocalAsset(key, remote)
        if (!equalBytes(await dependencies.readLocalAsset(key), remote)) {
            throw new Error(`Failed to verify local asset: ${key}`)
        }
    }
}

export async function installLocalBackup(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        publishAcceptedRevision: () => Promise<void>
        writeLocalMirror: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, 'local-backup')
    await dependencies.publishAcceptedRevision()
    await dependencies.writeLocalMirror()
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
        captureAcceptedDatabase: () => Database
        writeLegacyMirror: (database: Database) => Promise<void>
        finalize: () => void
    },
): Promise<void> {
    const candidate = safeStructuredClone(database)
    candidate.account = null

    await dependencies.prepareResources()
    await dependencies.replaceDatabase(candidate, 'account-unmigration')
    await dependencies.writeLegacyMirror(safeStructuredClone(dependencies.captureAcceptedDatabase()))
    dependencies.finalize()
}

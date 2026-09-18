import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'
import type { CommittedApplyOutcome, PersistentDataRuntime } from './persistentDataRuntime'

type RestoreFollowupDependencies = {
    onPostCommitError?(error: unknown): void | Promise<void>
}

type PluginRestoreDependencies = RestoreFollowupDependencies & {
    replaceDatabase: PersistentDataRuntime['replacePersistentDatabase']
    loadPlugins: () => void | Promise<void>
}

async function finishCommittedRestore(
    outcome: CommittedApplyOutcome,
    followup: () => void | Promise<void>,
    dependencies: RestoreFollowupDependencies,
): Promise<CommittedApplyOutcome> {
    try {
        await followup()
    } catch (error) {
        try {
            if (dependencies.onPostCommitError) await dependencies.onPostCommitError(error)
            else console.error('Post-commit restore action failed', error)
        } catch (reportError) {
            console.error('Post-commit restore error reporting failed', reportError)
        }
    }
    return outcome
}

async function installPluginRestore(
    database: Database,
    reason: string,
    dependencies: PluginRestoreDependencies,
): Promise<CommittedApplyOutcome> {
    const outcome = await dependencies.replaceDatabase(database, reason)
    if (outcome.projection === 'refresh-required') return outcome
    return finishCommittedRestore(outcome, dependencies.loadPlugins, dependencies)
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
    readRemoteCold(key: string): Promise<unknown | null>
    onProgress?(stage: 'cold' | 'assets', completed: number, total: number): void
}

function equalBytes(left: Uint8Array | null, right: Uint8Array): boolean {
    if (!left || left.byteLength !== right.byteLength) return false
    return left.every((value, index) => value === right[index])
}

export async function materializeAccountUnmigrationResources(
    dependencies: AccountUnmigrationResourceDependencies,
): Promise<ReadonlyMap<string, unknown>> {
    const selectedCold = new Map<string, unknown>()
    const coldKeys = [...new Set(dependencies.coldKeys)]
    let completed = 0
    dependencies.onProgress?.('cold', completed, coldKeys.length)
    for (const key of coldKeys) {
        const remote = await dependencies.readRemoteCold(key)
        if (remote === null) throw new Error(`Missing account cold payload: ${key}`)
        if (!dependencies.isValidCold(remote)) {
            throw new Error(`Invalid account cold payload: ${key}`)
        }
        selectedCold.set(key, remote)
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

    return selectedCold
}

export async function installLocalBackup(
    database: Database,
    dependencies: RestoreFollowupDependencies & {
        replaceDatabase: PersistentDataRuntime['replacePersistentDatabase']
        publishAcceptedRevision: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<CommittedApplyOutcome> {
    const outcome = await dependencies.replaceDatabase(database, 'local-backup', { publishOfficial: true })
    if (outcome.projection === 'refresh-required') return outcome
    return finishCommittedRestore(outcome, async () => {
        await dependencies.publishAcceptedRevision()
        await dependencies.relaunch()
    }, dependencies)
}

export async function installDriveRestore(
    database: Database,
    dependencies: RestoreFollowupDependencies & {
        replaceDatabase: PersistentDataRuntime['replacePersistentDatabase']
        publishAcceptedRevision: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<CommittedApplyOutcome> {
    const outcome = await dependencies.replaceDatabase(database, 'drive-restore', { publishOfficial: true })
    if (outcome.projection === 'refresh-required') return outcome
    return finishCommittedRestore(outcome, async () => {
        await dependencies.publishAcceptedRevision()
        await dependencies.relaunch()
    }, dependencies)
}

export async function completeAccountUnmigration(
    database: Database,
    dependencies: RestoreFollowupDependencies & {
        prepareResources: (candidate: Database) => Promise<void>
        replaceDatabase: PersistentDataRuntime['replacePersistentDatabase']
        finalize: () => void | Promise<void>
    },
): Promise<CommittedApplyOutcome> {
    const candidate = safeStructuredClone(database)
    candidate.account = null

    await dependencies.prepareResources(candidate)
    const outcome = await dependencies.replaceDatabase(candidate, 'account-unmigration')
    // Device account markers must follow the committed authority even if projection failed.
    return finishCommittedRestore(outcome, dependencies.finalize, dependencies)
}

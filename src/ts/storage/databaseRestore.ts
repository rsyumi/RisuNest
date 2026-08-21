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

export async function installLocalBackup(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        writeLocalMirror: () => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, 'local-backup')
    await dependencies.writeLocalMirror()
    await dependencies.relaunch()
}

export async function installDriveRestore(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        relaunch: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, 'drive-restore')
    await dependencies.relaunch()
}

export async function completeAccountUnmigration(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        captureAcceptedDatabase: () => Database
        writeLegacyMirror: (database: Database) => Promise<void>
        finalize: () => void
    },
): Promise<void> {
    const candidate = safeStructuredClone(database)
    candidate.account = null

    await dependencies.replaceDatabase(candidate, 'account-unmigration')
    await dependencies.writeLegacyMirror(safeStructuredClone(dependencies.captureAcceptedDatabase()))
    dependencies.finalize()
}

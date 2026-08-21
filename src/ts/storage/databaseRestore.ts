import { safeStructuredClone } from '../polyfill'
import type { Database } from './database.svelte'

export async function replaceDatabaseBefore(
    database: Database,
    reason: string,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        afterReplacement: () => void | Promise<void>
    },
): Promise<void> {
    await dependencies.replaceDatabase(database, reason)
    await dependencies.afterReplacement()
}

export async function completeAccountUnmigration(
    database: Database,
    dependencies: {
        replaceDatabase: (database: Database, reason: string) => Promise<void>
        writeLegacyMirror: (database: Database) => Promise<void>
        finalize: () => void
    },
): Promise<void> {
    const candidate = safeStructuredClone(database)
    candidate.account = null

    await dependencies.replaceDatabase(candidate, 'account-unmigration')
    await dependencies.writeLegacyMirror(safeStructuredClone(candidate))
    dependencies.finalize()
}

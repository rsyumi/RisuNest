import { language } from 'src/lang'
import { alertConfirm, alertError, alertNormal, alertSelect } from '../../alert'
import type { Database } from '../database.svelte'
import { installLocalBackup } from '../databaseRestore'
import {
    flushPendingData,
    capturePersistentMutationToken,
    publishCurrentOfficialRevision,
    replacePersistentDatabase,
} from '../persistentDataRuntime.svelte'
import { decodeRisuSave } from '../risuSave'
import {
    getSyncConflictBackupStore,
    type SyncConflictBackupEntry,
} from './syncConflictBackup'

function entryLabel(entry: SyncConflictBackupEntry): string {
    const side = entry.side === 'local'
        ? language.syncBackupSideLocal
        : language.syncBackupSideRemote
    const label = language.syncBackupEntry
        .replace('{date}', new Date(entry.createdAt).toLocaleString())
        .replace('{side}', side)
        .replace('{count}', `${entry.characterCount}`)
    return `${label} / ${language.syncBackupDatabaseOnly}`
}

export async function openSyncConflictBackups(): Promise<void> {
    const store = getSyncConflictBackupStore()
    const entries = await store.list()
    if (entries.length === 0) {
        alertNormal(language.syncConflictNoBackups)
        return
    }
    const selected = await alertSelect(entries.map(entryLabel), language.syncConflictBackups)
    const entry = entries[Number(selected)]
    if (!entry) return
    if (!await alertConfirm(language.syncConflictRestoreConfirm)) return
    const bytes = await store.read(entry.id)
    if (!bytes) {
        alertError(language.syncConflictNoBackups)
        return
    }
    const decoded = await decodeRisuSave(bytes) as Database
    if (!decoded || typeof decoded !== 'object' || !Array.isArray(decoded.characters)) {
        alertError('Invalid sync conflict backup')
        return
    }
    await flushPendingData('sync-conflict-restore')
    const current = await capturePersistentMutationToken(
        'sync-conflict-restore',
    )
    await installLocalBackup(decoded, {
        replaceDatabase: (database, reason, options) => replacePersistentDatabase(database, reason, {
            ...options,
            authoritative: true,
            expectedRevision: current.revision,
            expectedMutationGeneration: current.mutationGeneration,
        }),
        publishAcceptedRevision: publishCurrentOfficialRevision,
        onPostCommitError: (error) => {
            console.error('Committed conflict restore follow-up failed', error)
            alertError(language.risuNest.persistentData.followupFailed)
        },
        relaunch: () => location.reload(),
    })
}

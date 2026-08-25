import { language } from 'src/lang'
import { alertConfirm, alertError, alertNormal, alertSelect } from '../../alert'
import type { Database } from '../database.svelte'
import { installLocalBackup } from '../databaseRestore'
import {
    getPersistentDataRuntime,
    publishCurrentOfficialRevision,
    replacePersistentDatabase,
} from '../persistentDataRuntime.svelte'
import { decodeRisuSave } from '../risuSave'
import { withFlushedRisuSaveExport } from '../risuSaveStoreAdapter'
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
    const current = await withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'sync-conflict-restore-safety-backup',
        async (pinned) => ({
            revision: pinned.revision,
            mutationGeneration: pinned.mutationGeneration,
            bytes: await pinned.collectBytes(),
            characterCount: await pinned.countCharacters(),
        }),
    )
    await store.save({
        side: 'local',
        bytes: current.bytes,
        characterCount: current.characterCount,
    })
    await installLocalBackup(decoded, {
        replaceDatabase: (database, reason) => replacePersistentDatabase(database, reason, {
            authoritative: true,
            expectedRevision: current.revision,
            expectedMutationGeneration: current.mutationGeneration,
        }),
        publishAcceptedRevision: publishCurrentOfficialRevision,
        relaunch: () => location.reload(),
    })
}

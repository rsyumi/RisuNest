import { language } from 'src/lang'
import { alertConfirm, alertError, alertNormal, alertSelect } from '../../alert'
import { getDatabase, type Database } from '../database.svelte'
import { installLocalBackup } from '../databaseRestore'
import {
    publishCurrentOfficialRevision,
    replacePersistentDatabase,
} from '../persistentDataRuntime.svelte'
import { decodeRisuSave, encodeRisuSaveLegacy } from '../risuSave'
import {
    getSyncConflictBackupStore,
    type SyncConflictBackupEntry,
} from './syncConflictBackup'

function entryLabel(entry: SyncConflictBackupEntry): string {
    const side = entry.side === 'local'
        ? language.syncBackupSideLocal
        : language.syncBackupSideRemote
    return language.syncBackupEntry
        .replace('{date}', new Date(entry.createdAt).toLocaleString())
        .replace('{side}', side)
        .replace('{count}', `${entry.characterCount}`)
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
    const current = getDatabase()
    await store.save({
        side: 'local',
        bytes: encodeRisuSaveLegacy(current, 'compression'),
        characterCount: current.characters?.length ?? 0,
    })
    await installLocalBackup(decoded, {
        replaceDatabase: replacePersistentDatabase,
        publishAcceptedRevision: publishCurrentOfficialRevision,
        relaunch: () => location.reload(),
    })
}

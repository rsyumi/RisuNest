import { getDatabase } from '../storage/database.svelte'
import {
    getPersistentDataRuntime,
    materializePersistentDatabaseSnapshot,
} from '../storage/persistentDataRuntime.svelte'
import { keiServerURL } from './kei'
import { runNativeKeiBackupJob } from './nativeBackup'

let lastKeiSave = 0

export async function saveDbKei(): Promise<void> {
    try {
        const liveAccount = getDatabase()?.account
        if (!liveAccount?.kei) {
            return
        }
        if (Date.now() - lastKeiSave < 60000 * 5) {
            return
        }
        lastKeiSave = Date.now()
        const liveAccountId = liveAccount.id
        const liveToken = liveAccount.token
        const url = keiServerURL() + '/autobackup/save'
        if (await runNativeKeiBackupJob({
            runtime: getPersistentDataRuntime(),
            url,
            accountId: liveAccountId,
            token: liveToken,
        })) {
            return
        }
        const database = await materializePersistentDatabaseSnapshot('kei-auto-backup')
        const snapshotAccount = database.account
        if (
            !snapshotAccount?.kei ||
            snapshotAccount.id !== liveAccountId ||
            snapshotAccount.token !== liveToken
        ) {
            throw new Error('Kei account changed during backup materialization')
        }
        await fetch(url, {
            method: 'POST',
            headers: {
                'Content-Type': 'application/json',
            },
            body: JSON.stringify({
                token: snapshotAccount.token,
                database,
            }),
        })
    } catch (error) {
        console.error('Kei auto backup failed:', error)
    }
}

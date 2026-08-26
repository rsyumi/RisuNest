import { invoke } from '@tauri-apps/api/core'

import { isTauri } from '../platform'
import {
    hasNativePersistentRevisionLease,
    nativePersistentRevisionLease,
} from '../storage/nativePersistentExport'
import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'
import {
    releasePersistentRevisionLease,
    withPersistentRevisionLease,
} from '../storage/persistentRecordIterator'

export interface NativeKeiBackupRequest {
    runtime: PersistentDataRuntime
    url: string
    accountId: string
    token: string
}

export interface NativeKeiBackupDependencies {
    isTauri(): boolean
    invoke(command: string, args: Record<string, unknown>): Promise<unknown>
}

const productionDependencies: NativeKeiBackupDependencies = {
    isTauri: () => isTauri,
    invoke: (command, args) => invoke(command, args),
}

export async function tryNativeKeiBackup(
    request: NativeKeiBackupRequest,
    dependencies: NativeKeiBackupDependencies = productionDependencies,
): Promise<boolean> {
    if (!dependencies.isTauri()) return false

    await request.runtime.flushPendingData('kei-auto-backup')
    const revision = request.runtime.revision
    const lease = await request.runtime.store.acquireRevision(revision)
    if (!hasNativePersistentRevisionLease(lease)) {
        await releasePersistentRevisionLease(lease)
        return false
    }

    await withPersistentRevisionLease(lease, async () => {
        await dependencies.invoke('pds_kei_backup_upload', {
            lease: lease[nativePersistentRevisionLease],
            url: request.url,
            expectedAccountId: request.accountId,
            token: request.token,
        })
    })
    return true
}

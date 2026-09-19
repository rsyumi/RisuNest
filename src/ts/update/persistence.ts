import type { PersistentDataRuntime } from '../storage/persistentDataRuntime'

type UpdatePersistence = Pick<PersistentDataRuntime,
    'flushPendingDataLocally' | 'capturePersistentMutationToken' | 'acquireDestructiveReplacementFence'>

/** The caller retains this fence until installation/restart returns or fails. */
export async function prepareUpdateInstallation(
    runtime?: UpdatePersistence,
): Promise<{ release(): void }> {
    runtime ??= (await import('../storage/persistentDataRuntime.svelte')).getPersistentDataRuntime()
    await runtime.flushPendingDataLocally('app-update-install')
    const token = await runtime.capturePersistentMutationToken('app-update-install', {
        publishOfficial: false,
    })
    return runtime.acquireDestructiveReplacementFence(token)
}

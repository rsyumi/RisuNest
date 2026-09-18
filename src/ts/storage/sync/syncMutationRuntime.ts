import type { PersistentDataRuntime } from '../persistentDataRuntime'

/** The sync adapter shares the local runtime's exact-token and committed-result contracts. */
export type SyncMutationRuntime = Pick<
    PersistentDataRuntime,
    | 'flushPendingData'
    | 'capturePersistentMutationToken'
    | 'acquireDestructiveReplacementFence'
    | 'refreshActiveWorkingSetFromStore'
>

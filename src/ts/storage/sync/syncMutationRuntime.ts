/**
 * Renderer mutation runtime backing a sync adapter. The fence is the save
 * coordinator's global exclusive destructive-replacement fence: while held it
 * blocks every persistent mutation, so holders must release it or keep a
 * retry path alive (see the module-level lane singletons).
 */
export interface SyncMutationRuntime {
    flushPendingData(reason: string): Promise<void>
    capturePersistentMutationToken(reason: string): Promise<{
        revision: number
        mutationGeneration: number
    }>
    acquireDestructiveReplacementFence(token: {
        revision: number
        mutationGeneration: number
    }): Promise<{
        readonly revision: number
        refreshCommittedWorkingSet(revision: number): Promise<void>
        release(): void
    }>
}

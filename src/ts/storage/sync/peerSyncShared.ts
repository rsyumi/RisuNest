/**
 * Shared contracts for the peer sync lanes (clone, delta, bidirectional).
 *
 * Every lane invokes native commands through the same Tauri invoke shape,
 * drives the Android foreground service through the same Kotlin bridge, and
 * coordinates renderer mutation through the save coordinator's exclusive
 * destructive-replacement fence. Declaring these once keeps a new lane from
 * copying the contracts and drifting.
 */

export interface PeerSyncInvoke {
    <T>(command: string, args?: Record<string, unknown>): Promise<T>
}

export interface PeerSyncForegroundBridge {
    startSource(lane: string, operationId: string, generation: number): boolean
    stopSource(lane: string, operationId: string, generation: number): boolean
}

/**
 * Renderer mutation runtime backing a peer sync lane. The fence is the save
 * coordinator's global exclusive destructive-replacement fence: while held it
 * blocks every persistent mutation, so holders must release it or keep a
 * retry path alive (see the module-level lane singletons).
 */
export interface PeerSyncMutationRuntime {
    flushPendingData(reason: string): Promise<void>
    capturePersistentMutationToken(reason: string): Promise<{
        revision: number
        mutationGeneration: number
    }>
    acquireDestructiveReplacementFence(token: {
        revision: number
        mutationGeneration: number
    }): Promise<{
        refreshCommittedWorkingSet(revision: number): Promise<void>
        release(): void
    }>
}

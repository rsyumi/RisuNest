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
    /**
     * Whether Android can actually show the foreground notification that
     * carries the user's Stop affordance. Absent on older bridges.
     */
    notificationsEnabled?(): boolean
}

/**
 * Returns false when the Android peer-sync Stop notification is suppressed
 * (notifications denied or the channel silenced), true when it can be shown,
 * and null when no bridge is available to tell (non-Android or old bridge).
 */
export function androidPeerSyncNotificationsEnabled(
    bridge?: PeerSyncForegroundBridge,
): boolean | null {
    const resolved = bridge
        ?? (typeof window === 'undefined' ? undefined : window.RisuPeerCloneBridge)
    if (!resolved || typeof resolved.notificationsEnabled !== 'function') return null
    try {
        return resolved.notificationsEnabled()
    } catch {
        return null
    }
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

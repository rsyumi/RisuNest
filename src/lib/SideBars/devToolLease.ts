import type {
    CompleteConversationLease,
    SelectedConversationTarget,
} from 'src/ts/storage/activeWorkingSet.svelte'

export class DevToolConversationLease {
    private destroyed = false
    private generation = 0
    private lease: CompleteConversationLease | null = null

    async acquire(
        target: SelectedConversationTarget | null,
        acquire: (
            reason: string,
            target: SelectedConversationTarget,
        ) => Promise<CompleteConversationLease>,
    ): Promise<boolean> {
        const generation = ++this.generation
        this.lease?.release()
        this.lease = null
        if (this.destroyed) return false
        if (!target) return !this.destroyed
        let lease: CompleteConversationLease
        try {
            lease = await acquire('devtool-panel', target)
        } catch {
            return false
        }
        if (this.destroyed || generation !== this.generation) {
            lease.release()
            return false
        }
        this.lease = lease
        return true
    }

    destroy(): void {
        if (this.destroyed) return
        this.destroyed = true
        this.generation++
        this.lease?.release()
        this.lease = null
    }
}

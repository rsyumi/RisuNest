import { PersistentConversationViewportSource, type PersistentConversationViewportSourceOptions } from './conversationViewportSource'
import type { Message } from './storage/database.svelte'
import { WindowedConversationOverlay } from './storage/windowedConversationOverlay'

/** Exercises the same overlay mutation and publication used by the working set. */
export class EditableViewportFixture extends PersistentConversationViewportSource {
    readonly edits: WindowedConversationOverlay
    private sessionVersion = 0

    constructor(options: PersistentConversationViewportSourceOptions) {
        const edits = new WindowedConversationOverlay(options.characterId, options.conversationId, options.revision, options.totalMessages)
        super({ ...options, overlay: edits })
        this.edits = edits
    }

    applyOptimisticRange(start: number, count: number, messages: Message[], version = ++this.sessionVersion) {
        const resident = Array.from({ length: count }, (_, i) =>
            this.projectedLocalRow(start + i) ?? this.snapshot().rowAt(start + i)?.message)
        const before = resident.map((row, i) => row ?? messages[i])
        if (before.some(row => !row)) throw new Error('Load the messages before editing them')
        const epoch = this.edits.epoch
        const rollback = this.edits.applyRange(start, before, messages, version)
        const structureChanged = this.edits.epoch !== epoch
        const pairChanged = count !== messages.length || resident.some((row, i) => !row || row.role !== messages[i].role ||
            !!row.disabled !== !!messages[i].disabled || !!row.isComment !== !!messages[i].isComment)
        this.refreshProjection({ start, end: start + Math.max(count, messages.length), structureChanged, pairChanged })
        return () => {
            const end = Math.max(this.edits.count, start + count)
            rollback()
            this.refreshProjection({ start: 0, end, structureChanged })
        }
    }

    override advanceUnchangedRevision(revision: number) {
        this.edits.advanceUntouched(this.edits.revision, revision)
        super.advanceUnchangedRevision(revision)
    }
}

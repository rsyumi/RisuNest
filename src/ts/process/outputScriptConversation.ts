import type { ConversationCommitObserver } from './conversationOperationContext'
import { PersistentMutationFencedError } from '../storage/saveCoordinator'
import { flushPendingDataLocally } from '../storage/persistentDataRuntime.svelte'
import type { GenerationOutputScript } from './generationOutputPhase'
import { captureHistoryWindowSource, openHistoryWindowCopy, type HistoryWindowCopy } from './historyWindowWrite'
import type { HistoryWindowController } from './historyWindowController'
import type { Chat } from '../storage/database.svelte'
import type { ScriptConversationAccess } from './scriptConversationAccess'
import { requestChatData, type OutputPluginRequestBoundary } from './request/request'

/** The positional draft shared by a script and its nested frames. */
export class ScriptHistoryWindow {
    private sourceCurrent: () => boolean
    private draft: HistoryWindowCopy | null = null

    constructor(readonly controller: HistoryWindowController, private readonly onCommit?: ConversationCommitObserver) {
        this.sourceCurrent = captureHistoryWindowSource(controller)
    }

    read(): Chat {
        if (!this.draft) {
            if (!this.sourceCurrent()) throw new PersistentMutationFencedError()
            this.draft = openHistoryWindowCopy(this.controller, this.onCommit)
        }
        return this.draft.chat
    }

    commit(checkSource = false): void {
        const draft = this.draft
        this.draft = null
        if (draft ? !draft.commit() : checkSource && !this.sourceCurrent()) throw new PersistentMutationFencedError()
    }

    reopen(): void {
        if (!this.controller.reconcileMetadata()) throw new PersistentMutationFencedError()
        this.sourceCurrent = captureHistoryWindowSource(this.controller)
    }
}

/** One output invocation, including the trigger, command and Lua frames sharing its edits. */
export class OutputScriptConversation {
    private readonly frames = new Set<() => void>()
    private failure: { error: unknown } | null = null
    private finished = false

    constructor(
        private readonly owner: GenerationOutputScript,
        readonly access: ScriptConversationAccess | null,
        readonly window: ScriptHistoryWindow | null,
    ) {}

    get signal(): AbortSignal {
        return this.owner.signal
    }

    get stopped(): boolean {
        return this.failure !== null || this.signal.aborted
    }

    requireActive(): void {
        if (this.failure) throw this.failure.error
        if (this.finished) throw new PersistentMutationFencedError()
        this.signal.throwIfAborted()
    }

    finish(): void {
        this.finished = true
    }

    onReopen(refresh: () => void): () => void {
        this.frames.add(refresh)
        return () => { this.frames.delete(refresh) }
    }

    requestBoundary(): OutputPluginRequestBoundary {
        this.requireActive()
        let endInterval: (() => Promise<void>) | undefined
        return {
            signal: this.signal,
            open: async () => {
                try {
                    this.requireActive()
                    this.owner.assertOwner()
                    if (this.window) this.window.commit(true)
                    else if (this.access) this.access.checkpoint()
                    else throw new PersistentMutationFencedError()
                    await flushPendingDataLocally('output-script-plugin-request')
                    this.requireActive()
                    endInterval = this.owner.openInterval()
                } catch (error) {
                    this.failure ??= { error }
                    throw error
                }
            },
            close: async failure => {
                try {
                    await endInterval?.()
                    if (failure) {
                        this.failure ??= failure
                        return
                    }
                    await this.owner.refresh()
                    this.requireActive()
                    this.window?.reopen()
                    this.access?.readChat()
                    for (const refresh of this.frames) refresh()
                } catch (error) {
                    this.failure ??= { error }
                    throw error
                }
            },
        }
    }
}

export function requestScriptChatData(
    arg: Parameters<typeof requestChatData>[0],
    model: Parameters<typeof requestChatData>[1],
    invocation?: OutputScriptConversation,
    collect?: Parameters<typeof requestChatData>[4],
) {
    return invocation
        ? requestChatData(arg, model, undefined, invocation.requestBoundary(), collect)
        : requestChatData(arg, model)
}

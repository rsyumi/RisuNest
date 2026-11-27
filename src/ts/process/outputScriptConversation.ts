import type { ConversationCommitObserver } from './conversationOperationContext'
import { PersistentMutationFencedError } from '../storage/saveCoordinator'
import { flushPendingDataLocally } from '../storage/persistentDataRuntime.svelte'
import type { GenerationOutputScript } from './generationOutputPhase'
import { captureHistoryWindowSource, openHistoryWindowCopy, type HistoryWindowCopy } from './historyWindowWrite'
import type { HistoryWindowController } from './historyWindowController'
import type { Chat } from '../storage/database.svelte'
import type { ScriptConversationAccess } from './scriptConversationAccess'
import { Mutex } from '../mutex'
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
    private readonly intervals = new Mutex()
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
        return this.signal.aborted
    }

    requireActive(): void {
        this.signal.throwIfAborted()
        if (this.finished) throw new PersistentMutationFencedError()
    }

    finish(): void {
        this.finished = true
    }

    onReopen(refresh: () => void): () => void {
        this.frames.add(refresh)
        return () => { this.frames.delete(refresh) }
    }

    /** A coroutine can request the chat while another coroutine is awaiting a plugin. */
    private withScriptView<T>(read: () => T): T | Promise<T> {
        this.requireActive()
        if (!this.intervals.isLocked) return read()
        return this.intervals.acquire(this.signal).then(release => {
            try {
                this.requireActive()
                return read()
            } finally {
                release()
            }
        })
    }

    beginLuaFrame() {
        let accepting = true
        const assertFrame = () => {
            if (!accepting) throw new PersistentMutationFencedError()
        }
        return {
            withScriptView: <T>(read: () => T) => {
                assertFrame()
                return this.withScriptView(read)
            },
            requestBoundary: () => this.requestBoundary(assertFrame),
            finish: () => {
                accepting = false
                // Finish admissions already queued by this frame. A detached ordinary request
                // can still return, but any later attempt to enter plugin code is fenced.
                if (!this.stopped && this.intervals.isLocked) return this.intervals.acquire(this.signal).then(release => release())
            },
        }
    }

    requestBoundary(assertFrame?: () => void): OutputPluginRequestBoundary {
        this.requireActive()
        let endInterval: (() => Promise<void>) | undefined
        let release: (() => void) | undefined
        return {
            signal: this.signal,
            open: async () => {
                assertFrame?.()
                release = await this.intervals.acquire(this.signal)
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
                    this.owner.fail(error)
                    release()
                    throw error
                }
            },
            close: async failure => {
                try {
                    if (failure) this.owner.fail(failure.error)
                    await endInterval?.()
                    await this.owner.refresh()
                    this.requireActive()
                    this.window?.reopen()
                    this.access?.readChat()
                    for (const refresh of this.frames) refresh()
                } catch (error) {
                    this.owner.fail(error)
                    throw error
                } finally {
                    release?.()
                }
            },
        }
    }
}

export function requestScriptChatData(
    arg: Parameters<typeof requestChatData>[0],
    model: Parameters<typeof requestChatData>[1],
    invocation?: Pick<OutputScriptConversation, 'requestBoundary'>,
    collect?: Parameters<typeof requestChatData>[4],
) {
    return invocation
        ? requestChatData(arg, model, undefined, invocation.requestBoundary(), collect)
        : requestChatData(arg, model)
}

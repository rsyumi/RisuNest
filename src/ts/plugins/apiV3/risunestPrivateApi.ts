import {
    normalizeConversationContextInput,
    type ConversationContext,
} from '../conversationContext'
import {
    createConversationPatchLedger,
    normalizeConversationPatchInput,
    type ConversationPatchResult,
} from '../conversationPatch'
import type { ConversationPatchAccess } from '../conversationPatchAccess'
import { normalizeHostToolCallInput, type HostToolAccess, type HostToolList } from '../hostToolBridge'
import type { ChatViewEvent, ChatViewListenerAccess } from '../chatViewEvents'
import type { GenerationEndEvent, GenerationEndListenerAccess } from '../generationEndEvents'
import type { RPCToolCallContent } from '../../process/mcp/mcplib'
import { linkPluginQueryAbortSignals, type PluginDatabaseAccess } from '../pluginDatabaseAccess'

// Methods for RisuNest's own plugins. They are not part of the public plugin API and
// change together with those plugins.

export interface RisunestPrivateApiDependencies {
    databaseAccess: Pick<PluginDatabaseAccess, 'readConversationContext'>
    patchAccess: ConversationPatchAccess
    hostTools: HostToolAccess
    chatView: ChatViewListenerAccess
    generationEnd: GenerationEndListenerAccess
    /** The periodic `db` permission, the one `getDatabase` asks for. */
    hasDatabasePermission(): Promise<boolean>
    /** The periodic `dbWrite` permission, the one `setChatToIndex` asks for. */
    hasDatabaseWritePermission(): Promise<boolean>
    /** The periodic `hostTools` permission that the host tool methods ask for. */
    hasHostToolPermission(): Promise<boolean>
    lifetimeSignal: AbortSignal
    now?(): number
}

function throwIfAborted(signal: AbortSignal): void {
    if (!signal.aborted) return
    throw signal.reason ?? new DOMException('The operation was aborted', 'AbortError')
}

function inputSignal(input: unknown): AbortSignal | undefined {
    if (!input || typeof input !== 'object') return undefined
    const signal = (input as { signal?: unknown }).signal
    if (signal === undefined) return undefined
    if (!(signal instanceof AbortSignal)) throw new TypeError('signal must be an AbortSignal')
    return signal
}

export function createRisunestPrivateApi(dependencies: RisunestPrivateApiDependencies) {
    const patchLedger = createConversationPatchLedger(dependencies.now)
    dependencies.lifetimeSignal.addEventListener('abort', () => {
        dependencies.chatView.dispose()
        dependencies.generationEnd.dispose()
    }, { once: true })
    async function requireHostToolPermission(): Promise<void> {
        if (!(await dependencies.hasHostToolPermission())) throw new Error('Host tools require the hostTools permission')
    }
    return {
        async readConversationContext(input?: unknown): Promise<ConversationContext | null> {
            const request = normalizeConversationContextInput(input)
            const linked = linkPluginQueryAbortSignals(inputSignal(input), dependencies.lifetimeSignal)
            try {
                throwIfAborted(linked.signal)
                const allowPrivate = request.include.persona || request.include.globals
                    ? await dependencies.hasDatabasePermission()
                    : false
                throwIfAborted(linked.signal)
                const result = await dependencies.databaseAccess.readConversationContext(request, {
                    allowPrivate,
                    signal: linked.signal,
                })
                throwIfAborted(linked.signal)
                return result
            } finally {
                linked.dispose()
            }
        },
        async patchConversation(input?: unknown): Promise<ConversationPatchResult> {
            const request = normalizeConversationPatchInput(input)
            const signal = inputSignal(input)
            return patchLedger.run(request.mutationId, async () => {
                const linked = linkPluginQueryAbortSignals(signal, dependencies.lifetimeSignal)
                try {
                    throwIfAborted(linked.signal)
                    const granted = await dependencies.hasDatabaseWritePermission()
                    throwIfAborted(linked.signal)
                    if (!granted) throw new Error('Conversation patches require the dbWrite permission')
                    return await dependencies.patchAccess.patchConversation(request, linked.signal)
                } finally {
                    linked.dispose()
                }
            })
        },
        async listHostTools(): Promise<HostToolList> {
            await requireHostToolPermission()
            throwIfAborted(dependencies.lifetimeSignal)
            return dependencies.hostTools.listTools()
        },
        async callHostTool(input?: unknown): Promise<RPCToolCallContent[]> {
            const request = normalizeHostToolCallInput(input)
            const linked = linkPluginQueryAbortSignals(inputSignal(input), dependencies.lifetimeSignal)
            try {
                throwIfAborted(linked.signal)
                await requireHostToolPermission()
                throwIfAborted(linked.signal)
                return await dependencies.hostTools.callTool(request, linked.signal)
            } finally {
                linked.dispose()
            }
        },
        onChatView(callback?: unknown): { id: string } {
            if (typeof callback !== 'function') throw new TypeError('callback must be a function')
            throwIfAborted(dependencies.lifetimeSignal)
            return dependencies.chatView.register(callback as (event: ChatViewEvent) => unknown)
        },
        onGenerationEnd(callback?: unknown): { id: string } {
            if (typeof callback !== 'function') throw new TypeError('callback must be a function')
            throwIfAborted(dependencies.lifetimeSignal)
            return dependencies.generationEnd.register(callback as (event: GenerationEndEvent) => unknown)
        },
        /** The part of `unregisterUIPart` these methods own. */
        unregisterUIPart(id: unknown): void {
            if (typeof id !== 'string') return
            dependencies.chatView.unregister(id)
            dependencies.generationEnd.unregister(id)
        },
    }
}

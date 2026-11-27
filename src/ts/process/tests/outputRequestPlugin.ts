import { vi } from 'vitest'
import { executePluginV3 } from '../../plugins/apiV3/v3.svelte'
import type { pluginV2 } from '../../plugins/plugins.svelte'
import type { ConversationPatchRequest, ConversationPatchResult } from '../../plugins/conversationPatch'
import type { PluginIframeSnapshot } from '../../plugins/pluginDatabaseAccess'
import type { GenerationEndEvent } from '../../plugins/generationEndEvents'
import { attachPluginReadProvenance } from '../../plugins/pluginReadBaselines'
import type { Chat, Database, Message } from '../../storage/database.svelte'
import type { IndexedDbPersistentDataStore } from '../../storage/indexedDbPersistentDataStore'
import type { PersistentDataRuntime } from '../../storage/persistentDataRuntime'
import * as permissions from '../../storage/nativePluginPermissions'
import { requestChatData } from '../request/request'
import type { registerMCPModule } from '../mcp/pluginmcp'
import type { MCPTool } from '../mcp/mcplib'

export const outputPluginName = 'synthetic-output-provider'

type Provider = typeof pluginV2.providers extends Map<string, infer Callback> ? Callback : never
type BeforeRequest = typeof pluginV2.replacerbeforeRequest extends Set<infer Callback> ? Callback : never
type AfterRequest = typeof pluginV2.replacerafterRequest extends Set<infer Callback> ? Callback : never

export interface OutputRequestPlugin {
    registerMCP(...args: Parameters<typeof registerMCPModule>): Promise<void>
    addProvider(name: string, callback: Provider): void
    addRisuReplacer(name: 'beforeRequest', callback: BeforeRequest): Promise<void>
    addRisuReplacer(name: 'afterRequest', callback: AfterRequest): Promise<void>
    setChatToIndex(characterIndex: number, chatIndex: number, chat: Chat): Promise<void>
    setCharacterToIndex(index: number, character: Database['characters'][number]): Promise<void>
    getChatFromIndex(characterIndex: number, chatIndex: number): Promise<PluginIframeSnapshot>
    risunestPatchConversation(request: ConversationPatchRequest): Promise<ConversationPatchResult>
    risunestOnGenerationEnd(callback: (event: GenerationEndEvent) => unknown): { id: string }
}

export interface OutputRequestFixture {
    api: OutputRequestPlugin
    runtime: PersistentDataRuntime
    store: IndexedDbPersistentDataStore
    reopen(): Promise<Chat>
    connect(): Promise<IndexedDbPersistentDataStore>
    length: number
    windowed: boolean
    runNestedRequest(tools?: MCPTool[] | 'discover'): Promise<void>
}

export async function openOutputRequestPlugin(getHostApi: () => OutputRequestPlugin) {
    vi.spyOn(permissions, 'getPluginPermissionStore').mockReturnValue({
        isGranted: async () => true,
        lastGrantAt: async () => Date.now(),
        grant: vi.fn(), recordGrant: vi.fn(), clearAll: vi.fn(),
    })
    await executePluginV3({
        name: outputPluginName, script: '', version: '3.0',
        arguments: {}, realArg: {}, argMeta: {}, customLink: [],
    })
    return getHostApi()
}

export async function runNestedRequest(tools: MCPTool[] | 'discover' = []) {
    const actual = await vi.importActual<typeof import('../request/request')>('../request/request')
    vi.mocked(requestChatData).mockImplementationOnce((arg, ...options) =>
        actual.requestChatData(tools === 'discover' ? arg : { ...arg, tools }, ...options))
}

/** Consume the conversation and its read provenance at the simulated iframe boundary. */
export async function readPluginChat(api: OutputRequestPlugin) {
    const snapshot = await api.getChatFromIndex(0, 0)
    const reader = snapshot.value.getReader()
    let chat: Chat
    try {
        for (;;) {
            const { done, value } = await reader.read()
            if (done) return chat!
            for (const chunk of value.type === 'batch' ? value.chunks : [value]) {
                if (chunk.type === 'conversationStart') chat = { ...chunk.value as Chat, message: [] }
                if (chunk.type === 'message') chat!.message.push(chunk.value as Message)
                if (chunk.type === 'provenance') attachPluginReadProvenance({ characters: [{ chats: [chat!] }] }, chunk.entries)
            }
        }
    } finally {
        reader.releaseLock()
    }
}

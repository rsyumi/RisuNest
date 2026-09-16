import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { Chat } from 'src/ts/storage/database.svelte'

const state = vi.hoisted(() => ({ chat: null as Chat | null }))

vi.mock('src/ts/storage/database.svelte', () => ({
    getCurrentChat: () => state.chat,
    getDatabase: () => ({}),
}))
vi.mock('src/ts/stores.svelte', () => ({
    DBState: { db: {} },
    selIdState: { selId: -1 },
    selectedCharID: { subscribe: () => () => {} },
}))
vi.mock('../modules', () => ({ getModuleMcps: () => [] }))
vi.mock('src/ts/alert', () => ({
    alertError: vi.fn(), alertInput: vi.fn(), alertNormal: vi.fn(),
}))
vi.mock('src/ts/platform', () => ({
    isTauri: false, isNodeServer: false, isWeb: false, isAndroid: false, isMobile: false,
    isTauriIOS: false, isTauriAndroid: false, isTauriMobile: false, isTauriDesktop: false,
    isFirefox: false, isInStandaloneMode: false, isIOS: () => false,
}))
vi.mock('./pluginmcp', () => ({ registeredCustomPluginMCPs: new Map() }))
vi.mock('./mcplib', () => ({ MCPClient: class {} }))

import { decodeToolCall, encodeToolCall, type toolCallData } from './mcp'

function conversation(): Chat {
    return { message: [], note: '', name: 'chat', localLore: [] } as unknown as Chat
}

const call = (id: string): toolCallData => ({
    call: { id, name: 'search', arg: { query: 'synthetic' } },
    response: [{ type: 'text', text: 'result' }] as toolCallData['response'],
})

describe('tool call records', () => {
    beforeEach(() => {
        state.chat = conversation()
    })

    it('keeps the call on the conversation that references it', async () => {
        const marker = await encodeToolCall(call('call-1'))

        expect(marker).toBe('<tool_call>call-1search</tool_call>\n\n')
        expect(Object.keys(state.chat!.toolCalls ?? {})).toEqual(['call-1'])
        await expect(decodeToolCall('call-1search')).resolves.toMatchObject({
            call: { id: 'call-1', name: 'search' },
        })
    })

    it('gives an id to a call that arrives without one', async () => {
        await encodeToolCall(call(''))

        const [id] = Object.keys(state.chat!.toolCalls ?? {})
        expect(id).toMatch(/^[0-9a-f-]{36}$/)
    })

    it('reports a call another conversation recorded as unknown', async () => {
        await encodeToolCall(call('call-1'))
        state.chat = conversation()

        await expect(decodeToolCall('call-1search')).resolves.toBeUndefined()
        await expect(decodeToolCall('search')).resolves.toBeUndefined()
    })
})

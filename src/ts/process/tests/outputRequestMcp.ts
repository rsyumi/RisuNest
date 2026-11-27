import { expect, it, vi } from 'vitest'
import { fetchNative } from '../../globalApi.svelte'
import type { Database } from '../../storage/database.svelte'
import { callMCPTool } from '../mcp/mcp'
import { outputPluginName, readPluginChat, type OutputRequestFixture } from './outputRequestPlugin'
import { whileStreaming } from './streamingPluginWrites'

const module = { id: 'test-mcp', name: 'Synthetic MCP', description: '', mcp: { url: 'plugin:test-mcp' } }
const client = { identifier: module.mcp.url, name: module.name, description: '', version: '1' }
const tool = { name: 'save_test_row', description: '', inputSchema: { type: 'object', properties: { text: { type: 'string' } } } }

function enableMcp(seed: Database) {
    seed.modules = [module]
    seed.enabledModules = [module.id]
}

export function registerNestedMcpCases(nestedFixture: (windowed: boolean, configure: (seed: Database) => void) => Promise<OutputRequestFixture>) {
    it('fences a GraphMem write when the library changes during plugin MCP discovery', async () => {
        const fixture = await nestedFixture(false, seed => {
            enableMcp(seed)
            seed.modules.push({ id: 'memory', name: 'GraphMem', description: '', mcp: { url: 'internal:graphmem' } })
            seed.enabledModules.push('memory')
        })
        let release!: () => void
        const waiting = new Promise<void>(resolve => { release = resolve })
        let listing = false
        await fixture.api.registerMCP(client, async () => {
            listing = true
            await waiting
            return []
        }, async () => [])
        const owner = new AbortController()
        const call = callMCPTool('writeMemory', { name: 'old owner', summary: 'stale' }, {
            signal: owner.signal, beforePlugin: async () => owner.signal.throwIfAborted(),
        }).catch(error => error)
        try {
            await vi.waitFor(() => expect(listing).toBe(true))
            const replacement = await (await fixture.connect()).materializeDatabase()
            replacement.characters[0].chats[0].scriptstate = { $graphmem_graph: '[]' }
            await fixture.runtime.replacePersistentDatabase(replacement, 'replace-during-tool-lookup')
            owner.abort(new Error('request owner replaced'))
        } finally { release() }
        const result = await call
        await fixture.runtime.flushPendingData('after-tool-lookup')
        expect((await fixture.reopen()).scriptstate.$graphmem_graph).toBe('[]')
        expect(result).toBe(owner.signal.reason)
    })

    it('checkpoints the output trigger before plugin MCP tool discovery and resumes after its setter', async () => {
        const fixture = await nestedFixture(true, seed => {
            enableMcp(seed)
            seed.aiModel = 'echo_model'
            seed.echoMessage = 'discovered'
        })
        await fixture.api.registerMCP(client, async () => {
            const chat = await readPluginChat(fixture.api)
            expect((await fixture.reopen()).message.at(-1)?.data).toBe('reply edited by trigger')
            chat.message[0].data = 'saved by tool discovery'
            await fixture.api.setChatToIndex(0, 0, chat)
            return [tool]
        }, async () => [])
        await whileStreaming(fixture, async ({ finishStream }) => {
            await fixture.runNestedRequest('discover')
            finishStream()
        })
        const saved = await fixture.reopen()
        expect(saved.message[0].data).toBe('saved by tool discovery')
        expect(saved.scriptstate.$result).toBe('discovered')
    })

    it.each(['plugin MCP tool', 'internal AI access tool', 'internal AI access tool with a streaming provider'] as const)('resumes with a saved row after the nested model stream calls the %s', async path => {
        const indirect = path !== 'plugin MCP tool'
        const childStreams = path === 'internal AI access tool with a streaming provider'
        let discardedStreamCancelled = false
        const fixture = await nestedFixture(false, seed => {
            enableMcp(seed)
            if (indirect) seed.modules[0] = { ...module, mcp: { url: 'internal:aiaccess' } }
            seed.aiModel = 'gpt4o'
            seed.useStreaming = true
            seed.characters[0].triggerscript[0].effect = [
                { type: 'modifychat', index: '1', value: 'before tool' },
                { type: 'v2RunLLM', value: 'save a row', valueType: 'value', model: 'model', streaming: true, outputVar: 'result', indent: 0 },
                { type: 'v2GetMessageAtIndex', index: '0', indexType: 'value', outputVar: 'observedRow', indent: 0 },
            ]
        })
        async function saveRow(text: string) {
            expect((await fixture.reopen()).message[1].data).toBe('before tool')
            const chat = await readPluginChat(fixture.api)
            chat.message[0].data = text
            await fixture.api.setChatToIndex(0, 0, chat)
        }
        if (indirect) {
            fixture.api.addProvider(outputPluginName, async ({ prompt_chat }) => {
                await saveRow(prompt_chat[0].content)
                return { success: true, content: childStreams ? new ReadableStream<string>({
                    cancel() { discardedStreamCancelled = true },
                }) : 'tool completed' }
            })
        } else {
            await fixture.api.registerMCP(client, async () => [tool], async (name, args) => {
                expect(name).toBe(tool.name)
                await saveRow(args.text)
                return [{ type: 'text', text: 'tool completed' }]
            })
        }
        const headers = { 'Content-Type': 'text/event-stream' }
        const stream = new TransformStream<Uint8Array, Uint8Array>()
        const writer = stream.writable.getWriter()
        const event = (delta: object) => 'data: ' + JSON.stringify({ choices: [{ index: 0, delta }] }) + '\n\ndata: [DONE]\n\n'
        vi.mocked(fetchNative).mockResolvedValueOnce(new Response(stream.readable, { headers }))
            .mockImplementationOnce(async (_url, request) => {
                const messages = JSON.parse(String(request.body)).messages
                const content = messages.at(-1).content
                const result = indirect ? JSON.parse(content) : { success: true, message: content }
                return new Response(event({ content: result.success ? result.message : 'tool failed' }), { headers })
            })
        await whileStreaming(fixture, async ({ finishStream }) => {
            await fixture.runNestedRequest(indirect ? 'discover' : [tool])
            if (indirect) await fixture.runNestedRequest('discover')
            finishStream()
            await vi.waitFor(() => expect(stream.readable.locked).toBe(true))
            expect((await fixture.reopen()).message[1].data).toBe('m1')
            await writer.write(new TextEncoder().encode(event({ tool_calls: [{
                index: 0, id: 'synthetic-call', type: 'function',
                function: indirect
                    ? { name: 'runLLM', arguments: JSON.stringify({ model: 'lite', messages: [{ role: 'user', content: 'saved by streaming tool' }] }) }
                    : { name: tool.name, arguments: JSON.stringify({ text: 'saved by streaming tool' }) },
            }] })))
            await writer.close()
        })
        const saved = await fixture.reopen()
        expect(saved.message[0].data).toBe('saved by streaming tool')
        expect(saved.scriptstate).toMatchObject({ $result: childStreams ? 'tool failed' : 'tool completed', $observedRow: 'saved by streaming tool' })
        if (childStreams) await vi.waitFor(() => expect(discardedStreamCancelled).toBe(true))
    })
}

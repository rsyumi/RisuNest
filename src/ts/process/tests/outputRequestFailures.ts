import { IDBObjectStore as FakeObjectStore } from 'fake-indexeddb'
import { expect, it, vi } from 'vitest'
import { language } from 'src/lang'
import type { Chat, Database, Message } from '../../storage/database.svelte'
import { getPluginPermissionStore } from '../../storage/nativePluginPermissions'
import * as clock from '../../util'
import { getGenerationOutputPhase } from '../generationOutputPhase'
import { outputPluginName, readPluginChat, type OutputRequestFixture } from './outputRequestPlugin'
import { whileStreaming } from './streamingPluginWrites'

const target = { characterId: 'character-a', conversationId: 'chat-a' }
type NestedFixture = (windowed: boolean, configure?: (seed: Database) => void) => Promise<OutputRequestFixture>

function gate() {
    let resolve!: () => void
    const promise = new Promise<void>(done => { resolve = done })
    return { promise, resolve }
}

async function expectSendFinished(sending: Promise<boolean>) {
    let settled = false
    void sending.then(() => { settled = true }, () => { settled = true })
    await vi.waitFor(() => expect(settled).toBe(true))
    await sending
}

async function verifySubsequentSave(fixture: OutputRequestFixture, conversationId = target.conversationId) {
    const reader = await fixture.connect()
    const read = async () => (await reader.readConversation(target.characterId, conversationId))!.value
    const row = (await read()).message[0]
    const result = await fixture.api.risunestPatchConversation({
        ...target, conversationId, mutationId: 'after-failure',
        messages: [{ index: 0, messageId: row.chatId!, set: { __laterSave: true } }], chatVariables: [],
    })
    expect(result.status).toBe('applied')
    expect((await read()).message[0]).toMatchObject({ chatId: row.chatId, __laterSave: true })
}

export function registerNestedRequestFailureCases(nestedFixture: NestedFixture) {
    it('closes a cancelled ordinary output request and releases the setter waiting for its script', async () => {
        const fixture = await nestedFixture(false, seed => {
            seed.aiModel = 'echo_model'
            seed.echoDelay = 123
            seed.echoMessage = 'late echo'
        })
        const release = gate()
        let entered = false
        const sleep = clock.sleep
        const delay = vi.spyOn(clock, 'sleep').mockImplementation(ms => {
            if (ms !== 123_000) return sleep(ms)
            entered = true
            return release.promise
        })
        try {
            await whileStreaming(fixture, async ({ cancel, finishStream, sending }) => {
                const chat = await readPluginChat(fixture.api)
                await fixture.runNestedRequest()
                finishStream()
                try {
                    await vi.waitFor(() => expect(entered).toBe(true))
                    const queue = vi.spyOn(fixture.runtime, 'withLiveConversationWrite')
                    chat.note = 'saved after cancellation'
                    let saved = false
                    const write = fixture.api.setChatToIndex(0, 0, chat).then(() => { saved = true })
                    // Let the setter reach the live write queue and yield it to the script.
                    await vi.waitFor(() => expect(queue.mock.calls.some(([reason]) => reason === 'plugin-setChatToIndex')).toBe(true))
                    await fixture.runtime.withLiveConversationWrite('before-cancellation', async () => {})
                    expect(saved).toBe(false)
                    cancel()
                    await vi.waitFor(() => expect(saved).toBe(true))
                    await write
                    expect((await fixture.reopen()).note).toBe(chat.note)
                    await expectSendFinished(sending)
                    expect(getGenerationOutputPhase(target)).toBeNull()
                } finally { release.resolve() }
            }, undefined, false)
            expect((await fixture.reopen()).scriptstate?.$result).toBeUndefined()
        } finally {
            release.resolve()
            delay.mockRestore()
        }
    })

    it('honors dbWrite denial inside a registered output provider', async () => {
        const fixture = await nestedFixture(true)
        const permission = vi.spyOn(getPluginPermissionStore(), 'isGranted').mockImplementation(async (_hash, key) => key !== 'dbWrite')
        let denied = false
        fixture.api.addProvider(outputPluginName, async () => {
            const chat = await readPluginChat(fixture.api)
            chat.message[0].data = 'denied write'
            await fixture.api.setChatToIndex(0, 0, chat)
            denied = (await fixture.reopen()).message[0].data === 'm0'
            return { success: true, content: 'provider returned' }
        })
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() })
            expect(denied).toBe(true)
            const final = await fixture.reopen()
            expect(final.message[0].data).toBe('m0')
            expect(final.scriptstate?.$result).toBe('provider returned')
        } finally { permission.mockRestore() }
    })

    it.each(['setter', 'patch'] as const)('fences a provider %s awaiting dbWrite permission across a library replacement', async kind => {
        const fixture = await nestedFixture(false)
        const grant = gate()
        let asking = false
        const permissions = vi.spyOn(getPluginPermissionStore(), 'isGranted').mockImplementation(async (_hash, key) => key !== 'dbWrite')
        const { alertConfirm } = await import('../../alert')
        vi.mocked(alertConfirm).mockImplementationOnce(async () => {
            asking = true
            await grant.promise
            return true
        })
        let write: Promise<unknown> | undefined
        fixture.api.addProvider(outputPluginName, async () => {
            const chat = await readPluginChat(fixture.api)
            if (kind === 'setter') {
                chat.note = 'old provider note'
                write = fixture.api.setChatToIndex(0, 0, chat)
            } else {
                write = fixture.api.risunestPatchConversation({
                    ...target, mutationId: 'old-provider-patch', chatVariables: [],
                    messages: [{ index: 0, messageId: chat.message[0].chatId!, set: { __oldProvider: true } }],
                })
            }
            await write.catch(() => {})
            return { success: true, content: 'late result' }
        })
        try {
            await whileStreaming(fixture, async ({ finishStream, sending }) => {
                await fixture.runNestedRequest()
                finishStream()
                try {
                    await vi.waitFor(() => expect(asking).toBe(true))
                    const replacement = await (await fixture.connect()).materializeDatabase()
                    const sameId = replacement.characters[0].chats[0]
                    expect(sameId.message.at(-1)?.data).toBe('reply edited by trigger')
                    sameId.message = [{ ...sameId.message[0], data: 'new library' }]
                    sameId.note = 'new note'
                    sameId.scriptstate = {}
                    await fixture.runtime.replacePersistentDatabase(replacement, 'replace-during-permission')
                    await expectSendFinished(sending)
                } finally { grant.resolve() }
            }, undefined, false)
            await expect(write).rejects.toThrow()
            const stored = await fixture.reopen()
            expect(stored.message).toHaveLength(1)
            expect(stored.message[0]).toMatchObject({ chatId: 'id-0', data: 'new library' })
            expect(stored.message[0]).not.toHaveProperty('__oldProvider')
            expect(stored.note).toBe('new note')
            expect(stored.scriptstate?.$result).toBeUndefined()
            await verifySubsequentSave(fixture)
        } finally {
            grant.resolve()
            permissions.mockRestore()
        }
    })

    const abortFirstPut = <T>(store: string, matches: (value: T) => boolean) => abortPuts(store, matches, false)

    // A script's edit is saved by whichever save runs next, which can be a background save instead of
    // the request's own, so a case that fails that save fails every save of the edit.
    function abortPuts<T>(store: string, matches: (value: T) => boolean, every: boolean) {
        let aborted = false
        const put = FakeObjectStore.prototype.put
        const spy = vi.spyOn(FakeObjectStore.prototype, 'put').mockImplementation(function (this: IDBObjectStore, value, key) {
            const request = put.call(this, value, key)
            if ((every || !aborted) && this.name === store && matches(value.value)) {
                aborted = true
                this.transaction.abort()
            }
            return request
        })
        return { get aborted() { return aborted }, restore: () => spy.mockRestore() }
    }

    it.each(['setter', 'patch'] as const)('stops the output trigger when the provider catches a failed %s save', async kind => {
        const fixture = await nestedFixture(kind === 'patch', seed => {
            seed.characters[0].triggerscript[0].effect.push({ type: 'modifychat', index: '1', value: 'after failed save' })
        })
        let rejected = false
        fixture.api.addProvider(outputPluginName, async () => {
            try {
                if (kind === 'setter') {
                    const chat = await readPluginChat(fixture.api)
                    chat.message[0].data = 'faulted edit'
                    await fixture.api.setChatToIndex(0, 0, chat)
                } else {
                    await fixture.api.risunestPatchConversation({
                        ...target, mutationId: 'faulted-patch', chatVariables: [],
                        messages: [{ index: 0, messageId: 'id-0', set: { data: 'faulted edit' } }],
                    })
                }
            } catch { rejected = true }
            return { success: true, content: 'provider recovered' }
        })
        const fault = abortFirstPut('messagePages', (rows: Message[]) => rows.some(row => row.data === 'faulted edit'))
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() }, undefined, /AbortError|TransactionInactiveError|InvalidStateError/)
            expect(fault.aborted).toBe(true)
            await vi.waitFor(() => expect(rejected).toBe(true))
            const stored = await fixture.reopen()
            expect(stored.message.at(-1)?.data).toBe('reply edited by trigger')
            expect(stored.message[1].data).toBe('m1')
            expect(stored.scriptstate?.$result).toBeUndefined()
            if (kind === 'patch') expect(stored.message[0].data).toBe('m0')
        } finally { fault.restore() }
        await verifySubsequentSave(fixture)
    })

    it.each([true, false])('propagates a character save failure according to whether the setter also edited this chat (%s)', async changesChat => {
        const fixture = await nestedFixture(false)
        const reader = await fixture.connect()
        const { getDatabase } = await import('../../storage/database.svelte')
        const { safeStructuredClone } = await import('../../safeStructuredClone')
        let rejected = false
        fixture.api.addProvider(outputPluginName, async () => {
            const character = safeStructuredClone(getDatabase().characters[0])
            character.desc = 'faulted character edit'
            if (changesChat) character.chats[0].message[0].data = 'saved by mixed setter'
            try { await fixture.api.setCharacterToIndex(0, character) }
            catch { rejected = true }
            return { success: true, content: 'provider recovered' }
        })
        const fault = abortFirstPut('characters', (character: { desc: string }) => character.desc === 'faulted character edit')
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() }, undefined,
                changesChat ? /AbortError|TransactionInactiveError|InvalidStateError/ : true)
            expect(fault.aborted).toBe(true)
            await vi.waitFor(() => expect(rejected).toBe(true))
            const character = (await reader.materializeDatabase()).characters[0]
            expect(character.desc).not.toBe('faulted character edit')
            expect(character.chats[0].message[0].data).toBe(changesChat ? 'saved by mixed setter' : 'm0')
            expect(character.chats[0].scriptstate?.$result).toBe(changesChat ? undefined : 'provider recovered')
        } finally { fault.restore() }
        await verifySubsequentSave(fixture)
    })

    it('aborts a failed IndexedDB pre-dispatch save before calling the registered provider', async () => {
        const fixture = await nestedFixture(false)
        let invoked = false
        fixture.api.addProvider(outputPluginName, async () => {
            invoked = true
            return { success: true, content: 'unexpected callback' }
        })
        const fault = abortPuts<Message[]>('messagePages', rows => rows.some(row => row.data === 'reply edited by trigger'), true)
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() }, undefined, /AbortError|TransactionInactiveError|InvalidStateError/)
            expect(fault.aborted).toBe(true)
            expect(invoked).toBe(false)
            expect((await fixture.reopen()).scriptstate?.$result).toBeUndefined()
        } finally { fault.restore() }
        await verifySubsequentSave(fixture)
    })

    it('ends a pending ordinary Lua request when a sibling plugin checkpoint fails', async () => {
        const fixture = await nestedFixture(false, seed => {
            seed.aiModel = 'echo_model'
            seed.echoDelay = 123
            seed.characters[0].triggerscript[0].effect = [{ type: 'triggerlua', code: `
                onOutput = async(function(id)
                    local echo = simpleLLM(id, 'echo')
                    alertInput(id, 'wait for echo'):await()
                    setChat(id, 999, 'sibling checkpoint')
                    local plugin = axLLMMain(id, '[{"role":"user","content":"save"}]', false, '{}')
                    setChatVar(id, 'result', echo:await().result)
                    plugin:await()
                end)
            ` }]
        })
        const entered = gate()
        const release = gate()
        const sleep = clock.sleep
        const delay = vi.spyOn(clock, 'sleep').mockImplementation(ms => {
            if (ms !== 123_000) return sleep(ms)
            entered.resolve()
            return release.promise
        })
        const { alertInput } = await import('../../alert')
        vi.mocked(alertInput).mockImplementationOnce(async () => { await entered.promise; return '' })
        const provider = vi.fn(async () => ({ success: true, content: 'unexpected callback' }))
        fixture.api.addProvider(outputPluginName, provider)
        const fault = abortPuts<Message[]>('messagePages', rows => rows.some(row => row.data === 'sibling checkpoint'), true)
        try {
            await whileStreaming(fixture, async ({ finishStream, sending }) => {
                await fixture.runNestedRequest()
                await fixture.runNestedRequest()
                finishStream()
                try {
                    await vi.waitFor(() => expect(fault.aborted).toBe(true))
                    await expectSendFinished(sending)
                } finally { release.resolve() }
            }, undefined, /AbortError|TransactionInactiveError|InvalidStateError/)
            expect(provider).not.toHaveBeenCalled()
            expect((await fixture.reopen()).scriptstate?.$result).toBeUndefined()
        } finally { delay.mockRestore(); fault.restore(); release.resolve() }
        await verifySubsequentSave(fixture)
    })

    it.each(['returns', 'reads'] as const)('cancels while Lua %s during a pending plugin checkpoint', async action => {
        const fixture = await nestedFixture(false, seed => {
            seed.characters[0].triggerscript[0].effect = [{ type: 'triggerlua', code: `
                onOutput = async(function(id)
                    setChat(id, 999, 'checkpoint before cancel')
                    simpleLLM(id, 'save')
                    alertInput(id, 'wait for checkpoint'):await()
                    alertNormal(id, 'checkpoint pending')
                    ${action === 'reads' ? "setChatVar(id, 'result', getChatData(id, 999))" : ''}
                end)
            ` }]
        })
        const entered = gate()
        const release = gate()
        const flush = fixture.runtime.flushPendingDataLocally.bind(fixture.runtime)
        const checkpoint = vi.spyOn(fixture.runtime, 'flushPendingDataLocally').mockImplementation(async reason => {
            await flush(reason)
            if (reason === 'output-script-plugin-request') { entered.resolve(); await release.promise }
        })
        const { alertInput, alertNormal } = await import('../../alert')
        vi.mocked(alertInput).mockImplementationOnce(async () => { await entered.promise; return '' })
        vi.mocked(alertNormal).mockClear()
        const provider = vi.fn(async () => ({ success: true, content: 'unexpected callback' }))
        fixture.api.addProvider(outputPluginName, provider)
        try {
            await whileStreaming(fixture, async ({ finishStream, cancel, sending }) => {
                await fixture.runNestedRequest()
                finishStream()
                try {
                    await vi.waitFor(() => expect(alertNormal).toHaveBeenCalledWith('checkpoint pending'))
                    cancel()
                    await expectSendFinished(sending)
                } finally { release.resolve() }
            }, undefined, false)
            expect(provider).not.toHaveBeenCalled()
            expect((await fixture.reopen()).scriptstate?.$result).toBeUndefined()
        } finally { checkpoint.mockRestore(); release.resolve() }
    })

    it('returns a registered provider failure to the trigger and runs its remaining effects and output listener', async () => {
        const fixture = await nestedFixture(false, seed => {
            seed.characters[0].triggerscript[0].effect.push({ type: 'modifychat', index: '1', value: 'after failure' })
        })
        fixture.api.addProvider(outputPluginName, async () => {
            const chat = await readPluginChat(fixture.api)
            chat.message[0].data = 'saved before failure'
            await fixture.api.setChatToIndex(0, 0, chat)
            return { success: false, content: 'provider failure' }
        })
        const { pluginV2 } = await import('../../plugins/plugins.svelte')
        const observed: string[] = []
        const listener = async ({ chat }: { chat: Chat }) => { observed.push(chat.message[1].data) }
        pluginV2.chatOutput.add(listener)
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() })
            const stored = await fixture.reopen()
            expect(stored.scriptstate.$result).toBe('Error: provider failure')
            expect(stored.message[0].data).toBe('saved before failure')
            expect(stored.message[1].data).toBe('after failure')
            expect(observed).toEqual(['after failure'])
        } finally { pluginV2.chatOutput.delete(listener) }
    })

    it('returns an ordinary provider HTTP error to Lua after a read-only request hook', async () => {
        const fixture = await nestedFixture(true, seed => {
            seed.aiModel = 'gpt4'
            seed.characters[0].triggerscript[0].effect = [{ type: 'triggerlua', code: `
                onOutput = async(function(id)
                    local response = simpleLLM(id, 'translate'):await()
                    if not response.success then setChatVar(id, 'failure', response.result) end
                end)
            ` }]
        })
        await fixture.api.addRisuReplacer('beforeRequest', async messages => messages)
        const network = await import('../../globalApi.svelte')
        const fetch = vi.spyOn(network, 'globalFetch').mockResolvedValue({
            ok: false, status: 429, headers: {}, data: { error: { message: 'rate limited' } },
        })
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() })
            expect((await fixture.reopen()).scriptstate.$failure).toBe('Error: ' + language.errors.httpError + 'rate limited')
        } finally { fetch.mockRestore() }
    })

    it('returns a provider stream error to Lua and refreshes its view of the saved chat', async () => {
        const fixture = await nestedFixture(true, seed => {
            seed.characters[0].triggerscript[0].effect = [{ type: 'triggerlua', code: `
                onOutput = async(function(id)
                    local response = LLM(id, {{role = 'user', content = 'translate'}}, false, {streaming = true})
                    if not response.success then setChatVar(id, 'failure', response.result) end
                    setChatVar(id, 'observed', getChat(id, 999).data)
                end)
            ` }]
        })
        fixture.api.addProvider(outputPluginName, async () => {
            const chat = await readPluginChat(fixture.api)
            chat.message[999].data = 'saved before stream error'
            await fixture.api.setChatToIndex(0, 0, chat)
            return { success: true, content: new ReadableStream<string>({
                pull(controller) { controller.error('stream interrupted') },
            }) }
        })
        await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() })
        expect((await fixture.reopen()).scriptstate).toMatchObject({
            $failure: 'Error: stream interrupted', $observed: 'saved before stream error',
        })
    })

    it('stops v2RunLLM when its ordinary provider stream cannot be read', async () => {
        const fixture = await nestedFixture(false, seed => {
            seed.aiModel = 'gpt4o'
            seed.useStreaming = true
            seed.characters[0].triggerscript[0].effect.splice(1, 1,
                { type: 'v2RunLLM', value: 'translate', valueType: 'value', model: 'model', streaming: true, outputVar: 'result', indent: 0 },
                { type: 'modifychat', index: '1', value: 'after stream error' },
            )
        })
        const { fetchNative } = await import('../../globalApi.svelte')
        vi.mocked(fetchNative).mockResolvedValueOnce(new Response(new ReadableStream({
            pull(controller) { controller.error(new Error('stream interrupted')) },
        }), { headers: { 'Content-Type': 'text/event-stream' } }))
        await whileStreaming(fixture, async ({ finishStream }) => {
            await fixture.runNestedRequest()
            finishStream()
        }, undefined, /stream interrupted/)
        const stored = await fixture.reopen()
        expect(stored.scriptstate?.$result).toBeUndefined()
        expect(stored.message[1].data).toBe('m1')
    })

    it.each(['cancellation', 'owner deletion', 'library replacement'] as const)(
        'fences the suspended output invocation after %s', async reason => {
            const fixture = await nestedFixture(false, seed => {
                const owner = seed.characters[0]
                if (reason === 'owner deletion') owner.chats.push({
                    ...owner.chats[0], id: 'sibling', message: [{ role: 'user', data: 'sibling row', chatId: 'survivor' }],
                })
                owner.triggerscript[0].effect.push({ type: 'modifychat', index: '0', value: 'late trigger edit' })
            })
            const release = gate()
            let saved = false
            let writeAtFlush = false
            let lateWrite: Promise<void> | undefined
            fixture.api.addProvider(outputPluginName, async () => {
                const chat = await readPluginChat(fixture.api)
                chat.message[0].data = 'saved before failure'
                await fixture.api.setChatToIndex(0, 0, chat)
                saved = true
                if (reason === 'library replacement') {
                    const beforeReplacement = await readPluginChat(fixture.api)
                    beforeReplacement.note = 'note from the former owner'
                    const flush = fixture.runtime.flushPendingDataLocally
                    vi.spyOn(fixture.runtime, 'flushPendingDataLocally').mockImplementationOnce(async reason => {
                        writeAtFlush = true
                        await release.promise
                        return flush(reason)
                    })
                    lateWrite = fixture.api.setChatToIndex(0, 0, beforeReplacement)
                    await lateWrite.catch(() => {})
                }
                await release.promise
                return { success: true, content: 'late provider result' }
            })
            const reader = await fixture.connect()
            const read = (id: string) => reader.readConversation(target.characterId, id)
            try {
                await whileStreaming(fixture, async ({ cancel, finishStream, sending }) => {
                    await fixture.runNestedRequest()
                    finishStream()
                    try {
                        await vi.waitFor(() => expect(saved).toBe(true), { timeout: 3000 })
                        const old = (await read(target.conversationId))!.value
                        expect(old.message[0].data).toBe('saved before failure')
                        expect(old.message.at(-1)?.data).toBe('reply edited by trigger')
                        if (reason === 'cancellation') cancel()
                        if (reason === 'owner deletion') {
                            await fixture.runtime.commitPersistentUnitIntent('delete-owner', [], [{ ...target, type: 'delete' }])
                        }
                        if (reason === 'library replacement') {
                            await vi.waitFor(() => expect(writeAtFlush).toBe(true))
                            const replacement = await reader.materializeDatabase()
                            replacement.characters[0].chats[0] = {
                                ...old, message: [{ ...old.message[0], data: 'replacement library' }], scriptstate: {},
                            }
                            await fixture.runtime.replacePersistentDatabase(replacement, 'replace-library')
                        }
                        await expectSendFinished(sending)
                    } finally { release.resolve() }
                }, undefined, false)
                const currentId = reason === 'owner deletion' ? 'sibling' : target.conversationId
                await verifySubsequentSave(fixture, currentId)
                const stored = (await read(currentId))!.value
                expect(stored.scriptstate?.$result).toBeUndefined()
                if (reason === 'owner deletion') {
                    expect(await read(target.conversationId)).toBeNull()
                    expect(stored.message.map(row => row.data)).toEqual(['sibling row'])
                } else if (reason === 'library replacement') {
                    await expect(lateWrite).rejects.toThrow()
                    expect(stored.note).toBe('')
                    expect(stored.message.map(row => row.data)).toEqual(['replacement library'])
                } else {
                    expect(stored.message[0].data).toBe('saved before failure')
                    expect(stored.message.at(-1)?.data).toBe('reply edited by trigger')
                }
            } finally { release.resolve() }
        },
    )
}

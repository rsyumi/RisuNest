import { IDBObjectStore as FakeObjectStore } from 'fake-indexeddb'
import { expect, it, vi } from 'vitest'
import type { Database, Message } from '../../storage/database.svelte'
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

    it('aborts a failed IndexedDB pre-dispatch save before calling the registered provider', async () => {
        const fixture = await nestedFixture(false)
        let invoked = false
        fixture.api.addProvider(outputPluginName, async () => {
            invoked = true
            return { success: true, content: 'unexpected callback' }
        })
        let failed = false
        const put = FakeObjectStore.prototype.put
        const fault = vi.spyOn(FakeObjectStore.prototype, 'put').mockImplementation(function (this: IDBObjectStore, value, key) {
            const request = put.call(this, value, key)
            if (!failed && this.name === 'messagePages' && value.value?.some((row: Message) => row.data === 'reply edited by trigger')) {
                failed = true
                this.transaction.abort()
            }
            return request
        })
        try {
            await whileStreaming(fixture, async ({ finishStream }) => { await fixture.runNestedRequest(); finishStream() }, undefined, /AbortError|TransactionInactiveError|InvalidStateError/)
            expect(failed).toBe(true)
            expect(invoked).toBe(false)
            expect((await fixture.reopen()).scriptstate?.$result).toBeUndefined()
        } finally { fault.mockRestore() }
        await verifySubsequentSave(fixture)
    })

    it.each(['provider failure', 'cancellation', 'owner deletion', 'library replacement'] as const)(
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
                if (reason === 'provider failure') return { success: false, content: 'provider failure' }
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
                }, undefined, reason === 'provider failure' ? /provider failure/ : false)
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

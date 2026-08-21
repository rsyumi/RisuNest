import { describe, expect, it } from 'vitest'
import type { PersistentDataStore } from '../persistentDataStore'
import { RevisionConflictError } from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'

export interface PersistentDataStoreHarness {
    store: PersistentDataStore
    reopen(): Promise<PersistentDataStore>
}

export function persistentDataStoreContract(createHarness: () => Promise<PersistentDataStoreHarness>): void {
    describe('PersistentDataStore contract', () => {
        it('queries the character catalog without hydrating conversations', async () => {
            const { store } = await createHarness()
            await store.replaceFromDatabase(fixtureDatabase)

            expect(
                (await store.queryCharacters({ order: 'configured', trash: false, limit: 2 })).items.map(
                    (item) => item.id,
                ),
            ).toEqual(['char-b', 'char-a'])
            expect(
                (await store.queryCharacters({ order: 'recent', trash: false, limit: 10 })).items.map(
                    (item) => item.id,
                ),
            ).toEqual(['char-a', 'char-b'])
            expect(
                (
                    await store.queryCharacters({
                        search: 'beta',
                        order: 'configured',
                        trash: false,
                        limit: 10,
                    })
                ).items.map((item) => item.id),
            ).toEqual(['char-b'])

            const detail = await store.readCharacter('char-a')
            expect(detail?.value.chaId).toBe('char-a')
            expect(detail?.value).not.toHaveProperty('chats')
        })

        it('reads latest and anchored windows across an internal page boundary', async () => {
            const { store } = await createHarness()
            await store.replaceFromDatabase(fixtureDatabase)

            const latest = await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                limit: 4,
            })
            expect(latest?.value.messages.map((message) => message.chatId)).toEqual([
                'msg-126',
                'msg-127',
                'msg-128',
                'msg-129',
            ])
            expect(latest?.value).toMatchObject({
                startIndex: 126,
                endIndex: 130,
                totalMessages: 130,
                hasMoreBefore: true,
                hasMoreAfter: false,
            })

            const anchored = await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'msg-127',
                before: 2,
                after: 1,
            })
            expect(anchored?.value.messages.map((message) => message.chatId)).toEqual([
                'msg-125',
                'msg-126',
                'msg-127',
                'msg-128',
            ])
            expect(anchored?.value).toMatchObject({
                startIndex: 125,
                endIndex: 129,
                totalMessages: 130,
                hasMoreBefore: true,
                hasMoreAfter: true,
            })
        })

        it('preserves catalog and message results after reopening', async () => {
            const harness = await createHarness()
            await harness.store.replaceFromDatabase(fixtureDatabase)
            const reopened = await harness.reopen()

            expect(
                (await reopened.queryCharacters({ order: 'configured', trash: true, limit: 10 })).items.map(
                    (item) => item.id,
                ),
            ).toEqual(['char-c'])
            expect(
                (
                    await reopened.readConversationWindow({
                        characterId: 'char-a',
                        conversationId: 'conv-long',
                        limit: 2,
                    })
                )?.value.messages.map((message) => message.chatId),
            ).toEqual(['msg-128', 'msg-129'])
            const conversation = await reopened.readConversation('char-a', 'conv-short')
            expect(conversation?.revision).toBe(1)
            expect(conversation?.value).toEqual(fixtureDatabase.characters[1].chats[1])
        })

        it('rejects stale commits without changing the current data', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)

            await expect(
                store.commit({ expectedRevision: imported.revision - 1, deleteCharacterId: 'char-a' }),
            ).rejects.toBeInstanceOf(RevisionConflictError)
            expect((await store.readCharacter('char-a'))?.revision).toBe(imported.revision)
        })

        it('commits a replacement range and increments the revision once', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const committed = await store.commit({
                expectedRevision: imported.revision,
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'conv-long',
                        start: 0,
                        deleteCount: 130,
                        messages: [{ role: 'user', data: 'replacement', chatId: 'msg-replacement' }],
                    },
                ],
            })

            expect(committed.revision).toBe(imported.revision + 1)
            expect((await store.readRoot()).value.username).toBe('Fixture User')
            expect(
                (
                    await store.readConversationWindow({
                        characterId: 'char-a',
                        conversationId: 'conv-long',
                        limit: 10,
                    })
                )?.value.messages.map((message) => message.chatId),
            ).toEqual(['msg-replacement'])
            expect(
                (
                    await store.queryConversations({
                        characterId: 'char-a',
                        order: 'configured',
                        limit: 10,
                    })
                ).items[0].messageCount,
            ).toBe(1)
            expect((await store.materializeDatabase()).characters[1].chats[0].message).toEqual([
                { role: 'user', data: 'replacement', chatId: 'msg-replacement' },
            ])
        })

        it('atomically replaces the selected character and root while preserving catalog order', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const replacement = structuredClone(fixtureDatabase.characters[1])
            const longChat = replacement.chats[0]
            const shortChat = replacement.chats[1]
            shortChat.name = 'Renamed inactive chat'
            longChat.message = longChat.message.slice(0, 1)
            replacement.chats = [
                shortChat,
                longChat,
                {
                    id: 'conv-added',
                    name: 'Added chat',
                    note: 'supplied ID',
                    localLore: [],
                    message: [{ role: 'user', data: 'added', chatId: 'msg-added' }],
                    lastDate: 500,
                },
            ]
            const root = (await store.readRoot()).value

            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Committed with character' },
                replaceCharacter: replacement,
            })

            expect(committed.revision).toBe(imported.revision + 1)
            expect((await store.readRoot()).value.username).toBe('Committed with character')
            expect(
                (await store.queryCharacters({ order: 'configured', trash: false, limit: 10 })).items,
            ).toMatchObject([
                { id: 'char-b', configuredIndex: 0 },
                { id: 'char-a', configuredIndex: 1, conversationCount: 3 },
            ])
            expect(
                (
                    await store.queryConversations({
                        characterId: 'char-a',
                        order: 'configured',
                        limit: 10,
                    })
                ).items.map((item) => item.id),
            ).toEqual(['conv-short', 'conv-long', 'conv-added'])
            expect((await store.readConversation('char-a', 'conv-short'))?.value.name).toBe(
                'Renamed inactive chat',
            )
            expect((await store.readConversation('char-a', 'conv-long'))?.value.message).toHaveLength(1)
            expect(await store.readConversation('char-a', 'conv-added')).toMatchObject({
                revision: imported.revision + 1,
                value: {
                    id: 'conv-added',
                    note: 'supplied ID',
                    message: [{ chatId: 'msg-added', data: 'added' }],
                },
            })
        })

        it('appends a new character after the greatest configured index despite catalog gaps', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const afterDelete = await store.commit({
                expectedRevision: imported.revision,
                deleteCharacterId: 'char-a',
            })
            const replacement = structuredClone(fixtureDatabase.characters[1])
            replacement.chaId = 'char-new'
            replacement.name = 'New character'

            await store.commit({
                expectedRevision: afterDelete.revision,
                replaceCharacter: replacement,
            })

            expect(
                (await store.queryCharacters({ order: 'configured', trash: false, limit: 10 })).items,
            ).toMatchObject([
                { id: 'char-b', configuredIndex: 0 },
                { id: 'char-new', configuredIndex: 3 },
            ])
        })

        it('removes omitted conversations and their message pages', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const replacement = structuredClone(fixtureDatabase.characters[1])
            replacement.chats = [replacement.chats[1]]

            const committed = await store.commit({
                expectedRevision: imported.revision,
                replaceCharacter: replacement,
            })

            expect(await store.readConversation('char-a', 'conv-long')).toBeNull()
            expect(
                await store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    limit: 10,
                }),
            ).toBeNull()
            expect(
                (
                    await store.queryConversations({
                        characterId: 'char-a',
                        order: 'configured',
                        limit: 10,
                    })
                ).items.map((item) => item.id),
            ).toEqual(['conv-short'])
            expect((await store.readConversation('char-a', 'conv-short'))?.revision).toBe(
                committed.revision,
            )
        })

        it('rejects invalid selected-character IDs without changing revision or data', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const original = await store.readConversation('char-a', 'conv-long')

            for (const invalidId of ['', 'conv-long']) {
                const replacement = structuredClone(fixtureDatabase.characters[1])
                replacement.chats[1].id = invalidId
                await expect(
                    store.commit({
                        expectedRevision: imported.revision,
                        replaceCharacter: replacement,
                    }),
                ).rejects.toThrow('unique, nonempty chat IDs')
                expect(await store.readConversation('char-a', 'conv-long')).toEqual(original)
                expect((await store.readRoot()).revision).toBe(imported.revision)
            }

            const missingCharacterId = structuredClone(fixtureDatabase.characters[1])
            missingCharacterId.chaId = ''
            await expect(
                store.commit({
                    expectedRevision: imported.revision,
                    replaceCharacter: missingCharacterId,
                }),
            ).rejects.toThrow('nonempty character ID')
            expect((await store.readRoot()).revision).toBe(imported.revision)

            const invalidAndStale = structuredClone(fixtureDatabase.characters[1])
            invalidAndStale.chats[0].id = ''
            await expect(
                store.commit({
                    expectedRevision: imported.revision - 1,
                    replaceCharacter: invalidAndStale,
                }),
            ).rejects.toBeInstanceOf(RevisionConflictError)
        })

        it('aborts a failed transaction without changing its revision or data', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const rootBefore = await store.readRoot()
            const detail = (await store.readCharacter('char-a'))!.value

            await expect(
                store.commit({
                    expectedRevision: imported.revision,
                    root: { ...rootBefore.value, username: 'Must not persist' },
                    character: {
                        ...detail,
                        invalidFixtureValue: () => undefined,
                    } as unknown as typeof detail,
                }),
            ).rejects.toThrow()

            expect(await store.readRoot()).toEqual(rootBefore)
            expect((await store.readCharacter('char-a'))?.value.name).toBe('Alpha')
        })

        it('does not activate an invalid staged replacement', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const invalidDatabase = structuredClone(fixtureDatabase)
            invalidDatabase.characters[1].chaId = 'char-b'

            await expect(store.replaceFromDatabase(invalidDatabase)).rejects.toThrow(
                'unique character IDs',
            )

            expect((await store.readRoot()).revision).toBe(imported.revision)
            expect((await store.readCharacter('char-a'))?.value.name).toBe('Alpha')
        })
    })
}

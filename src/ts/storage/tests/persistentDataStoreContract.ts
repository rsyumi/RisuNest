import { describe, expect, it } from 'vitest'
import type { Database, groupChat } from '../database.svelte'
import type { PersistentDataStore } from '../persistentDataStore'
import { RevisionConflictError, SnapshotReleasedError } from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'

export interface PersistentDataStoreHarness {
    store: PersistentDataStore
    reopen(): Promise<PersistentDataStore>
}

export function persistentDataStoreContract(createHarness: () => Promise<PersistentDataStoreHarness>): void {
    describe('PersistentDataStore contract', () => {
        it('stores plugin values outside root and materializes the legacy object losslessly', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.pluginCustomStorage = { fixture: { value: 'stored' } }
            const imported = await store.replaceFromDatabase(database)

            expect((await store.readRoot()).value).not.toHaveProperty('pluginCustomStorage')
            expect(await store.queryPluginStorage()).toEqual({
                revision: imported.revision,
                items: [
                    {
                        key: 'fixture',
                        byteSize: new TextEncoder().encode(
                            JSON.stringify(database.pluginCustomStorage.fixture),
                        ).byteLength,
                    },
                ],
            })
            expect((await store.readPluginStorage('fixture'))?.value).toEqual(
                database.pluginCustomStorage.fixture,
            )
            expect(await store.readPluginStorage('missing')).toBeNull()
            expect((await store.materializeDatabase()).pluginCustomStorage).toEqual(
                database.pluginCustomStorage,
            )
        })

        it('atomically mutates plugin keys with root under revision CAS', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.pluginCustomStorage = { alpha: 'old', beta: { keep: false } }
            const imported = await store.replaceFromDatabase(database)
            const root = (await store.readRoot()).value

            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Plugin commit' },
                pluginStorage: [
                    { type: 'set', key: 'alpha', value: 'new' },
                    { type: 'delete', key: 'beta' },
                    { type: 'set', key: 'gamma', value: [1, 2, 3] },
                ],
            })

            expect((await store.readRoot()).value.username).toBe('Plugin commit')
            expect((await store.queryPluginStorage()).items.map((item) => item.key)).toEqual([
                'alpha',
                'gamma',
            ])
            expect((await store.materializeDatabase()).pluginCustomStorage).toEqual({
                alpha: 'new',
                gamma: [1, 2, 3],
            })

            await expect(store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Stale plugin commit' },
                pluginStorage: [{ type: 'clear' }],
            })).rejects.toBeInstanceOf(RevisionConflictError)
            expect((await store.readRoot()).revision).toBe(committed.revision)
            expect((await store.readRoot()).value.username).toBe('Plugin commit')
            expect((await store.materializeDatabase()).pluginCustomStorage).toEqual({
                alpha: 'new',
                gamma: [1, 2, 3],
            })

            await store.commit({
                expectedRevision: committed.revision,
                pluginStorage: [{ type: 'clear' }],
            })
            expect((await store.queryPluginStorage()).items).toEqual([])
        })

        it('isolates plugin reads through a revision lease and rejects them after release', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.pluginCustomStorage = { memory: { revision: 1 } }
            const imported = await store.replaceFromDatabase(database)
            const lease = await store.acquireRevision(imported.revision)

            await store.commit({
                expectedRevision: imported.revision,
                pluginStorage: [{ type: 'set', key: 'memory', value: { revision: 2 } }],
            })

            expect((await lease.queryPluginStorage()).items.map((item) => item.key)).toEqual([
                'memory',
            ])
            expect((await lease.readPluginStorage('memory'))?.value).toEqual({ revision: 1 })
            expect((await store.readPluginStorage('memory'))?.value).toEqual({ revision: 2 })
            await lease.release()
            await expect(lease.readPluginStorage('memory')).rejects.toBeInstanceOf(
                SnapshotReleasedError,
            )
        })

        it('preserves legacy Object.keys plugin ordering across mutation and reopen', async () => {
            const { store, reopen } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const storage: Record<string, unknown> = {}
            storage.zeta = 'first string'
            storage['10'] = 'ten'
            storage['2'] = 'two'
            storage['01'] = 'non-index'
            storage['4294967294'] = 'largest index'
            storage['4294967295'] = 'non-index boundary'
            storage['\uffffx'] = 'high unicode'
            database.pluginCustomStorage = storage
            const imported = await store.replaceFromDatabase(database)
            const originalOrder = Object.keys(storage)

            expect((await store.queryPluginStorage()).items.map((item) => item.key)).toEqual(
                originalOrder,
            )
            expect(Object.keys((await store.materializeDatabase()).pluginCustomStorage)).toEqual(
                originalOrder,
            )
            expect((await store.readPluginStorage('\uffffx'))?.value).toBe('high unicode')

            const updated = await store.commit({
                expectedRevision: imported.revision,
                pluginStorage: [
                    { type: 'set', key: 'zeta', value: 'updated in place' },
                    { type: 'delete', key: 'zeta' },
                    { type: 'set', key: 'zeta', value: 'reinserted last' },
                ],
            })
            const expectedAfterReinsert = originalOrder.filter((key) => key !== 'zeta')
            expectedAfterReinsert.push('zeta')
            const reopened = await reopen()

            expect((await reopened.queryPluginStorage()).items.map((item) => item.key)).toEqual(
                expectedAfterReinsert,
            )
            expect(Object.keys(
                (await reopened.materializeDatabase(updated.revision)).pluginCustomStorage,
            )).toEqual(expectedAfterReinsert)

            const cleared = await reopened.commit({
                expectedRevision: updated.revision,
                pluginStorage: [
                    { type: 'clear' },
                    { type: 'set', key: 'zeta', value: 'fresh string' },
                    { type: 'set', key: '2', value: 2 },
                    { type: 'set', key: '1', value: 1 },
                ],
            })
            expect((await reopened.queryPluginStorage()).items.map((item) => item.key)).toEqual([
                '1',
                '2',
                'zeta',
            ])
            expect(Object.keys(
                (await reopened.materializeDatabase(cleared.revision)).pluginCustomStorage,
            )).toEqual(['1', '2', 'zeta'])
        })

        it('always materializes empty plugin storage and ignores incidental root fields', async () => {
            const { store } = await createHarness()
            expect((await store.materializeDatabase()).pluginCustomStorage).toEqual({})
            const database = structuredClone(fixtureDatabase)
            database.pluginCustomStorage = { retained: 0 }
            const imported = await store.replaceFromDatabase(database)
            const root = (await store.readRoot()).value
            await store.commit({
                expectedRevision: imported.revision,
                root: {
                    ...root,
                    pluginCustomStorage: { incidental: 'must not replace records' },
                } as typeof root,
            })

            expect((await store.materializeDatabase()).pluginCustomStorage).toEqual({ retained: 0 })
        })

        it('stores presets outside root and preserves configured ordering and exact values', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))

            expect((await store.readRoot()).value).not.toHaveProperty('botPresets')
            expect(await store.queryPresets()).toEqual({
                revision: imported.revision,
                items: [
                    { id: '0', name: 'Preset Beta', image: 'preset-beta.png', configuredIndex: 0 },
                    { id: '1', name: 'Preset Alpha', configuredIndex: 1 },
                ],
            })
            expect((await store.readPreset('1'))?.value).toEqual(fixtureDatabase.botPresets[1])
            expect(await store.readPreset('missing')).toBeNull()
            expect((await store.materializeDatabase()).botPresets).toEqual(fixtureDatabase.botPresets)
        })

        it('atomically replaces presets with root and leaves both unchanged after stale CAS', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const root = (await store.readRoot()).value
            const replacement = [
                { ...fixtureDatabase.botPresets[1], name: 'Replacement' },
            ] as Database['botPresets']

            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Preset commit' },
                replacePresets: replacement,
            })
            expect((await store.readRoot()).value.username).toBe('Preset commit')
            expect((await store.materializeDatabase()).botPresets).toEqual(replacement)

            await expect(
                store.commit({
                    expectedRevision: imported.revision,
                    root: { ...root, username: 'Stale root' },
                    replacePresets: fixtureDatabase.botPresets,
                }),
            ).rejects.toBeInstanceOf(RevisionConflictError)
            expect((await store.readRoot()).revision).toBe(committed.revision)
            expect((await store.readRoot()).value.username).toBe('Preset commit')
            expect((await store.materializeDatabase()).botPresets).toEqual(replacement)
        })

        it('isolates preset reads through a revision lease and rejects them after release', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const lease = await store.acquireRevision(imported.revision)
            await store.commit({
                expectedRevision: imported.revision,
                replacePresets: [{ ...fixtureDatabase.botPresets[0], name: 'New active preset' }],
            })

            expect((await lease.queryPresets()).items.map((item) => item.name)).toEqual([
                'Preset Beta',
                'Preset Alpha',
            ])
            expect((await lease.readPreset('0'))?.value.name).toBe('Preset Beta')
            await lease.release()
            await expect(lease.queryPresets()).rejects.toBeInstanceOf(SnapshotReleasedError)
        })

        it('queries the character catalog without hydrating conversations', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)

            expect(
                await store.queryCharacters({ order: 'configured', trash: false, limit: 2 }),
            ).toHaveProperty('revision', imported.revision)

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
                (await store.queryCharacters({ order: 'configured', trash: false, limit: 2 })).items[0],
            ).toMatchObject({ type: 'character', creatorNotes: '' })
            expect(
                (await store.queryCharacters({ order: 'configured', trash: true, limit: 10 })).items[0],
            ).toMatchObject({ type: 'character', creatorNotes: '', trashTime: 350 })
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

        it('returns the active revision with every conversation page, including an empty page', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)

            expect(
                await store.queryConversations({
                    characterId: 'char-a',
                    order: 'configured',
                    limit: 1,
                }),
            ).toHaveProperty('revision', imported.revision)
            expect(
                await store.queryConversations({
                    characterId: 'missing',
                    order: 'configured',
                    limit: 10,
                }),
            ).toEqual({ revision: imported.revision, items: [] })
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

        it('atomically deletes a character with batch group details and preserves plugin records', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const makeGroup = (id: string, members: string[], trashTime?: number) => ({
                type: 'group',
                chaId: id,
                name: id,
                characters: members,
                characterTalks: members.map((_member, index) => index + 0.25),
                characterActive: members.map((_member, index) => index % 2 === 0),
                chats: [],
                ...(trashTime === undefined ? {} : { trashTime }),
            }) as Database['characters'][number]
            const activeGroup = makeGroup('group-active', ['char-b', 'char-a'])
            activeGroup.chats = [{
                id: 'group-chat',
                name: 'Group chat',
                message: [{ role: 'user', data: 'keep me', chatId: 'group-message' }],
            } as Database['characters'][number]['chats'][number]]
            database.characters.push(
                activeGroup,
                makeGroup('group-trash', ['char-a', 'char-c'], 500),
                makeGroup('group-unreferenced', ['char-b']),
            )
            database.pluginCustomStorage = { zero: 0 }
            database.characterOrder = database.characters.map((character) => character.chaId)
            const imported = await store.replaceFromDatabase(database)
            const lease = await store.acquireRevision(imported.revision)
            const root = (await store.readRoot()).value
            const activeSummaryBefore = (await store.queryCharacters({
                order: 'configured',
                trash: false,
                limit: 100,
            })).items.find((item) => item.id === 'group-active')!
            const activeConversationBefore = await store.readConversation(
                'group-active',
                'group-chat',
            )
            const active = (await store.readCharacter('group-active'))!.value as groupChat
            const trash = (await store.readCharacter('group-trash'))!.value as groupChat
            active.characters = ['char-b']
            active.characterTalks = [0.25]
            active.characterActive = [true]
            trash.characters = ['char-c']
            trash.characterTalks = [1.25]
            trash.characterActive = [false]

            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: {
                    ...root,
                    characterOrder: root.characterOrder.filter((id) => id !== 'char-a'),
                },
                deleteCharacterId: 'char-a',
                characterDetails: [active, trash],
            })

            expect(committed.revision).toBe(imported.revision + 1)
            expect(await store.readCharacter('char-a')).toBeNull()
            expect(await store.readCharacter('group-active')).toMatchObject({
                revision: committed.revision,
                value: {
                    characters: ['char-b'],
                    characterTalks: [0.25],
                    characterActive: [true],
                },
            })
            expect((await store.queryCharacters({
                order: 'configured',
                trash: false,
                limit: 100,
            })).items.find((item) => item.id === 'group-active')).toEqual({
                ...activeSummaryBefore,
                conversationCount: 1,
            })
            expect(await store.readConversation('group-active', 'group-chat')).toEqual({
                ...activeConversationBefore,
                revision: committed.revision,
            })
            expect(await store.readCharacter('group-trash')).toMatchObject({
                revision: committed.revision,
                value: {
                    characters: ['char-c'],
                    characterTalks: [1.25],
                    characterActive: [false],
                },
            })
            expect((await store.readCharacter('group-unreferenced'))?.value).toMatchObject({
                characters: ['char-b'],
            })
            expect((await store.readPluginStorage('zero'))?.value).toBe(0)
            expect((await lease.readCharacter('char-a'))?.value.name).toBe('Alpha')
            expect((await lease.readCharacter('group-active'))?.value).toMatchObject({
                characters: ['char-b', 'char-a'],
                characterTalks: [0.25, 1.25],
                characterActive: [true, false],
            })
            expect((await lease.readPluginStorage('zero'))?.value).toBe(0)
            await lease.release()

            await expect(store.commit({
                expectedRevision: imported.revision,
                characterDetails: [active],
            })).rejects.toBeInstanceOf(RevisionConflictError)
            expect((await store.readRoot()).revision).toBe(committed.revision)
            expect((await store.readPluginStorage('zero'))?.value).toBe(0)
        })

        it.each(['empty', 'duplicate', 'deleted', 'missing'] as const)(
            'rejects %s IDs in batch details without changing any character rows',
            async (invalidCase) => {
                const { store } = await createHarness()
                const imported = await store.replaceFromDatabase(fixtureDatabase)
                const beforeDatabase = await store.materializeDatabase()
                const beforeCatalog = await store.queryCharacters({
                    order: 'configured',
                    trash: false,
                    limit: 100,
                })
                const beforeConversations = await store.queryConversations({
                    characterId: 'char-b',
                    order: 'configured',
                    limit: 100,
                })
                const detail = (await store.readCharacter('char-b'))!.value
                const invalidDetail = structuredClone(detail)
                let characterDetails = [invalidDetail]
                let deleteCharacterId: string | undefined
                if (invalidCase === 'empty') invalidDetail.chaId = ''
                if (invalidCase === 'duplicate') {
                    characterDetails = [invalidDetail, structuredClone(invalidDetail)]
                }
                if (invalidCase === 'deleted') deleteCharacterId = 'char-b'
                if (invalidCase === 'missing') invalidDetail.chaId = 'missing-character'

                await expect(store.commit({
                    expectedRevision: imported.revision,
                    root: { ...(await store.readRoot()).value, username: 'Must not persist' },
                    characterDetails,
                    deleteCharacterId,
                })).rejects.toThrow()

                expect((await store.readRoot()).revision).toBe(imported.revision)
                expect(await store.queryCharacters({
                    order: 'configured',
                    trash: false,
                    limit: 100,
                })).toEqual(beforeCatalog)
                expect(await store.queryConversations({
                    characterId: 'char-b',
                    order: 'configured',
                    limit: 100,
                })).toEqual(beforeConversations)
                expect(await store.materializeDatabase()).toEqual(beforeDatabase)
            },
        )

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

        it('rolls back root, deletion, and every batch detail when one detail cannot be stored', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.pluginCustomStorage = { zero: 0 }
            const group = {
                type: 'group',
                chaId: 'group-a',
                name: 'Group',
                characters: ['char-a', 'char-b'],
                characterTalks: [0.25, 0.75],
                characterActive: [true, false],
                chats: [],
            } as Database['characters'][number]
            database.characters.push(group)
            const imported = await store.replaceFromDatabase(database)
            const rootBefore = await store.readRoot()
            const groupBefore = (await store.readCharacter('group-a'))!.value
            const updatedGroup = structuredClone(groupBefore) as groupChat
            updatedGroup.characters = ['char-b']
            updatedGroup.characterTalks = [0.75]
            updatedGroup.characterActive = [false]

            await expect(store.commit({
                expectedRevision: imported.revision,
                root: { ...rootBefore.value, username: 'Must roll back' },
                deleteCharacterId: 'char-a',
                characterDetails: [
                    updatedGroup,
                    {
                        ...structuredClone(groupBefore),
                        chaId: 'char-b',
                        invalidFixtureValue: () => undefined,
                    } as unknown as typeof groupBefore,
                ],
            })).rejects.toThrow()

            expect(await store.readRoot()).toEqual(rootBefore)
            expect((await store.readCharacter('char-a'))?.value.name).toBe('Alpha')
            expect((await store.readCharacter('group-a'))?.value).toEqual(groupBefore)
            expect((await store.readPluginStorage('zero'))?.value).toBe(0)
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

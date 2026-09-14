import { describe, expect, it } from 'vitest'
import type { Database, groupChat } from '../database.svelte'
import type {
    AssetAlias,
    AssetOwnerHead,
    ColdAlias,
    PersistentDataStore,
} from '../persistentDataStore'
import { RevisionConflictError, SnapshotReleasedError } from '../persistentDataStore'
import { fixtureDatabase } from './persistentDataFixtures'

export interface PersistentDataStoreHarness {
    store: PersistentDataStore
    reopen(): Promise<PersistentDataStore>
}

export function persistentDataStoreContract(createHarness: () => Promise<PersistentDataStoreHarness>): void {
    describe('PersistentDataStore contract', () => {
        it('applies root deltas atomically and preserves leased roots', async () => {
            const { store, reopen } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const before = (await store.readRoot()).value
            const lease = await store.acquireRevision(imported.revision)
            const rootMutations = [
                { type: 'set' as const, key: 'username', value: 'Changed' },
                { type: 'set' as const, key: '__proto__', value: { safe: true } },
                { type: 'set' as const, key: 'optional', value: null },
                { type: 'delete' as const, key: 'missing' },
            ]
            const committed = await store.commit({
                expectedRevision: imported.revision,
                rootMutations,
            })
            const expected = {
                ...before,
                username: 'Changed',
                ['__proto__']: { safe: true },
                optional: null,
            }
            expect((await store.readRoot()).value).toEqual(expected)
            expect((await lease.readRoot()).value).toEqual(before)
            await expect(
                store.commit({ expectedRevision: imported.revision, rootMutations }),
            ).rejects.toBeInstanceOf(RevisionConflictError)
            for (const invalid of [
                { root: before, rootMutations: [] },
                { rootMutations: [{ type: 'delete' as const, key: 'characters' }] },
                { rootMutations: [{ type: 'delete' as const, key: 'botPresets' }] },
                { rootMutations: [{ type: 'delete' as const, key: 'pluginCustomStorage' }] },
                { rootMutations: [rootMutations[0], rootMutations[0]] },
            ]) {
                await expect(
                    store.commit({ expectedRevision: committed.revision, ...invalid }),
                ).rejects.toThrow()
                expect(await store.readRoot()).toEqual({
                    revision: committed.revision,
                    value: expected,
                })
            }
            await lease.release()
            expect((await (await reopen()).readRoot()).value).toEqual(expected)
        })

        it.each(['root', 'rootMutations'] as const)(
            'isolates occurrence-located owner heads across a pinned reorder with %s',
            async (mode) => {
                const { store } = await createHarness()
                const database = structuredClone(fixtureDatabase)
                database.modules = [
                    {
                        id: 'duplicate-module',
                        name: 'First duplicate',
                        description: '',
                        assets: [
                            ['first', 'assets/first.bin', 'BIN'],
                            ['first', 'assets/first.bin', 'BIN'],
                        ],
                    },
                    {
                        id: 'duplicate-module',
                        name: 'Second duplicate',
                        description: '',
                        assets: [],
                    },
                ]
                database.personas = [
                    {
                        name: 'Missing ID and absent assets',
                        personaPrompt: '',
                        icon: '',
                        embeddedModule: {
                            id: '',
                            name: 'Absent assets',
                            description: '',
                        },
                    },
                    {
                        name: 'Missing ID and present assets',
                        personaPrompt: '',
                        icon: '',
                        embeddedModule: {
                            id: '',
                            name: 'Present assets',
                            description: '',
                            assets: [['persona', 'assets/persona.bin', 'OddExt']],
                        },
                    },
                ]
                const imported = await store.replaceFromDatabase(database)
                const root = (await store.readRoot()).value
                const originalHeads: AssetOwnerHead[] = [
                    {
                        owner: { kind: 'root-module-assets', index: 0 },
                        present: true,
                        manifestHash: '11'.repeat(32),
                        entryCount: 2,
                    },
                    {
                        owner: { kind: 'root-module-assets', index: 1 },
                        present: true,
                        manifestHash: '22'.repeat(32),
                        entryCount: 0,
                    },
                    {
                        owner: { kind: 'persona-embedded-module-assets', index: 0 },
                        present: false,
                        manifestHash: null,
                        entryCount: 0,
                    },
                    {
                        owner: { kind: 'persona-embedded-module-assets', index: 1 },
                        present: true,
                        manifestHash: '33'.repeat(32),
                        entryCount: 1,
                    },
                ]
                const shadowed = await store.commit({
                    expectedRevision: imported.revision,
                    root,
                    assetOwnerHeads: originalHeads,
                })
                const lease = await store.acquireRevision(shadowed.revision)
                const reorderedRoot = structuredClone(root)
                reorderedRoot.modules.reverse()
                const reorderedHeads: AssetOwnerHead[] = [
                    {
                        owner: { kind: 'root-module-assets', index: 0 },
                        present: true,
                        manifestHash: '22'.repeat(32),
                        entryCount: 0,
                    },
                    {
                        owner: { kind: 'root-module-assets', index: 1 },
                        present: true,
                        manifestHash: '11'.repeat(32),
                        entryCount: 2,
                    },
                ]

                const reordered = await store.commit({
                    expectedRevision: shadowed.revision,
                    ...(mode === 'root'
                        ? { root: reorderedRoot }
                        : {
                              rootMutations: [
                                  {
                                      type: 'set' as const,
                                      key: 'modules',
                                      value: reorderedRoot.modules,
                                  },
                              ],
                          }),
                    assetOwnerHeads: reorderedHeads,
                })

                expect(
                    await store.readAssetOwnerHead({
                        kind: 'root-module-assets',
                        index: 0,
                    }),
                ).toEqual({ revision: reordered.revision, value: reorderedHeads[0] })
                expect(
                    await store.readAssetOwnerHead({
                        kind: 'persona-embedded-module-assets',
                        index: 0,
                    }),
                ).toEqual({ revision: reordered.revision, value: originalHeads[2] })
                expect(
                    await lease.readAssetOwnerHead({
                        kind: 'root-module-assets',
                        index: 0,
                    }),
                ).toEqual({ revision: shadowed.revision, value: originalHeads[0] })
                expect(
                    await lease.readAssetOwnerHead({
                        kind: 'persona-embedded-module-assets',
                        index: 0,
                    }),
                ).toEqual({ revision: shadowed.revision, value: originalHeads[2] })
                await lease.release()
            },
        )

        it('rejects stale or invalid owner-head commits without changing parent data', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [{
                id: 'module',
                name: 'Module',
                description: '',
                assets: [['kept', 'assets/kept.bin', 'BIN']],
            }]
            const imported = await store.replaceFromDatabase(database)
            const originalRoot = (await store.readRoot()).value
            const validHead: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '44'.repeat(32),
                entryCount: 1,
            }
            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: originalRoot,
                assetOwnerHeads: [validHead],
            })
            const invalidRoot = structuredClone(originalRoot)
            invalidRoot.username = 'must not commit'
            const invalidHead = {
                ...validHead,
                manifestHash: 'INVALID',
            } as unknown as AssetOwnerHead

            await expect(store.commit({
                expectedRevision: committed.revision,
                root: invalidRoot,
                assetOwnerHeads: [invalidHead],
            })).rejects.toThrow('manifestHash')
            await expect(store.commit({
                expectedRevision: imported.revision,
                root: invalidRoot,
                assetOwnerHeads: [validHead],
            })).rejects.toBeInstanceOf(RevisionConflictError)

            expect(await store.readRoot()).toEqual({
                revision: committed.revision,
                value: originalRoot,
            })
            expect(await store.readAssetOwnerHead(validHead.owner)).toEqual({
                revision: committed.revision,
                value: validHead,
            })
        })

        it('preserves a character shadow head when unrelated parent fields change', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.characters[0].additionalAssets = [
                ['duplicate', 'assets/duplicate.bin', 'BIN'],
                ['duplicate', 'assets/duplicate.bin', 'BIN'],
            ]
            const imported = await store.replaceFromDatabase(database)
            const detail = (await store.readCharacter(database.characters[0].chaId))!.value
            const head: AssetOwnerHead = {
                owner: {
                    kind: 'character-additional-assets',
                    characterId: database.characters[0].chaId,
                },
                present: true,
                manifestHash: '55'.repeat(32),
                entryCount: 2,
            }
            const shadowed = await store.commit({
                expectedRevision: imported.revision,
                character: detail,
                assetOwnerHeads: [head],
            })

            expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                revision: shadowed.revision,
                value: head,
            })
            const changed = await store.commit({
                expectedRevision: shadowed.revision,
                character: { ...detail, name: 'Changed through the legacy path' },
            })

            expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                revision: changed.revision,
                value: head,
            })
            expect((await store.readCharacter(database.characters[0].chaId))!.revision).toBe(
                changed.revision,
            )

            const invalidated = await store.commit({
                expectedRevision: changed.revision,
                character: {
                    ...detail,
                    additionalAssets: [['replacement', 'assets/replacement.bin', 'BIN']],
                },
            })
            expect(invalidated.revision).toBe(changed.revision + 1)
            expect(await store.readAssetOwnerHead(head.owner)).toBeNull()
        })

        it('preserves root owner heads when unrelated root fields change', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [{
                id: 'module',
                name: 'Module',
                description: '',
                assets: [['kept', 'assets/kept.bin', 'BIN']],
            }]
            const imported = await store.replaceFromDatabase(database)
            const root = (await store.readRoot()).value
            const head: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '56'.repeat(32),
                entryCount: 1,
            }
            const shadowed = await store.commit({
                expectedRevision: imported.revision,
                root,
                assetOwnerHeads: [head],
            })

            const changed = await store.commit({
                expectedRevision: shadowed.revision,
                root: { ...root, username: 'Unrelated root change' },
            })

            expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                revision: changed.revision,
                value: head,
            })

            const changedModules = structuredClone(root.modules!)
            changedModules[0].assets = [['replacement', 'assets/replacement.bin', 'BIN']]
            const invalidated = await store.commit({
                expectedRevision: changed.revision,
                root: { ...root, modules: changedModules },
            })
            expect(invalidated.revision).toBe(changed.revision + 1)
            expect(await store.readAssetOwnerHead(head.owner)).toBeNull()
        })

        it('moves omitted root owner heads with uniquely identified modules', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [
                {
                    id: 'module-a',
                    name: 'Module A',
                    description: '',
                    assets: [['a', 'assets/a.bin', 'BIN']],
                },
                {
                    id: 'module-b',
                    name: 'Module B',
                    description: '',
                    assets: [['b', 'assets/b.bin', 'BIN']],
                },
            ]
            const imported = await store.replaceFromDatabase(database)
            const root = (await store.readRoot()).value
            const heads: AssetOwnerHead[] = [
                {
                    owner: { kind: 'root-module-assets', index: 0 },
                    present: true,
                    manifestHash: '57'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: { kind: 'root-module-assets', index: 1 },
                    present: true,
                    manifestHash: '58'.repeat(32),
                    entryCount: 1,
                },
            ]
            const shadowed = await store.commit({
                expectedRevision: imported.revision,
                root,
                assetOwnerHeads: heads,
            })
            const reorderedRoot = structuredClone(root)
            reorderedRoot.modules!.reverse()

            const reordered = await store.commit({
                expectedRevision: shadowed.revision,
                root: reorderedRoot,
            })

            expect(await store.readAssetOwnerHead({
                kind: 'root-module-assets',
                index: 0,
            })).toEqual({
                revision: reordered.revision,
                value: { ...heads[1], owner: { kind: 'root-module-assets', index: 0 } },
            })
            expect(await store.readAssetOwnerHead({
                kind: 'root-module-assets',
                index: 1,
            })).toEqual({
                revision: reordered.revision,
                value: { ...heads[0], owner: { kind: 'root-module-assets', index: 1 } },
            })
        })

        it('validates a character owner head against the final same-ID parent atomically', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const imported = await store.replaceFromDatabase(database)
            const earlierDetail = (await store.readCharacter('char-a'))!.value
            earlierDetail.additionalAssets = [['earlier', 'assets/earlier.bin', 'BIN']]
            const finalCharacter = structuredClone(
                database.characters.find((character) => character.chaId === 'char-a')!,
            )
            finalCharacter.additionalAssets = [
                ['final-a', 'assets/final-a.bin', 'BIN'],
                ['final-b', 'assets/final-b.bin', 'OddExt'],
            ]
            const owner = {
                kind: 'character-additional-assets' as const,
                characterId: 'char-a',
            }
            const finalHead: AssetOwnerHead = {
                owner,
                present: true,
                manifestHash: '66'.repeat(32),
                entryCount: 2,
            }
            const committed = await store.commit({
                expectedRevision: imported.revision,
                characterDetails: [earlierDetail],
                replaceCharacter: finalCharacter,
                assetOwnerHeads: [finalHead],
            })

            const storedCharacter = await store.readCharacter('char-a')
            expect(storedCharacter?.value.additionalAssets).toEqual(
                finalCharacter.additionalAssets,
            )
            expect(await store.readAssetOwnerHead(owner)).toEqual({
                revision: committed.revision,
                value: finalHead,
            })

            const rootBefore = await store.readRoot()
            const earlierHead: AssetOwnerHead = {
                ...finalHead,
                manifestHash: '77'.repeat(32),
                entryCount: 1,
            }
            await expect(store.commit({
                expectedRevision: committed.revision,
                root: { ...rootBefore.value, username: 'must not commit' },
                characterDetails: [earlierDetail],
                replaceCharacter: finalCharacter,
                assetOwnerHeads: [earlierHead],
            })).rejects.toThrow('entryCount')

            expect(await store.readRoot()).toEqual(rootBefore)
            expect(await store.readCharacter('char-a')).toEqual(storedCharacter)
            expect(await store.readAssetOwnerHead(owner)).toEqual({
                revision: committed.revision,
                value: finalHead,
            })
        })

        it('isolates exact asset alias metadata across a pinned overwrite', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const original = {
                key: 'assets/shared.bin',
                objectHash: '11'.repeat(32),
                kind: 'asset' as const,
                size: 3,
                mime: 'application/octet-stream',
                name: 'Shared Original',
                ext: 'BIN',
            }
            const first = await store.commitAssetAlias(original, imported.revision)
            const lease = await store.acquireRevision(first.revision)
            const replacement = {
                key: original.key,
                objectHash: '22'.repeat(32),
                kind: 'asset' as const,
                size: 7,
                mime: 'application/octet-stream',
                name: 'Shared Replacement',
                ext: 'BIN',
            }

            const second = await store.commitAssetAlias(replacement, first.revision)

            expect(await store.readAssetAlias({ kind: original.kind, key: original.key })).toEqual({
                revision: second.revision,
                value: replacement,
            })
            expect(await lease.readAssetAlias({ kind: original.kind, key: original.key })).toEqual({
                revision: first.revision,
                value: original,
            })
            await lease.release()
        })

        it('reads found asset aliases by unique keys at the exact current and leased revisions', async () => {
            const { store } = await createHarness()
            const sharedKey = 'assets/batch-shared.bin'
            const asset: AssetAlias = {
                key: sharedKey,
                objectHash: '13'.repeat(32),
                kind: 'asset',
                size: 3,
                mime: 'application/octet-stream',
                name: 'Batch asset',
                ext: 'bin',
            }
            const inlay: AssetAlias = {
                key: sharedKey,
                objectHash: '14'.repeat(32),
                kind: 'inlay',
                size: 4,
                mime: 'image/webp',
                name: 'Batch inlay',
                ext: 'webp',
                inlayType: 'image',
            }
            const imported = await store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                undefined,
                [asset, inlay],
            )
            const lease = await store.acquireRevision(imported.revision)
            const replacement = { ...asset, objectHash: '15'.repeat(32), size: 5 }
            const committed = await store.commitAssetAlias(replacement, imported.revision)

            expect(await store.readAssetAliasesByKeys('asset', [sharedKey, 'assets/missing.bin']))
                .toEqual({ revision: committed.revision, value: [replacement] })
            expect(await store.readAssetAliasesByKeys('inlay', [sharedKey]))
                .toEqual({ revision: committed.revision, value: [inlay] })
            expect(await lease.readAssetAliasesByKeys('asset', [sharedKey, 'assets/missing.bin']))
                .toEqual({ revision: imported.revision, value: [asset] })
            await lease.release()
        })

        it('accepts exactly 512 asset alias keys', async () => {
            const { store } = await createHarness()
            const aliases: AssetAlias[] = Array.from({ length: 512 }, (_, index) => ({
                key: `assets/batch-${index}.bin`,
                objectHash: index.toString(16).padStart(64, '0'),
                kind: 'asset',
                size: index,
                mime: 'application/octet-stream',
                name: `Batch ${index}`,
                ext: 'bin',
            }))
            const imported = await store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                undefined,
                aliases,
            )

            const result = await store.readAssetAliasesByKeys(
                'asset',
                aliases.map((alias) => alias.key),
            )

            expect(result.revision).toBe(imported.revision)
            expect(new Map(result.value.map((alias) => [alias.key, alias]))).toEqual(
                new Map(aliases.map((alias) => [alias.key, alias])),
            )
        })

        it('rejects invalid asset alias key batches', async () => {
            const { store } = await createHarness()
            const keys = Array.from({ length: 513 }, (_, index) => `assets/${index}.bin`)

            await expect(store.readAssetAliasesByKeys('asset', [])).rejects.toThrow('between 1 and 512')
            await expect(store.readAssetAliasesByKeys('asset', ['duplicate', 'duplicate']))
                .rejects.toThrow('unique')
            await expect(store.readAssetAliasesByKeys('asset', keys)).rejects.toThrow('between 1 and 512')
            await expect(store.readAssetAliasesByKeys('invalid' as 'asset', ['assets/valid.bin']))
                .rejects.toThrow('kind')
            await expect(store.readAssetAliasesByKeys(
                'asset',
                ['assets/valid.bin', null] as unknown as string[],
            )).rejects.toThrow('string')
        })

        it('commits imported character and module aliases in one revision', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const root = structuredClone((await store.readRoot()).value)
            root.modules = [{
                id: 'native-module',
                name: 'Native module',
                description: '',
                assets: [['module asset', 'assets/native-module.bin', 'BIN']],
            }]
            const character = structuredClone(fixtureDatabase.characters[0])
            character.chaId = 'native-character'
            character.name = 'Native character'
            character.additionalAssets = [
                ['character asset', 'assets/native-character.bin', 'BIN'],
            ]
            const aliases: AssetAlias[] = [
                {
                    key: 'assets/native-character.bin',
                    objectHash: '91'.repeat(32),
                    kind: 'asset',
                    size: 4,
                    mime: 'application/octet-stream',
                    name: 'native-character.bin',
                    ext: 'BIN',
                },
                {
                    key: 'assets/native-module.bin',
                    objectHash: '92'.repeat(32),
                    kind: 'asset',
                    size: 5,
                    mime: 'application/octet-stream',
                    name: 'native-module.bin',
                    ext: 'BIN',
                },
            ]
            const ownerHeads: AssetOwnerHead[] = [
                {
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: character.chaId,
                    },
                    present: true,
                    manifestHash: '93'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: { kind: 'root-module-assets', index: 0 },
                    present: true,
                    manifestHash: '94'.repeat(32),
                    entryCount: 1,
                },
            ]

            const committed = await store.commit({
                expectedRevision: imported.revision,
                root,
                addCharacter: character,
                assetAliases: aliases,
                assetOwnerHeads: ownerHeads,
            })

            expect((await store.readCharacter(character.chaId))?.revision).toBe(committed.revision)
            expect((await store.readRoot()).revision).toBe(committed.revision)
            for (const alias of aliases) {
                expect(await store.readAssetAlias({ kind: alias.kind, key: alias.key })).toEqual({
                    revision: committed.revision,
                    value: alias,
                })
            }
            for (const head of ownerHeads) {
                expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                    revision: committed.revision,
                    value: head,
                })
            }
        })

        it('keeps colon-bearing character and conversation ids distinct', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const template = fixtureDatabase.characters[0]
            const first = structuredClone(template)
            first.chaId = 'colliding:character'
            first.name = 'Colliding One'
            first.chats = [{
                ...structuredClone(template.chats[0]),
                id: 'shared',
                name: 'First chat',
                message: [{ role: 'user', data: 'first payload', chatId: 'first-msg' }],
            }]
            const second = structuredClone(template)
            second.chaId = 'colliding'
            second.name = 'Colliding Two'
            second.chats = [{
                ...structuredClone(template.chats[0]),
                id: 'character:shared',
                name: 'Second chat',
                message: [{ role: 'user', data: 'second payload', chatId: 'second-msg' }],
            }]

            const one = await store.commit({
                expectedRevision: imported.revision,
                addCharacter: first,
            })
            const two = await store.commit({
                expectedRevision: one.revision,
                addCharacter: second,
            })

            expect((await store.readConversation('colliding:character', 'shared'))?.value.message)
                .toMatchObject([{ data: 'first payload' }])
            expect((await store.readConversation('colliding', 'character:shared'))?.value.message)
                .toMatchObject([{ data: 'second payload' }])

            await store.commit({
                expectedRevision: two.revision,
                conversations: [{
                    type: 'delete',
                    characterId: 'colliding',
                    conversationId: 'character:shared',
                }],
            })

            expect(await store.readConversation('colliding', 'character:shared')).toBeNull()
            expect((await store.readConversation('colliding:character', 'shared'))?.value.message)
                .toMatchObject([{ data: 'first payload' }])
        })

        it('rejects an imported alias batch without exposing its character or module', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const beforeRoot = await store.readRoot()
            const root = structuredClone(beforeRoot.value)
            root.modules = [{
                id: 'rejected-native-module',
                name: 'Rejected native module',
                description: '',
                assets: [['module asset', 'assets/rejected-module.bin', 'BIN']],
            }]
            const character = structuredClone(fixtureDatabase.characters[0])
            character.chaId = 'rejected-native-character'
            character.additionalAssets = [
                ['character asset', 'assets/rejected-character.bin', 'BIN'],
            ]
            const invalidAlias = {
                key: 'assets/rejected-character.bin',
                objectHash: 'INVALID',
                kind: 'asset',
                size: 4,
                mime: 'application/octet-stream',
                name: 'rejected-character.bin',
                ext: 'BIN',
            } as unknown as AssetAlias

            await expect(store.commit({
                expectedRevision: imported.revision,
                root,
                addCharacter: character,
                assetAliases: [invalidAlias],
            })).rejects.toThrow('objectHash')

            expect(await store.readRoot()).toEqual(beforeRoot)
            expect(await store.readCharacter(character.chaId)).toBeNull()
            expect(await store.readAssetAlias({
                kind: 'asset',
                key: invalidAlias.key,
            })).toBeNull()
        })

        it('keeps asset and inlay aliases with the same logical key independent', async () => {
            const { store } = await createHarness()
            const sharedKey = 'shared/same-key.bin'
            const asset = {
                key: sharedKey,
                objectHash: '31'.repeat(32),
                kind: 'asset' as const,
                size: 3,
                mime: 'application/octet-stream',
                name: 'Shared asset',
                ext: 'bin',
            }
            const inlay = {
                key: sharedKey,
                objectHash: '41'.repeat(32),
                kind: 'inlay' as const,
                size: 4,
                mime: 'image/webp',
                name: 'Shared inlay',
                ext: 'webp',
                inlayType: 'image' as const,
                width: 16,
                height: 9,
            }
            const imported = await store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                undefined,
                [asset, inlay],
            )
            const lease = await store.acquireRevision(imported.revision)
            const updatedAsset = {
                ...asset,
                objectHash: '51'.repeat(32),
                size: 5,
                name: 'Updated shared asset',
            }

            const committed = await store.commitAssetAlias(updatedAsset, imported.revision)

            expect(await store.readAssetAlias({ kind: 'asset', key: sharedKey })).toEqual({
                revision: committed.revision,
                value: updatedAsset,
            })
            expect(await store.readAssetAlias({ kind: 'inlay', key: sharedKey })).toEqual({
                revision: committed.revision,
                value: inlay,
            })
            expect(await lease.readAssetAlias({ kind: 'asset', key: sharedKey })).toEqual({
                revision: imported.revision,
                value: asset,
            })
            expect(await lease.readAssetAlias({ kind: 'inlay', key: sharedKey })).toEqual({
                revision: imported.revision,
                value: inlay,
            })
            await lease.release()
        })

        it('pages and deletes typed aliases without changing the pinned generation', async () => {
            const { store } = await createHarness()
            const sharedKey = 'shared/catalog-key'
            const asset: AssetAlias = {
                key: sharedKey,
                objectHash: '12'.repeat(32),
                kind: 'asset',
                size: 1,
                mime: 'application/octet-stream',
                name: 'Asset',
                ext: 'bin',
            }
            const inlay: AssetAlias = {
                key: sharedKey,
                objectHash: '34'.repeat(32),
                kind: 'inlay',
                size: 1,
                mime: 'audio/ogg',
                name: 'Inlay',
                ext: 'ogg',
                inlayType: 'audio',
            }
            const imported = await store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                undefined,
                [inlay, asset],
            )
            const lease = await store.acquireRevision(imported.revision)

            const first = await store.listAssetAliases({ limit: 1 })
            expect(first).toEqual({
                revision: imported.revision,
                items: [asset],
                nextCursor: expect.any(String),
            })
            expect(await store.listAssetAliases({
                limit: 1,
                cursor: first.nextCursor,
            })).toEqual({ revision: imported.revision, items: [inlay] })
            expect(await lease.listAssetAliases({ kind: 'asset', limit: 4 })).toEqual({
                revision: imported.revision,
                items: [asset],
            })

            const deleted = await store.deleteAssetAlias(
                { kind: 'asset', key: sharedKey },
                imported.revision,
            )
            expect(await store.readAssetAlias({ kind: 'asset', key: sharedKey })).toBeNull()
            expect(await store.readAssetAlias({ kind: 'inlay', key: sharedKey })).toEqual({
                revision: deleted.revision,
                value: inlay,
            })
            expect(await lease.readAssetAlias({ kind: 'asset', key: sharedKey })).toEqual({
                revision: imported.revision,
                value: asset,
            })
            await lease.release()
        })

        it('activates DB, aliases, owner heads, and v2 authority in one revision', async () => {
            const { store, reopen } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [{
                id: 'migration-module',
                name: 'Migration module',
                description: '',
                assets: [['asset', 'assets/migrated.bin', 'BIN']],
            }]
            const initial = await store.replaceFromDatabase(database)
            const lease = await store.acquireRevision(initial.revision)
            const alias: AssetAlias = {
                key: 'assets/migrated.bin',
                objectHash: '56'.repeat(32),
                kind: 'asset',
                size: 3,
                mime: 'application/octet-stream',
                name: 'Migrated',
                ext: 'BIN',
            }
            const head: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '78'.repeat(32),
                entryCount: 1,
            }

            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })
            const activated = await store.activateAssetRepositoryMigration({
                sourceRevision: initial.revision,
                migrationId: 'migration-atomic',
                compatibilityHash: '9a'.repeat(32),
                database,
                assetAliases: [alias],
                assetOwnerHeads: [head],
            })

            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: activated.revision,
                value: {
                    format: 'v2',
                    migrationId: 'migration-atomic',
                    compatibilityHash: '9a'.repeat(32),
                },
            })
            expect(await store.readAssetAlias({ kind: 'asset', key: alias.key })).toEqual({
                revision: activated.revision,
                value: alias,
            })
            expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                revision: activated.revision,
                value: head,
            })
            expect(await lease.readAssetRepositoryAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })
            expect(await lease.readAssetAlias({ kind: 'asset', key: alias.key })).toBeNull()
            await lease.release()

            const reopened = await reopen()
            expect(await reopened.readAssetRepositoryAuthority()).toEqual({
                revision: activated.revision,
                value: {
                    format: 'v2',
                    migrationId: 'migration-atomic',
                    compatibilityHash: '9a'.repeat(32),
                },
            })
        })

        it('rolls back a rejected or stale migration without exposing preparing authority', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const initial = await store.replaceFromDatabase(database)
            const invalidHead = {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: 'invalid',
                entryCount: 0,
            } as unknown as AssetOwnerHead

            await expect(store.activateAssetRepositoryMigration({
                sourceRevision: initial.revision,
                migrationId: 'migration-invalid',
                compatibilityHash: 'ab'.repeat(32),
                database,
                assetAliases: [],
                assetOwnerHeads: [invalidHead],
            })).rejects.toThrow('manifestHash')
            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })

            const changed = await store.commit({
                expectedRevision: initial.revision,
                root: { ...(await store.readRoot()).value, username: 'changed' },
            })
            await expect(store.activateAssetRepositoryMigration({
                sourceRevision: initial.revision,
                migrationId: 'migration-stale',
                compatibilityHash: 'cd'.repeat(32),
                database,
                assetAliases: [],
                assetOwnerHeads: [],
            })).rejects.toBeInstanceOf(RevisionConflictError)
            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: changed.revision,
                value: { format: 'legacy' },
            })
        })

        it('activates cold aliases and v2 authority in one revision', async () => {
            const { store, reopen } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const lease = await store.acquireRevision(initial.revision)
            const aliases: ColdAlias[] = [
                {
                    key: 'conversation/alpha',
                    objectHash: '31'.repeat(32),
                    size: 17,
                    metadata: { codec: 'gzip', nested: { version: 1 } },
                },
                {
                    key: 'conversation/empty',
                    objectHash: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
                    size: 0,
                    metadata: {},
                },
            ]

            expect(await store.readColdPayloadAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })
            const activated = await store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-atomic',
                compatibilityHash: '42'.repeat(32),
                coldAliases: aliases,
            })

            expect(await store.readColdPayloadAuthority()).toEqual({
                revision: activated.revision,
                value: {
                    format: 'v2',
                    migrationId: 'cold-atomic',
                    compatibilityHash: '42'.repeat(32),
                },
            })
            expect(await store.listColdAliases()).toEqual({
                revision: activated.revision,
                value: aliases,
            })
            expect(await store.readColdAlias('conversation/alpha')).toEqual({
                revision: activated.revision,
                value: aliases[0],
            })
            expect(await lease.readColdPayloadAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })
            expect(await lease.readColdAlias('conversation/alpha')).toBeNull()
            await lease.release()

            const reopened = await reopen()
            expect(await reopened.readColdPayloadAuthority()).toEqual({
                revision: activated.revision,
                value: {
                    format: 'v2',
                    migrationId: 'cold-atomic',
                    compatibilityHash: '42'.repeat(32),
                },
            })
            expect(await reopened.listColdAliases()).toEqual({
                revision: activated.revision,
                value: aliases,
            })
        })

        it('preserves v2 cold authority and aliases across a database-only replacement', async () => {
            const { store, reopen } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const alias: ColdAlias = {
                key: 'conversation/restored',
                objectHash: '49'.repeat(32),
                size: 23,
                metadata: { source: 'restored-before-database' },
            }
            const activated = await store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-restore-preserve',
                compatibilityHash: '4a'.repeat(32),
                coldAliases: [alias],
            })
            const replacement = structuredClone(fixtureDatabase)
            replacement.username = 'Restored after cold payloads'

            const replaced = await store.replaceFromDatabase(replacement, activated.revision)

            expect(await store.readColdPayloadAuthority()).toEqual({
                revision: replaced.revision,
                value: {
                    format: 'v2',
                    migrationId: 'cold-restore-preserve',
                    compatibilityHash: '4a'.repeat(32),
                },
            })
            expect(await store.listColdAliases()).toEqual({
                revision: replaced.revision,
                value: [alias],
            })
            const reopened = await reopen()
            expect(await reopened.readColdAlias(alias.key)).toEqual({
                revision: replaced.revision,
                value: alias,
            })
        })

        it('preserves repository aliases and only unchanged owner heads across a database-only replacement', async () => {
            const { store, reopen } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [
                {
                    id: 'kept-module',
                    name: 'Kept module',
                    description: '',
                    assets: [['kept', 'assets/kept.bin', 'BIN']],
                },
                {
                    id: 'changed-module',
                    name: 'Changed module',
                    description: '',
                    assets: [['old', 'assets/old.bin', 'BIN']],
                },
            ]
            database.characters[0].additionalAssets = [
                ['character', 'assets/character.bin', 'BIN'],
            ]
            database.personas = [{
                name: 'Absent module assets',
                personaPrompt: '',
                icon: '',
                embeddedModule: {
                    id: 'persona-module',
                    name: 'Persona module',
                    description: '',
                },
            }]
            const imported = await store.replaceFromDatabase(database)
            const aliases: AssetAlias[] = [
                {
                    key: 'assets/kept.bin',
                    objectHash: '61'.repeat(32),
                    kind: 'asset',
                    size: 4,
                    mime: 'application/octet-stream',
                    name: 'Kept',
                    ext: 'BIN',
                },
                {
                    key: 'assets/old.bin',
                    objectHash: '62'.repeat(32),
                    kind: 'asset',
                    size: 3,
                    mime: 'application/octet-stream',
                    name: 'Old',
                    ext: 'BIN',
                },
            ]
            const heads: AssetOwnerHead[] = [
                {
                    owner: { kind: 'root-module-assets', index: 0 },
                    present: true,
                    manifestHash: '63'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: { kind: 'root-module-assets', index: 1 },
                    present: true,
                    manifestHash: '64'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: database.characters[0].chaId,
                    },
                    present: true,
                    manifestHash: '68'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: { kind: 'persona-embedded-module-assets', index: 0 },
                    present: false,
                    manifestHash: null,
                    entryCount: 0,
                },
            ]
            const assetAuthority = {
                format: 'v2' as const,
                migrationId: 'asset-database-replacement',
                compatibilityHash: '65'.repeat(32),
            }
            const assetsActivated = await store.activateAssetRepositoryMigration({
                sourceRevision: imported.revision,
                migrationId: assetAuthority.migrationId,
                compatibilityHash: assetAuthority.compatibilityHash,
                database,
                assetAliases: aliases,
                assetOwnerHeads: heads,
            })
            const coldAlias: ColdAlias = {
                key: 'cold/database-replacement',
                objectHash: '66'.repeat(32),
                size: 6,
                metadata: { source: 'before-database-replacement' },
            }
            const coldAuthority = {
                format: 'v2' as const,
                migrationId: 'cold-database-replacement',
                compatibilityHash: '67'.repeat(32),
            }
            const coldActivated = await store.activateColdPayloadMigration({
                sourceRevision: assetsActivated.revision,
                migrationId: coldAuthority.migrationId,
                compatibilityHash: coldAuthority.compatibilityHash,
                coldAliases: [coldAlias],
            })
            const replacement = structuredClone(database)
            replacement.username = 'Database-only replacement'
            replacement.modules[1].assets = [['new', 'assets/new.bin', 'BIN']]
            replacement.personas[0].embeddedModule!.assets = []

            const replaced = await store.replaceFromDatabase(replacement, coldActivated.revision)

            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: replaced.revision,
                value: assetAuthority,
            })
            expect(await store.listAssetAliases({ limit: 8 })).toEqual({
                revision: replaced.revision,
                items: aliases,
            })
            const ownerExpectations = [
                { head: heads[0], retained: true },
                { head: heads[1], retained: false },
                { head: heads[2], retained: true },
                { head: heads[3], retained: false },
            ]
            for (const { head, retained } of ownerExpectations) {
                expect(await store.readAssetOwnerHead(head.owner)).toEqual(retained
                    ? { revision: replaced.revision, value: head }
                    : null)
            }
            expect(await store.readColdPayloadAuthority()).toEqual({
                revision: replaced.revision,
                value: coldAuthority,
            })
            expect(await store.listColdAliases()).toEqual({
                revision: replaced.revision,
                value: [coldAlias],
            })

            const reopened = await reopen()
            expect(await reopened.readAssetAlias({ kind: 'asset', key: aliases[0].key })).toEqual({
                revision: replaced.revision,
                value: aliases[0],
            })
            for (const { head, retained } of ownerExpectations) {
                expect(await reopened.readAssetOwnerHead(head.owner)).toEqual(retained
                    ? { revision: replaced.revision, value: head }
                    : null)
            }
            expect(await reopened.readColdAlias(coldAlias.key)).toEqual({
                revision: replaced.revision,
                value: coldAlias,
            })
        })

        it('preserves an owner head when nested object key insertion order changes', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [{
                id: 'semantic-module',
                name: 'Semantic module',
                description: '',
                assets: [[{
                    metadata: { first: 1, second: 2 },
                }]] as unknown as [string, string, string][],
            }]
            const imported = await store.replaceFromDatabase(database)
            const head: AssetOwnerHead = {
                owner: { kind: 'root-module-assets', index: 0 },
                present: true,
                manifestHash: '69'.repeat(32),
                entryCount: 1,
            }
            const activated = await store.activateAssetRepositoryMigration({
                sourceRevision: imported.revision,
                migrationId: 'asset-semantic-owner-tuple',
                compatibilityHash: '6a'.repeat(32),
                database,
                assetAliases: [],
                assetOwnerHeads: [head],
            })
            const replacement = structuredClone(database)
            replacement.modules[0].assets = [[{
                metadata: { second: 2, first: 1 },
            }]] as unknown as [string, string, string][]

            const replaced = await store.replaceFromDatabase(replacement, activated.revision)

            expect(await store.readAssetOwnerHead(head.owner)).toEqual({
                revision: replaced.revision,
                value: head,
            })
        })

        it('drops owner heads for malformed replacement parents without failing the replacement', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            database.modules = [{
                id: 'kept-module',
                name: 'Kept module',
                description: '',
                assets: [['kept', 'assets/kept.bin', 'BIN']],
            }]
            database.personas = [{
                name: 'Persona',
                personaPrompt: '',
                icon: '',
                embeddedModule: {
                    id: 'persona-module',
                    name: 'Persona module',
                    description: '',
                    assets: [['persona', 'assets/persona.bin', 'BIN']],
                },
            }]
            database.characters[0].additionalAssets = [['c', 'assets/c.bin', 'BIN']]
            const alias: AssetAlias = {
                key: 'assets/kept.bin',
                objectHash: '71'.repeat(32),
                kind: 'asset',
                size: 4,
                mime: 'application/octet-stream',
                name: 'Kept',
                ext: 'BIN',
            }
            const heads: AssetOwnerHead[] = [
                {
                    owner: { kind: 'root-module-assets', index: 0 },
                    present: true,
                    manifestHash: '72'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: { kind: 'persona-embedded-module-assets', index: 0 },
                    present: true,
                    manifestHash: '73'.repeat(32),
                    entryCount: 1,
                },
                {
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: database.characters[0].chaId,
                    },
                    present: true,
                    manifestHash: '74'.repeat(32),
                    entryCount: 1,
                },
            ]
            const authority = {
                format: 'v2' as const,
                migrationId: 'asset-malformed-parent',
                compatibilityHash: '75'.repeat(32),
            }
            const imported = await store.replaceFromDatabase(database)
            const activated = await store.activateAssetRepositoryMigration({
                sourceRevision: imported.revision,
                migrationId: authority.migrationId,
                compatibilityHash: authority.compatibilityHash,
                database,
                assetAliases: [alias],
                assetOwnerHeads: heads,
            })
            const replacement = structuredClone(database)
            replacement.username = 'Malformed replacement parents'
            replacement.modules = null as unknown as Database['modules']
            replacement.personas[0].embeddedModule = null as unknown as
                Database['personas'][number]['embeddedModule']
            replacement.characters[0].additionalAssets = null as unknown as
                Database['characters'][number]['additionalAssets']

            const replaced = await store.replaceFromDatabase(replacement, activated.revision)

            for (const head of heads) {
                expect(await store.readAssetOwnerHead(head.owner)).toBeNull()
            }
            expect(await store.readAssetAlias({ kind: 'asset', key: alias.key })).toEqual({
                revision: replaced.revision,
                value: alias,
            })
            expect(await store.readAssetRepositoryAuthority()).toEqual({
                revision: replaced.revision,
                value: authority,
            })
        })

        it('copy-on-write isolates cold alias commits and deletes from a pinned revision', async () => {
            const { store } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const original: ColdAlias[] = [
                {
                    key: 'conversation/alpha',
                    objectHash: '51'.repeat(32),
                    size: 10,
                    metadata: { codec: 'gzip' },
                },
                {
                    key: 'conversation/beta',
                    objectHash: '52'.repeat(32),
                    size: 20,
                    metadata: { codec: 'gzip' },
                },
            ]
            const activated = await store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-cow',
                compatibilityHash: '53'.repeat(32),
                coldAliases: original,
            })
            const lease = await store.acquireRevision(activated.revision)
            const replacement: ColdAlias = {
                key: original[0].key,
                objectHash: '54'.repeat(32),
                size: 30,
                metadata: { codec: 'gzip', arbitrary: ['kept', 2, true] },
            }

            const committed = await store.commitColdAlias(replacement, activated.revision)
            const deleted = await store.deleteColdAlias(original[1].key, committed.revision)

            expect(await store.listColdAliases()).toEqual({
                revision: deleted.revision,
                value: [replacement],
            })
            expect(await lease.listColdAliases()).toEqual({
                revision: activated.revision,
                value: original,
            })
            expect(await lease.readColdAlias(original[0].key)).toEqual({
                revision: activated.revision,
                value: original[0],
            })
            expect(await lease.readColdAlias(original[1].key)).toEqual({
                revision: activated.revision,
                value: original[1],
            })
            await lease.release()
        })

        it('rejects legacy cold mutations and invalid migration batches atomically', async () => {
            const { store } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const valid: ColdAlias = {
                key: 'conversation/valid',
                objectHash: '61'.repeat(32),
                size: 1,
                metadata: {},
            }

            await expect(store.commitColdAlias(valid, initial.revision)).rejects.toThrow('v2')
            await expect(store.deleteColdAlias(valid.key, initial.revision)).rejects.toThrow('v2')
            await expect(store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-null-payload',
                compatibilityHash: '62'.repeat(32),
                coldAliases: [{ ...valid, objectHash: null }],
            })).rejects.toThrow('objectHash')
            await expect(store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-duplicate',
                compatibilityHash: '63'.repeat(32),
                coldAliases: [valid, { ...valid, objectHash: '64'.repeat(32) }],
            })).rejects.toThrow('Duplicate cold alias')

            expect((await store.readRoot()).revision).toBe(initial.revision)
            expect(await store.readColdPayloadAuthority()).toEqual({
                revision: initial.revision,
                value: { format: 'legacy' },
            })
            expect(await store.listColdAliases()).toEqual({
                revision: initial.revision,
                value: [],
            })
        })

        it('rejects a null object hash in a normal v2 cold mutation', async () => {
            const { store } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const activated = await store.activateColdPayloadMigration({
                sourceRevision: initial.revision,
                migrationId: 'cold-no-legacy-fallback',
                compatibilityHash: '69'.repeat(32),
                coldAliases: [],
            })

            await expect(store.commitColdAlias({
                key: 'conversation/null-hash',
                objectHash: null,
                size: 0,
                metadata: {},
            }, activated.revision)).rejects.toThrow('objectHash')
            expect((await store.readRoot()).revision).toBe(activated.revision)
            expect((await store.listColdAliases()).value).toEqual([])
        })

        it('activates staged zero-byte and missing-payload aliases across reopen', async () => {
            const { store, reopen } = await createHarness()
            const initial = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const zeroByte = {
                key: 'assets/empty.bin',
                objectHash: 'e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855',
                kind: 'asset' as const,
                size: 0,
                mime: 'application/octet-stream',
                name: '',
                ext: '',
            }
            const missingPayload = {
                key: 'inlay/missing',
                objectHash: null,
                kind: 'inlay' as const,
                size: 0,
                mime: 'image/png',
                name: 'Missing payload',
                ext: 'PNG',
                inlayType: 'image' as const,
                width: 0,
                height: 0,
            }
            const duplicateBytesAlias = {
                ...zeroByte,
                key: 'assets/empty-copy.dat',
                name: 'Empty Copy',
                ext: 'DAT',
            }

            const replaced = await store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                initial.revision,
                [zeroByte, duplicateBytesAlias, missingPayload],
            )
            const reopened = await reopen()

            expect(await reopened.readAssetAlias({
                kind: 'asset',
                key: 'assets/not-present.bin',
            })).toBeNull()
            expect(await reopened.readAssetAlias({ kind: zeroByte.kind, key: zeroByte.key })).toEqual({
                revision: replaced.revision,
                value: zeroByte,
            })
            expect(await reopened.readAssetAlias({
                kind: missingPayload.kind,
                key: missingPayload.key,
            })).toEqual({
                revision: replaced.revision,
                value: missingPayload,
            })
            expect(await reopened.readAssetAlias({
                kind: duplicateBytesAlias.kind,
                key: duplicateBytesAlias.key,
            })).toEqual({
                revision: replaced.revision,
                value: duplicateBytesAlias,
            })
        })

        it('rejects an invalid alias batch without exposing a partial alias or revision', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const valid = {
                key: 'assets/prepared.bin',
                objectHash: '33'.repeat(32),
                kind: 'asset' as const,
                size: 1,
                mime: 'application/octet-stream',
                name: 'Prepared',
                ext: 'bin',
            }
            const invalid = {
                ...valid,
                key: 'assets/invalid.bin',
                objectHash: 'INVALID',
            }

            await expect(store.replaceFromDatabase(
                structuredClone(fixtureDatabase),
                imported.revision,
                [valid, invalid],
            )).rejects.toThrow('objectHash')

            expect((await store.readRoot()).revision).toBe(imported.revision)
            expect(await store.readAssetAlias({ kind: valid.kind, key: valid.key })).toBeNull()
            expect(await store.readAssetAlias({ kind: invalid.kind, key: invalid.key })).toBeNull()
        })

        it('rejects aliases whose metadata does not match their kind', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const base = {
                key: 'assets/discriminated.bin',
                objectHash: '44'.repeat(32),
                size: 1,
                mime: 'application/octet-stream',
                name: 'Discriminated',
                ext: 'bin',
            }
            const inlayWithoutType = { ...base, kind: 'inlay' as const }
            const assetWithInlayMetadata = {
                ...base,
                kind: 'asset' as const,
                inlayType: 'image' as const,
                width: 1,
                height: 1,
            }

            await expect(store.commitAssetAlias(
                inlayWithoutType as unknown as AssetAlias,
                imported.revision,
            )).rejects.toThrow('inlayType')
            await expect(store.commitAssetAlias(
                assetWithInlayMetadata as unknown as AssetAlias,
                imported.revision,
            )).rejects.toThrow('Inlay metadata')

            expect((await store.readRoot()).revision).toBe(imported.revision)
            expect(await store.readAssetAlias({ kind: 'asset', key: base.key })).toBeNull()
        })

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

        it('reads detached conversation metadata at the exact current and leased revisions', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const original = database.characters
                .find((character) => character.chaId === 'char-a')!
                .chats.find((conversation) => conversation.id === 'conv-short')!
            const originalMetadata = original as unknown as Record<
                string,
                unknown
            >
            originalMetadata.unknownMetadata = { nested: ['preserved'] }
            delete originalMetadata.folderId
            const imported = await store.replaceFromDatabase(database)
            const lease = await store.acquireRevision(imported.revision)
            const replacement = structuredClone(original)
            replacement.name = 'Changed after lease'
            replacement.message.push({
                role: 'user',
                data: 'later',
                chatId: 'msg-later',
            })
            const changed = await store.commit({
                expectedRevision: imported.revision,
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'conv-short',
                        start: 0,
                        deleteCount: original.message.length,
                        messages: replacement.message,
                        conversation: replacement,
                    },
                ],
            })

            const current = await store.readConversationMetadata(
                'char-a',
                'conv-short',
            )
            const pinned = await lease.readConversationMetadata(
                'char-a',
                'conv-short',
            )
            const {
                message: _currentMessages,
                ...expectedCurrentConversation
            } = replacement
            const { message: _pinnedMessages, ...expectedPinnedConversation } =
                original
            expect(current).toEqual({
                revision: changed.revision,
                value: {
                    characterId: 'char-a',
                    conversationId: 'conv-short',
                    conversation: expectedCurrentConversation,
                    totalMessages: 3,
                },
            })
            expect(pinned).toEqual({
                revision: imported.revision,
                value: {
                    characterId: 'char-a',
                    conversationId: 'conv-short',
                    conversation: expectedPinnedConversation,
                    totalMessages: 2,
                },
            })
            expect(Object.hasOwn(current!.value.conversation, 'message')).toBe(
                false,
            )
            expect(Object.hasOwn(current!.value.conversation, 'folderId')).toBe(
                false,
            )
            expect(
                await store.readConversationMetadata('char-b', 'conv-short'),
            ).toBeNull()
            expect(
                await store.readConversationMetadata('char-a', 'missing'),
            ).toBeNull()

            ;(
                current!.value.conversation as unknown as Record<
                    string,
                    unknown
                >
            ).name = 'mutated result'
            expect(
                (await store.readConversationMetadata('char-a', 'conv-short'))!
                    .value.conversation.name,
            ).toBe('Changed after lease')

            await lease.release()
            await expect(
                lease.readConversationMetadata('char-a', 'conv-short'),
            ).rejects.toBeInstanceOf(SnapshotReleasedError)
        })

        it('keeps the acquired revision exact through active commits and staged replacement', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const lease = await store.acquireRevision(imported.revision)
            const root = (await store.readRoot()).value
            const committed = await store.commit({
                expectedRevision: imported.revision,
                root: { ...root, username: 'Committed after lease' },
                deleteCharacterId: 'char-b',
                conversations: [
                    {
                        type: 'replace-range',
                        characterId: 'char-a',
                        conversationId: 'conv-short',
                        start: 2,
                        deleteCount: 0,
                        messages: [
                            { role: 'user', data: 'active append', chatId: 'active-append' },
                        ],
                    },
                ],
            })

            expect((await store.readRoot()).value.username).toBe('Committed after lease')
            expect(await store.readCharacter('char-b')).toBeNull()
            expect((await store.readConversation('char-a', 'conv-short'))?.value.message).toHaveLength(3)
            expect(await lease.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-short',
                startIndex: 1,
                limit: 2,
            })).toMatchObject({
                revision: imported.revision,
                value: {
                    startIndex: 1,
                    endIndex: 2,
                    totalMessages: 2,
                    messages: [{ chatId: 'msg-001' }],
                },
            })
            expect(await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-short',
                startIndex: 1,
                limit: 2,
            })).toMatchObject({
                revision: committed.revision,
                value: {
                    startIndex: 1,
                    endIndex: 3,
                    totalMessages: 3,
                    messages: [{ chatId: 'msg-001' }, { chatId: 'active-append' }],
                },
            })

            const replacement = structuredClone(fixtureDatabase)
            replacement.username = 'Staged after lease'
            await store.replaceFromDatabase(replacement, committed.revision)

            expect((await lease.readRoot()).value.username).toBe('Fixture User')
            expect((await lease.readCharacter('char-b'))?.value.name).toBe('Beta')
            expect((await lease.readConversation('char-a', 'conv-short'))?.value.message).toHaveLength(2)
            expect((await store.readRoot()).value.username).toBe('Staged after lease')

            await lease.release()
            await expect(lease.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
        })

        it('releases shared revision leases independently', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(structuredClone(fixtureDatabase))
            const first = await store.acquireRevision(imported.revision)
            const second = await store.acquireRevision(imported.revision)

            await store.commit({
                expectedRevision: imported.revision,
                root: { ...(await store.readRoot()).value, username: 'New active root' },
            })
            await first.release()

            await expect(first.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
            expect((await second.readRoot()).value.username).toBe('Fixture User')

            await second.release()
            await expect(second.readRoot()).rejects.toBeInstanceOf(SnapshotReleasedError)
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

        it('resolves first, last, and absent far duplicate anchors directly', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const character = database.characters.find((entry) => entry.chaId === 'char-a')!
            const conversation = character.chats.find((entry) => entry.id === 'conv-long')!
            conversation.message[1].chatId = 'far-duplicate'
            conversation.message[128].chatId = 'far-duplicate'
            await store.replaceFromDatabase(database)

            const first = await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'far-duplicate',
                before: 0,
                after: 0,
            })
            const last = await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'far-duplicate',
                anchorOccurrence: 'last',
                before: 0,
                after: 0,
            })
            const absent = await store.readConversationWindow({
                characterId: 'char-a',
                conversationId: 'conv-long',
                anchorMessageId: 'absent',
                anchorOccurrence: 'last',
                before: 0,
                after: 0,
            })

            expect(first?.value).toMatchObject({ startIndex: 1, endIndex: 2 })
            expect(last?.value).toMatchObject({ startIndex: 128, endIndex: 129 })
            expect(absent).toBeNull()
        })

        it('keeps anchor occurrences truthful across message ID and range mutations', async () => {
            const { store } = await createHarness()
            const database = structuredClone(fixtureDatabase)
            const character = database.characters.find((entry) => entry.chaId === 'char-a')!
            const conversation = character.chats.find((entry) => entry.id === 'conv-long')!
            conversation.message = [
                { role: 'user', data: 'first', chatId: 'duplicate' },
                { role: 'char', data: 'middle', chatId: 'middle' },
                { role: 'user', data: 'last', chatId: 'duplicate' },
            ]
            let revision = (await store.replaceFromDatabase(database)).revision
            const anchor = (messageId: string, anchorOccurrence: 'first' | 'last') =>
                store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    anchorMessageId: messageId,
                    anchorOccurrence,
                    before: 0,
                    after: 0,
                })

            expect((await anchor('duplicate', 'first'))?.value.startIndex).toBe(0)
            expect((await anchor('duplicate', 'last'))?.value.startIndex).toBe(2)

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 3,
                    deleteCount: 0,
                    messages: [{ role: 'char', data: 'appended', chatId: 'duplicate' }],
                }],
            })).revision
            expect((await anchor('duplicate', 'last'))?.value.startIndex).toBe(3)

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 3,
                    deleteCount: 1,
                    messages: [{ role: 'char', data: 'edited ID', chatId: 'edited' }],
                }],
            })).revision
            expect((await anchor('duplicate', 'last'))?.value.startIndex).toBe(2)
            expect((await anchor('edited', 'first'))?.value.startIndex).toBe(3)

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 0,
                    deleteCount: 1,
                    messages: [],
                }],
            })).revision
            expect((await anchor('duplicate', 'first'))?.value.startIndex).toBe(1)
            expect((await anchor('duplicate', 'last'))?.value.startIndex).toBe(1)
            expect((await anchor('edited', 'first'))?.value.startIndex).toBe(2)

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 1,
                    deleteCount: 2,
                    messages: [],
                }],
            })).revision
            expect(await anchor('duplicate', 'last')).toBeNull()
            expect(await anchor('edited', 'first')).toBeNull()

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 0,
                    deleteCount: 1,
                    messages: [
                        { role: 'user', data: 'replacement first', chatId: 'replacement' },
                        { role: 'char', data: 'replacement last', chatId: 'replacement' },
                    ],
                }],
            })).revision
            expect((await anchor('replacement', 'first'))?.value.startIndex).toBe(0)
            expect((await anchor('replacement', 'last'))?.value.startIndex).toBe(1)

            revision = (await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'delete',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                }],
            })).revision
            await store.commit({
                expectedRevision: revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'user', data: 'recreated', chatId: 'recreated' }],
                    conversation: {
                        id: 'conv-long',
                        name: 'Recreated',
                        note: '',
                        localLore: [],
                    },
                    configuredIndex: 0,
                }],
            })
            expect(await anchor('replacement', 'last')).toBeNull()
            expect((await anchor('recreated', 'first'))?.value.startIndex).toBe(0)
        })

        it('reads absolute conversation ranges with zero-based exclusive-end semantics', async () => {
            const { store } = await createHarness()
            await store.replaceFromDatabase(fixtureDatabase)

            const cases = [
                {
                    startIndex: 0,
                    limit: 3,
                    ids: ['msg-000', 'msg-001', 'msg-002'],
                    resultStart: 0,
                    resultEnd: 3,
                    hasMoreBefore: false,
                    hasMoreAfter: true,
                },
                {
                    startIndex: 126,
                    limit: 3,
                    ids: ['msg-126', 'msg-127', 'msg-128'],
                    resultStart: 126,
                    resultEnd: 129,
                    hasMoreBefore: true,
                    hasMoreAfter: true,
                },
                {
                    startIndex: 127,
                    limit: 1,
                    ids: ['msg-127'],
                    resultStart: 127,
                    resultEnd: 128,
                    hasMoreBefore: true,
                    hasMoreAfter: true,
                },
                {
                    startIndex: 128,
                    limit: 10,
                    ids: ['msg-128', 'msg-129'],
                    resultStart: 128,
                    resultEnd: 130,
                    hasMoreBefore: true,
                    hasMoreAfter: false,
                },
                {
                    startIndex: 130,
                    limit: 4,
                    ids: [],
                    resultStart: 130,
                    resultEnd: 130,
                    hasMoreBefore: true,
                    hasMoreAfter: false,
                },
                {
                    startIndex: 200,
                    limit: 4,
                    ids: [],
                    resultStart: 130,
                    resultEnd: 130,
                    hasMoreBefore: true,
                    hasMoreAfter: false,
                },
            ]

            for (const range of cases) {
                const result = await store.readConversationWindow({
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    startIndex: range.startIndex,
                    limit: range.limit,
                })
                expect(result?.value.messages.map((message) => message.chatId)).toEqual(range.ids)
                expect(result?.value).toMatchObject({
                    startIndex: range.resultStart,
                    endIndex: range.resultEnd,
                    totalMessages: 130,
                    hasMoreBefore: range.hasMoreBefore,
                    hasMoreAfter: range.hasMoreAfter,
                })
            }

            expect(
                await store.readConversationWindow({
                    characterId: 'missing',
                    conversationId: 'missing',
                    startIndex: 0,
                    limit: 1,
                }),
            ).toBeNull()
        })

        it('strictly validates only the absolute conversation range mode', async () => {
            const { store } = await createHarness()
            await store.replaceFromDatabase(fixtureDatabase)
            const base = {
                characterId: 'char-a',
                conversationId: 'conv-long',
            }

            for (const query of [
                { ...base, startIndex: -1, limit: 1 },
                { ...base, startIndex: 1.5, limit: 1 },
                { ...base, startIndex: Number.POSITIVE_INFINITY, limit: 1 },
                { ...base, startIndex: Number.MAX_SAFE_INTEGER + 1, limit: 1 },
                { ...base, startIndex: 0 },
                { ...base, startIndex: 0, limit: 0 },
                { ...base, startIndex: 0, limit: -1 },
                { ...base, startIndex: 0, limit: 1.5 },
                { ...base, startIndex: 0, limit: Number.NaN },
                { ...base, startIndex: 0, limit: 4_097 },
                { ...base, startIndex: 0, limit: 1, anchorMessageId: 'msg-000' },
            ]) {
                await expect(store.readConversationWindow(query)).rejects.toBeInstanceOf(RangeError)
            }

            await expect(store.readConversationWindow({ ...base, limit: -1 })).resolves.not.toThrow()
            await expect(
                store.readConversationWindow({
                    ...base,
                    anchorMessageId: 'msg-000',
                    before: -1,
                    after: -1,
                }),
            ).resolves.not.toThrow()
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

        it('creates a conversation at an explicit configured position without replacing siblings', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const source = fixtureDatabase.characters[1].chats[0]
            const { message: _sourceMessages, ...sourceDetail } = structuredClone(source)
            const conversation = {
                ...sourceDetail,
                id: 'conv-branch',
                name: 'Long chat (Branch)',
                note: 'branch detail',
                fmIndex: -1,
                unknownDetail: { keep: false },
            }
            const branchMessages = [
                { role: 'user' as const, data: 'duplicate first', chatId: 'duplicate' },
                { role: 'char' as const, data: 'missing ID' },
                { role: 'user' as const, data: 'duplicate second', chatId: 'duplicate' },
            ]

            await store.commit({
                expectedRevision: imported.revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-branch',
                    start: 0,
                    deleteCount: 0,
                    messages: branchMessages,
                    conversation,
                    configuredIndex: 0,
                }],
            })

            const summaries = (await store.queryConversations({
                characterId: 'char-a',
                order: 'configured',
                limit: 10,
            })).items
            expect(summaries.map(({ id, configuredIndex }) => ({ id, configuredIndex }))).toEqual([
                { id: 'conv-branch', configuredIndex: 0 },
                { id: 'conv-long', configuredIndex: 1 },
                { id: 'conv-short', configuredIndex: 2 },
            ])
            expect(summaries[0]).toHaveProperty('fmIndex', -1)
            expect(summaries[1]).not.toHaveProperty('fmIndex')
            const materialized = await store.materializeDatabase()
            expect(materialized.characters[1].chats.map((chat) => chat.id)).toEqual([
                'conv-branch',
                'conv-long',
                'conv-short',
            ])
            expect(materialized.characters[1].chats[0]).toEqual({
                ...conversation,
                message: branchMessages,
            })
        })

        it('rejects an explicit conversation insertion when the target ID already exists', async () => {
            const { store } = await createHarness()
            const imported = await store.replaceFromDatabase(fixtureDatabase)
            const before = await store.materializeDatabase()

            await expect(store.commit({
                expectedRevision: imported.revision,
                conversations: [{
                    type: 'replace-range',
                    characterId: 'char-a',
                    conversationId: 'conv-long',
                    start: 0,
                    deleteCount: 0,
                    messages: [{ role: 'user', data: 'must not insert' }],
                    conversation: {
                        id: 'conv-long',
                        name: 'Collision',
                        note: '',
                        localLore: [],
                    },
                    configuredIndex: 0,
                }],
            })).rejects.toThrow('already exists')
            expect(await store.materializeDatabase()).toEqual(before)
            expect((await store.readRoot()).revision).toBe(imported.revision)
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

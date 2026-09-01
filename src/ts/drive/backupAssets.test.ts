import { beforeEach, describe, expect, test, vi } from 'vitest'
import type { BlobStore } from '../storage/blobStore'
import { configureOfficialAccountAssetReader } from '../storage/accountAssetAccess'
import {
    collectBackupAssetKeys,
    collectExactPluginStorageAssetReferences,
    collectPinnedBackupAssetReferences,
    collectReferencedBackupInlays,
    createPinnedBackupReferenceAccumulator,
    decodeBackupInlayEntry,
    encodeBackupInlayEntry,
    getBackupInlayName,
    isLegacyBackupAssetKey,
    readBackupAsset,
    replaceExactPluginStorageAssetReferences,
    selectLegacyBackupAssetKeys,
    writeBackupAsset,
} from './backupAssets'

beforeEach(() => configureOfficialAccountAssetReader(null))

describe('legacy backup asset selection', () => {
    test('includes every nonempty assets descendant', () => {
        const keys = ['assets/a.png', 'assets/b.jpg', 'assets/c.mp3', 'assets/d.webm', 'assets/noext', 'assets\\windows.gif']
        expect(selectLegacyBackupAssetKeys(keys)).toEqual(keys)
    })

    test('excludes database, cold, BlobStore, and raw inlay keys', () => {
        for (const key of [
            'assets', 'database/database.bin', 'coldstorage/a', 'coldstorage_a',
            'blobstore/metadata/a.json', 'blobstore/inlays/a.bin', 'raw-inlay-id', 'backup/file',
        ]) expect(isLegacyBackupAssetKey(key)).toBe(false)
    })
})

describe('backup inlay entries', () => {
    const metadata = {
        key: 'inlay-1',
        kind: 'inlay',
        size: 3,
        mime: 'image/png',
        name: 'shot.png',
        ext: 'png',
        inlayType: 'image',
        width: 4,
        height: 5,
    } as const

    test('round trips payload and metadata through a single entry', () => {
        const data = new Uint8Array([1, 2, 3])
        const name = getBackupInlayName(metadata.key)

        expect(name).toBe('inlay_696e6c61792d31.risuinlay')
        expect(decodeBackupInlayEntry(name, encodeBackupInlayEntry(metadata, data))).toEqual({
            key: 'inlay-1',
            data,
            metadata: {
                kind: 'inlay',
                mime: 'image/png',
                name: 'shot.png',
                ext: 'png',
                inlayType: 'image',
                width: 4,
                height: 5,
            },
        })
    })

    test('round trips a zero byte payload', () => {
        const name = getBackupInlayName('empty')
        const decoded = decodeBackupInlayEntry(name, encodeBackupInlayEntry(
            { ...metadata, key: 'empty', size: 0 },
            new Uint8Array(),
        ))

        expect(decoded?.data).toEqual(new Uint8Array())
    })

    test('leaves entries it does not own to the asset and cold branches', () => {
        const entry = encodeBackupInlayEntry(metadata, new Uint8Array([1]))

        expect(decodeBackupInlayEntry('profile.png', entry)).toBeNull()
        expect(decodeBackupInlayEntry('coldstorage_a.json', entry)).toBeNull()
        expect(decodeBackupInlayEntry('inlay_zz.risuinlay', entry)).toBeNull()
    })

    test('rejects damaged, mistyped, and asset shadowing entries', () => {
        const name = getBackupInlayName(metadata.key)
        const entry = encodeBackupInlayEntry(metadata, new Uint8Array([1]))

        expect(decodeBackupInlayEntry(name, entry.subarray(0, 3))).toBeNull()
        expect(decodeBackupInlayEntry(name, entry.subarray(0, 6))).toBeNull()
        expect(decodeBackupInlayEntry(name, new Uint8Array([9, 0, 0, 0, 1, 2]))).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, kind: 'asset' } as never, new Uint8Array([1])),
        )).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, inlayType: 'model' } as never, new Uint8Array([1])),
        )).toBeNull()
        expect(decodeBackupInlayEntry(
            name,
            encodeBackupInlayEntry({ ...metadata, key: 'assets/shadow.png' }, new Uint8Array([1])),
        )).toBeNull()
        for (const header of ['null', '[]', '"text"', '{"key":"a","kind":"inlay","inlayType":"image"}']) {
            const payload = new TextEncoder().encode(header)
            const forged = new Uint8Array(4 + payload.byteLength)
            new DataView(forged.buffer).setUint32(0, payload.byteLength, true)
            forged.set(payload, 4)
            expect(decodeBackupInlayEntry(name, forged)).toBeNull()
        }
    })

    test('keeps only inlays referenced by the pinned logical snapshot', async () => {
        const list = vi.fn(async () => [
            { ...metadata, key: 'pinned-id' },
            { ...metadata, key: 'created-after-pin' },
        ])
        const store = { list } as unknown as BlobStore

        await expect(collectReferencedBackupInlays(store, ['pinned-id'])).resolves.toEqual([
            { ...metadata, key: 'pinned-id' },
        ])
    })
})

describe('plugin storage asset references', () => {
    test('collects only whole flat paths recursively and normalizes separators', () => {
        const cyclic: Record<string, unknown> = {
            direct: 'assets/direct.webp',
            nested: [{ audio: 'assets\\voice.mp3' }],
            collisionA: 'assets/a/x.bin',
            collisionB: 'assets/b/x.bin',
            prose: 'prefix assets/not-a-reference.png',
            serialized: '{"path":"assets/not-json.png"}',
        }
        cyclic.self = cyclic

        expect(collectExactPluginStorageAssetReferences(cyclic)).toEqual([
            'assets/direct.webp',
            'assets/voice.mp3',
        ])
    })

    test('projects exact values once without changing keys, source, or own proto data', () => {
        const source = JSON.parse(
            '{"direct":"assets/direct.webp",' +
            '"nested":["assets/chain.bin","prefix assets/direct.webp"],' +
            '"windows":"assets\\\\windows.bin",' +
            '"nestedPath":"assets/a/x.bin",' +
            '"assets/direct.webp":"object-key-is-not-a-reference",' +
            '"__proto__":"assets/proto.bin"}',
        ) as Record<string, unknown>
        const projected = replaceExactPluginStorageAssetReferences(source, {
            'assets/direct.webp': 'remote/direct.webp',
            'assets/chain.bin': 'assets/chain-step.bin',
            'assets/chain-step.bin': 'remote/chain-final.bin',
            'assets/windows.bin': 'remote/windows.bin',
            'assets/a/x.bin': 'remote/a/x.bin',
            'assets/proto.bin': 'remote/proto.bin',
        })

        expect(projected).not.toBe(source)
        expect(projected.direct).toBe('remote/direct.webp')
        expect(projected.nested).toEqual([
            'assets/chain-step.bin',
            'prefix assets/direct.webp',
        ])
        expect(projected['assets/direct.webp']).toBe('object-key-is-not-a-reference')
        expect(projected.windows).toBe('remote/windows.bin')
        expect(projected.nestedPath).toBe('remote/a/x.bin')
        expect(Object.hasOwn(projected, '__proto__')).toBe(true)
        expect(projected.__proto__).toBe('remote/proto.bin')
        expect(Object.getPrototypeOf(projected)).toBe(Object.prototype)
        expect(source.direct).toBe('assets/direct.webp')
        expect((source.nested as unknown[])[0]).toBe('assets/chain.bin')
        expect(source.__proto__).toBe('assets/proto.bin')
    })
})

describe('pinned backup reference accumulator', () => {
    test('collects full references record by record and lets cold characters override stubs', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        accumulator.visitRoot({
            customBackground: 'assets/root.png',
            modules: [],
            personas: [],
            characterOrder: [],
            note: '{{inlay::root-inlay}}',
        } as never)
        accumulator.visitPreset({ id: 'preset', name: 'Preset', image: 'assets/preset.png' } as never)
        accumulator.visitCharacter({
            summary: {
                id: 'char-1', name: 'Character', image: 'assets/stub.png', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 1, type: 'character',
            },
            detail: {
                chaId: 'char-1', name: 'Character', type: 'character', image: 'assets/stub.png',
                additionalAssets: [['stub-extra', 'assets/stub-extra.png', 'png']],
                coldstorage: 'cold-character', coldStoragedChats: ['cold-old-chat'],
            } as never,
        })
        accumulator.visitConversation({
            summary: {
                id: 'chat-1', characterId: 'char-1', name: 'Chat', configuredIndex: 0,
                recentAt: 0, messageCount: 2,
            },
            value: {
                id: 'chat-1', name: 'Chat',
                message: [
                    { role: 'user', data: '\uEF01COLDSTORAGE\uEF01cold-chat', time: 1 },
                    { role: 'char', data: '{{inlayed::chat-inlay}}', time: 2 },
                ],
            } as never,
        })
        accumulator.visitPluginStorage({
            direct: 'assets/plugin.webp',
            nested: [{ audio: 'assets\\plugin-audio.mp3' }],
            prose: 'prefix assets/not-a-reference.png',
            markup: '{{inlayeddata::plugin-inlay}}',
        })
        accumulator.visitColdPayload({
            character: {
                chaId: 'char-1', name: 'Character', type: 'character', chats: [],
                image: 'assets/cold.png',
                additionalAssets: [['cold-extra', 'assets/cold-extra.png', 'png']],
            },
            text: '{{inlay::cold-inlay}}',
        })

        const result = accumulator.finish()

        expect(result.assetKeys).toEqual([
            'assets/cold-extra.png',
            'assets/cold.png',
            'assets/plugin-audio.mp3',
            'assets/plugin.webp',
            'assets/root.png',
        ])
        expect(result.inlayKeys).toEqual([
            'chat-inlay',
            'cold-inlay',
            'plugin-inlay',
            'root-inlay',
        ])
        expect(result.coldKeys).toEqual(['cold-character', 'cold-old-chat', 'cold-chat'])
        expect(result.assetLabels.has('assets/cold.png')).toBe(false)
        expect(result.coldCharacterReferences).toEqual([{
            characterId: 'char-1',
            characterName: 'Character',
            keys: ['cold-character', 'cold-old-chat', 'cold-chat'],
        }])
    })

    test('keeps partial backup selection limited to documented profile assets', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('partial')
        accumulator.visitRoot({
            userIcon: 'assets/user.png',
            customBackground: 'assets/background.png',
            personas: [{ name: 'Persona', icon: 'assets/persona.png' }],
            characterOrder: [{
                name: 'Folder', img: 'assets/folder-img.png', imgFile: 'assets/folder-file.png',
            }],
        } as never)
        accumulator.visitPreset({ id: 'preset', name: 'Preset', image: 'assets/preset.png' } as never)
        accumulator.visitCharacter({
            summary: {
                id: 'char-1', name: 'Character', image: 'assets/profile.png', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 0, type: 'character',
            },
            detail: {
                chaId: 'char-1', name: 'Character', type: 'character', image: 'assets/profile.png',
                additionalAssets: [['bulk', 'assets/bulk.png', 'png']],
            } as never,
        })
        accumulator.visitPluginStorage({
            asset: 'assets/plugin-partial.bin',
            inlay: '{{inlay::plugin-partial-inlay}}',
        })

        const result = accumulator.finish()

        expect(result.assetKeys).toEqual([
            'assets/profile.png',
            'assets/user.png',
            'assets/persona.png',
            'assets/background.png',
            'assets/folder-img.png',
            'assets/folder-file.png',
            'assets/preset.png',
        ])
        expect(result.inlayKeys).toEqual([])
        expect(result.assetLabels.get('assets/profile.png')?.assetName).toBe('Profile Image')
    })

    test('ignores an empty top-level coldstorage field', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        accumulator.visitCharacter({
            summary: {
                id: 'char-empty', name: 'Empty marker', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 0, type: 'character',
            },
            detail: {
                chaId: 'char-empty', name: 'Empty marker', type: 'character',
                coldstorage: '',
            } as never,
        })

        expect(accumulator.finish()).toMatchObject({
            coldKeys: [],
            coldCharacterReferences: [],
        })
    })

    test('preserves an empty cold key from the legacy coldStoragedChats array', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        accumulator.visitCharacter({
            summary: {
                id: 'char-empty', name: 'Empty marker', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 0, type: 'character',
            },
            detail: {
                chaId: 'char-empty', name: 'Empty marker', type: 'character',
                coldStoragedChats: [''],
            } as never,
        })

        expect(accumulator.finish()).toMatchObject({
            coldKeys: [''],
            coldCharacterReferences: [{
                characterId: 'char-empty',
                characterName: 'Empty marker',
                keys: [''],
            }],
        })
    })

    test('preserves an empty cold key from a header-only conversation marker', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        accumulator.visitCharacter({
            summary: {
                id: 'char-empty', name: 'Empty marker', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 1, type: 'character',
            },
            detail: {
                chaId: 'char-empty', name: 'Empty marker', type: 'character',
            } as never,
        })
        accumulator.visitConversation({
            summary: {
                id: 'chat-empty', characterId: 'char-empty', name: 'Chat', configuredIndex: 0,
                recentAt: 0, messageCount: 1,
            },
            value: {
                id: 'chat-empty', name: 'Chat',
                message: [{ role: 'user', data: '\uEF01COLDSTORAGE\uEF01', time: 1 }],
            } as never,
        })

        expect(accumulator.finish()).toMatchObject({
            coldKeys: [''],
            coldCharacterReferences: [{
                characterId: 'char-empty',
                characterName: 'Empty marker',
                keys: [''],
            }],
        })
    })

    test('retains no character entry for records without assets or cold keys', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        for (let index = 0; index < 512; index += 1) {
            accumulator.visitCharacter({
                summary: {
                    id: `empty-${index}`, name: `Empty ${index}`, configuredIndex: index,
                    recentAt: 0, trashed: false, conversationCount: 0, type: 'character',
                },
                detail: {
                    chaId: `empty-${index}`, name: `Empty ${index}`, type: 'character',
                } as never,
            })
        }

        expect(accumulator.finish()).toMatchObject({
            assetKeys: [],
            coldKeys: [],
            coldCharacterReferences: [],
        })
    })

    test('removes a stub asset set when a cold character overrides it with no assets', () => {
        const accumulator = createPinnedBackupReferenceAccumulator('full')
        accumulator.visitCharacter({
            summary: {
                id: 'cold-empty', name: 'Cold empty', configuredIndex: 0,
                recentAt: 0, trashed: false, conversationCount: 0, type: 'character',
            },
            detail: {
                chaId: 'cold-empty', name: 'Cold empty', type: 'character',
                image: 'assets/stub.png',
            } as never,
        })
        accumulator.visitColdPayload({
            character: {
                chaId: 'cold-empty', name: 'Cold empty', type: 'character', chats: [],
            },
        })

        expect(accumulator.finish().assetKeys).toEqual([])
    })
})

describe('account backup asset I/O', () => {
    test('derives asset references from one database and cold-payload snapshot', () => {
        const database = {
            customBackground: 'assets/root.png',
            characters: [{
                chaId: 'cold-char', type: 'character', image: 'assets/stub.png', chats: [],
            }],
        }
        const coldPayloads = [{
            character: {
                chaId: 'cold-char', type: 'character', image: 'assets/cold.png', chats: [],
                additionalAssets: [['prop', 'assets/cold-prop.png', 'png']],
            },
        }]

        expect(collectPinnedBackupAssetReferences(
            database as never,
            coldPayloads,
        )).toEqual([
            'assets/cold-prop.png',
            'assets/cold.png',
            'assets/root.png',
        ])
    })

    test('does not add unreferenced live BlobStore payloads to a pinned backup', async () => {
        const list = vi.fn(async () => [
            { key: 'assets/local.png', kind: 'asset' },
            { key: 'sync-conflict-backups/backup-id.risudat', kind: 'asset' },
        ])
        const store = { list } as unknown as BlobStore

        await expect(collectBackupAssetKeys(store, [])).resolves.toEqual([])
        expect(list).not.toHaveBeenCalled()
    })

    test('combines active-root assets with remote-only database references', async () => {
        const localBytes = new Uint8Array([1])
        const read = vi.fn(async (key: string) => key === 'assets/local.png' ? localBytes : null)
        const list = vi.fn(async () => [{ key: 'assets/local.png', kind: 'asset' }])
        const store = { read, list } as unknown as BlobStore
        const readRemote = vi.fn(async () => new Uint8Array([9]))
        configureOfficialAccountAssetReader(readRemote)

        const keys = await collectBackupAssetKeys(store, [
            'assets/local.png',
            'assets/remote.png',
            'https://example.invalid/not-an-asset.png',
        ])

        expect(keys).toEqual(['assets/local.png', 'assets/remote.png'])
        await expect(readBackupAsset(store, 'assets/local.png', true)).resolves.toEqual(localBytes)
        await expect(readBackupAsset(store, 'assets/remote.png', true)).resolves.toEqual(
            new Uint8Array([9]),
        )
        expect(readRemote).toHaveBeenCalledOnce()
        expect(readRemote).toHaveBeenCalledWith('assets/remote.png')
    })

    test('keeps pinned references stable when a live orphan appears after pinning', async () => {
        const list = vi.fn(async () => [
            { key: 'assets/pinned.png', kind: 'asset' },
            { key: 'assets/created-after-pin.png', kind: 'asset' },
        ])
        const store = { list } as unknown as BlobStore

        const keys = await collectBackupAssetKeys(store, [
            'assets/pinned.png',
            'assets/remote-only.png',
        ])

        expect(keys).toEqual(['assets/pinned.png', 'assets/remote-only.png'])
        expect(list).not.toHaveBeenCalled()
    })

    test('restores through BlobStore metadata on a Tauri-shaped backend', async () => {
        const put = vi.fn(async () => undefined)
        const store = { put } as unknown as BlobStore
        const bytes = new Uint8Array([4, 5])

        await writeBackupAsset(store, 'assets/restored.webp', bytes)

        expect(put).toHaveBeenCalledWith('assets/restored.webp', bytes, {
            kind: 'asset',
            mime: '',
            name: 'restored.webp',
            ext: 'webp',
        })
    })

    test('preserves an ordinary asset hash across restore and export', async () => {
        const values = new Map<string, Uint8Array>()
        const store = {
            put: vi.fn(async (key: string, bytes: Uint8Array) => {
                values.set(key, bytes.slice())
            }),
            read: vi.fn(async (key: string) => values.get(key)?.slice() ?? null),
        } as unknown as BlobStore
        const source = Uint8Array.of(0, 255, 1, 2, 3, 128)

        await writeBackupAsset(store, 'assets/original.bin', source)
        const exported = await readBackupAsset(store, 'assets/original.bin', false)
        const digest = await crypto.subtle.digest('SHA-256', exported!.slice().buffer as ArrayBuffer)
        const hash = Array.from(new Uint8Array(digest), (byte) => byte.toString(16).padStart(2, '0')).join('')

        expect(exported).toEqual(source)
        expect(hash).toBe('e8e7f00d2b9a028c7a8ad275f02fe8206dd1f654446e88b305bf06f8adfc67f1')
    })
})

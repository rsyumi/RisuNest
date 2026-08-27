import { describe, expect, it, vi } from 'vitest'

vi.mock('../characterCards', () => ({
    decodePreparedNativePngCharacterCard: vi.fn(),
    mapPreparedNativeCharacterCard: vi.fn(),
}))
vi.mock('./nativeAssetRepository', () => ({
    createNativeImmutablePayloadCas: vi.fn(),
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    upsertPersistentCompleteCharacter: vi.fn(),
}))

import type { character } from './database.svelte'
import type { PreparedImmutablePayload } from './payloadCas'
import {
    activatePreparedNativeCharacterContent,
    UnsupportedPreparedNativeCharacterCardError,
    type NativeCharacterContentActivationDependencies,
} from './nativeCharacterContentActivation'
import type {
    PreparedNativeContent,
    PreparedNativeContentActivationLifecycle,
} from './nativeFileJobs'
import { decodeOwnerManifest, ownerManifestIdentity } from './ownerManifestCodec'
import { runNativePreparedContentRoute } from './nativePreparedContentRoute'

const firstHash = '11'.repeat(32)
const secondHash = '22'.repeat(32)

const content: PreparedNativeContent = {
    casSessionId: 'content-1',
    format: 'json-card',
    metadata: {
        spec: 'chara_card_v3',
        spec_version: '3.0',
        data: { name: 'Native Card', extensions: {} },
    },
    assets: [
        {
            referenceKey: 'data.assets.0.uri',
            token: 'native-data-0',
            logicalId: `assets/${firstHash}.png`,
            objectHash: firstHash,
            byteSize: 4,
            mime: 'image/png',
            name: 'portrait',
            ext: 'png',
        },
        {
            referenceKey: 'data.assets.1.uri',
            token: 'native-data-1',
            logicalId: `assets/${secondHash}.json`,
            objectHash: secondHash,
            byteSize: 9,
            mime: 'application/json',
            name: 'config',
            ext: 'json',
        },
        {
            referenceKey: 'data.assets.2.uri',
            token: 'native-data-2',
            logicalId: `assets/${secondHash}.json`,
            objectHash: secondHash,
            byteSize: 9,
            mime: 'application/json',
            name: 'config',
            ext: 'json',
        },
    ],
}

function mappedCharacter(): character {
    return {
        type: 'character',
        chaId: 'character-1',
        name: 'Native Card',
        chats: [],
        additionalAssets: [
            ['config', `assets/${secondHash}.json`, 'json'],
            ['config duplicate', `assets/${secondHash}.json`, 'json'],
        ],
    } as character
}

function dependencies(
    overrides: Partial<NativeCharacterContentActivationDependencies> = {},
): NativeCharacterContentActivationDependencies {
    return {
        decodePng: vi.fn(async () => content.metadata as any),
        map: vi.fn(async () => mappedCharacter()),
        upsert: vi.fn(async () => true),
        ...overrides,
    }
}

function lifecycle(
    overrides: Partial<PreparedNativeContentActivationLifecycle> = {},
): PreparedNativeContentActivationLifecycle {
    return {
        prepareOwnerManifestAndSeal: vi.fn(async (bytes): Promise<PreparedImmutablePayload> => ({
            contentHash: await ownerManifestIdentity(bytes),
            byteSize: bytes.byteLength,
            physicalKey: 'unused-by-activation',
            deduplicated: false,
        })),
        ...overrides,
    }
}

function deferred<T>(): {
    promise: Promise<T>
    resolve(value?: T): void
} {
    let resolve!: (value: T) => void
    const promise = new Promise<T>((done) => {
        resolve = done
    })
    return { promise, resolve: (value) => resolve(value as T) }
}

describe('prepared native character content activation', () => {
    it('seals the owner manifest session before the atomic character upsert', async () => {
        const events: string[] = []
        const deps = dependencies({
            upsert: vi.fn(async () => {
                events.push('upsert')
                return true
            }),
        })
        const session = lifecycle({
            prepareOwnerManifestAndSeal: vi.fn(async (bytes) => {
                events.push('finalize')
                return {
                    contentHash: await ownerManifestIdentity(bytes),
                    byteSize: bytes.byteLength,
                    physicalKey: 'unused-by-activation',
                    deduplicated: false,
                }
            }),
        })

        await activatePreparedNativeCharacterContent(content, session, deps)

        expect(events).toEqual(['finalize', 'upsert'])
    })

    it('commits ordinary aliases and the exact ordered owner manifest with the character', async () => {
        const deps = dependencies()
        const session = lifecycle()

        const result = await activatePreparedNativeCharacterContent(content, session, deps)

        expect(result).toEqual({ characterId: 'character-1' })
        expect(deps.map).toHaveBeenCalledWith({
            card: content.metadata,
            assets: content.assets.map(({ token, logicalId }) => ({ token, logicalId })),
        })
        expect(session.prepareOwnerManifestAndSeal).toHaveBeenCalledOnce()
        const manifestBytes = vi.mocked(session.prepareOwnerManifestAndSeal).mock.calls[0][0]
        expect(decodeOwnerManifest(manifestBytes)).toEqual([
            {
                tuple: ['config', `assets/${secondHash}.json`, 'json'],
                payloadHash: Uint8Array.from({ length: 32 }, () => 0x22),
            },
            {
                tuple: ['config duplicate', `assets/${secondHash}.json`, 'json'],
                payloadHash: Uint8Array.from({ length: 32 }, () => 0x22),
            },
        ])
        const expectedManifestHash = await ownerManifestIdentity(manifestBytes)
        expect(deps.upsert).toHaveBeenCalledWith(
            'character-1',
            'native-content-import',
            expect.any(Function),
            {
                assetAliases: [
                    {
                        kind: 'asset',
                        key: `assets/${firstHash}.png`,
                        objectHash: firstHash,
                        size: 4,
                        mime: 'image/png',
                        name: 'portrait',
                        ext: 'png',
                    },
                    {
                        kind: 'asset',
                        key: `assets/${secondHash}.json`,
                        objectHash: secondHash,
                        size: 9,
                        mime: 'application/json',
                        name: 'config',
                        ext: 'json',
                    },
                ],
                assetOwnerHeads: [{
                    owner: {
                        kind: 'character-additional-assets',
                        characterId: 'character-1',
                    },
                    present: true,
                    manifestHash: expectedManifestHash,
                    entryCount: 2,
                }],
            },
        )
        const create = vi.mocked(deps.upsert).mock.calls[0][2]
        expect(await create(null)).toEqual(mappedCharacter())
    })

    it('passes a CharX portrait and module overlay to the mapper while committing only ordinary asset aliases', async () => {
        const charxContent = {
            ...content,
            format: 'charx-card',
            portraitLogicalId: content.assets[0].logicalId,
            module: { trigger: [], regex: [], lorebook: [] },
        } as PreparedNativeContent
        const deps = dependencies()
        const session = lifecycle()

        await activatePreparedNativeCharacterContent(charxContent, session, deps)

        expect(deps.map).toHaveBeenCalledWith({
            card: content.metadata,
            assets: content.assets.map(({ token, logicalId }) => ({ token, logicalId })),
            portraitLogicalId: content.assets[0].logicalId,
            module: { trigger: [], regex: [], lorebook: [] },
        })
        const options = vi.mocked(deps.upsert).mock.calls[0][3]
        expect(options?.assetAliases).toEqual(expect.arrayContaining([
            expect.objectContaining({ kind: 'asset', key: content.assets[0].logicalId }),
        ]))
        expect(options?.assetAliases).not.toEqual(expect.arrayContaining([
            expect.objectContaining({ kind: 'inlay' }),
        ]))
        for (const alias of options?.assetAliases ?? []) {
            expect(Object.keys(alias).sort()).toEqual([
                'ext',
                'key',
                'kind',
                'mime',
                'name',
                'objectHash',
                'size',
            ])
        }
    })

    it('keeps an appended CharX JPEG portrait byte-exact as an ordinary asset alias', async () => {
        const portrait = {
            referenceKey: 'portrait',
            token: 'native-appended-portrait',
            logicalId: `assets/${firstHash}.jpeg`,
            objectHash: firstHash,
            byteSize: 10_485_763,
            mime: 'image/jpeg',
            name: 'original portrait',
            ext: 'jpeg',
        }
        const appendedCharxContent = {
            ...content,
            format: 'appended-charx-jpeg',
            assets: [portrait],
            portraitLogicalId: portrait.logicalId,
        } as PreparedNativeContent
        const character = mappedCharacter()
        character.additionalAssets = []
        const deps = dependencies({ map: vi.fn(async () => character) })
        const session = lifecycle()

        await activatePreparedNativeCharacterContent(appendedCharxContent, session, deps)

        expect(deps.map).toHaveBeenCalledWith({
            card: content.metadata,
            assets: [{ token: portrait.token, logicalId: portrait.logicalId }],
            portraitLogicalId: portrait.logicalId,
        })
        const options = vi.mocked(deps.upsert).mock.calls[0][3]
        expect(options?.assetAliases).toEqual([{
            kind: 'asset',
            key: portrait.logicalId,
            objectHash: portrait.objectHash,
            size: portrait.byteSize,
            mime: portrait.mime,
            name: portrait.name,
            ext: portrait.ext,
        }])
        expect(JSON.stringify(options?.assetAliases)).not.toMatch(/inlay|webp|resize/i)
    })

    it('decodes and activates PNG metadata using only logical portrait and chunk aliases', async () => {
        const portraitHash = '33'.repeat(32)
        const chunkHash = '44'.repeat(32)
        const decodedCard = {
            spec: 'chara_card_v2',
            spec_version: '2.0',
            data: {
                name: 'PNG Card',
                extensions: {
                    risuai: {
                        additionalAssets: [['chunk', '__asset:007', 'png']],
                    },
                },
            },
        }
        const pngContent = {
            casSessionId: 'content-1',
            format: 'png-card',
            metadata: { chara: 'encoded-v2', ccv3: 'encoded-v3' },
            portraitLogicalId: `assets/${portraitHash}.png`,
            assets: [
                {
                    referenceKey: 'native-png-portrait',
                    token: 'native-png-portrait',
                    logicalId: `assets/${portraitHash}.png`,
                    objectHash: portraitHash,
                    byteSize: 100,
                    mime: 'image/png',
                    name: `${portraitHash}.png`,
                    ext: 'png',
                },
                {
                    referenceKey: '007',
                    token: '007',
                    logicalId: `assets/${chunkHash}.png`,
                    objectHash: chunkHash,
                    byteSize: 4,
                    mime: '',
                    name: `${chunkHash}.png`,
                    ext: 'png',
                },
            ],
        } as PreparedNativeContent
        const character = mappedCharacter()
        character.image = pngContent.portraitLogicalId
        character.additionalAssets = [['chunk', pngContent.assets[1].logicalId, 'png']]
        const deps = dependencies({
            decodePng: vi.fn(async () => decodedCard as any),
            map: vi.fn(async () => character),
        })
        const session = lifecycle()

        await activatePreparedNativeCharacterContent(pngContent, session, deps)

        expect(deps.decodePng).toHaveBeenCalledWith(pngContent.metadata)
        expect(deps.map).toHaveBeenCalledWith({
            card: decodedCard,
            assets: [
                { token: 'native-png-portrait', logicalId: pngContent.assets[0].logicalId },
                { token: '007', logicalId: pngContent.assets[1].logicalId },
            ],
            portraitLogicalId: pngContent.portraitLogicalId,
        })
        const options = vi.mocked(deps.upsert).mock.calls[0][3]
        expect(options?.assetAliases).toEqual([
            expect.objectContaining({
                kind: 'asset',
                key: pngContent.assets[0].logicalId,
                mime: 'image/png',
            }),
            expect.objectContaining({
                kind: 'asset',
                key: pngContent.assets[1].logicalId,
                mime: '',
            }),
        ])
        expect(options?.assetAliases).not.toEqual(expect.arrayContaining([
            expect.objectContaining({ kind: 'inlay' }),
        ]))
        const manifestBytes = vi.mocked(session.prepareOwnerManifestAndSeal).mock.calls[0][0]
        expect(decodeOwnerManifest(manifestBytes)).toEqual([{
            tuple: ['chunk', pngContent.assets[1].logicalId, 'png'],
            payloadHash: Uint8Array.from({ length: 32 }, () => 0x44),
        }])
        expect(JSON.stringify(vi.mocked(deps.map).mock.calls)).not.toMatch(/payload|staged|path|Uint8Array/i)
    })

    it('declines a cancelled PNG password prompt before finalizing', async () => {
        const deps = dependencies({ decodePng: vi.fn(async () => null) })
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent({
            ...content,
            format: 'png-card',
            metadata: { chara: 'rcc-envelope' },
            portraitLogicalId: content.assets[0].logicalId,
        }, session, deps)).resolves.toBeNull()

        expect(deps.map).not.toHaveBeenCalled()
        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('deduplicates a PNG portrait and opaque chunk with the same exact bytes', async () => {
        const sharedHash = '55'.repeat(32)
        const logicalId = `assets/${sharedHash}.png`
        const pngContent = {
            ...content,
            format: 'png-card',
            metadata: { ccv3: 'encoded-card' },
            portraitLogicalId: logicalId,
            assets: [
                {
                    referenceKey: 'native-png-portrait',
                    token: 'native-png-portrait',
                    logicalId,
                    objectHash: sharedHash,
                    byteSize: 100,
                    mime: 'image/png',
                    name: `${sharedHash}.png`,
                    ext: 'png',
                },
                {
                    referenceKey: 'same',
                    token: 'same',
                    logicalId,
                    objectHash: sharedHash,
                    byteSize: 100,
                    mime: '',
                    name: `${sharedHash}.png`,
                    ext: 'png',
                },
            ],
        } as PreparedNativeContent
        const character = mappedCharacter()
        character.additionalAssets = [['same', logicalId, 'png']]
        const deps = dependencies({
            decodePng: vi.fn(async () => content.metadata as any),
            map: vi.fn(async () => character),
        })

        await activatePreparedNativeCharacterContent(pngContent, lifecycle(), deps)

        expect(vi.mocked(deps.upsert).mock.calls[0][3]?.assetAliases).toEqual([{
            kind: 'asset',
            key: logicalId,
            objectHash: sharedHash,
            size: 100,
            mime: 'image/png',
            name: `${sharedHash}.png`,
            ext: 'png',
        }])
    })

    it('rejects unsupported PNG inline payloads before finalizing', async () => {
        const deps = dependencies({
            decodePng: vi.fn(async () => {
                throw new UnsupportedPreparedNativeCharacterCardError('inline payload')
            }),
        })
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent({
            ...content,
            format: 'png-card',
            metadata: { ccv3: 'encoded-inline-card' },
            portraitLogicalId: content.assets[0].logicalId,
        }, session, deps)).rejects.toBeInstanceOf(UnsupportedPreparedNativeCharacterCardError)

        expect(deps.map).not.toHaveBeenCalled()
        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('cancels the retained route when abort arrives during PNG metadata decoding', async () => {
        const controller = new AbortController()
        const pending = deferred<any>()
        const decodeStarted = deferred<void>()
        const deps = dependencies({
            decodePng: vi.fn(async () => {
                decodeStarted.resolve()
                return pending.promise
            }),
        })
        const session = lifecycle()
        const receipt = {
            jobId: 'content-1',
            content: {
                ...content,
                format: 'png-card',
                metadata: { chara: 'rcc-envelope' },
                portraitLogicalId: content.assets[0].logicalId,
            } as PreparedNativeContent,
            warningCodes: [],
            ...session,
            confirmActivated: vi.fn(async () => undefined),
            cancel: vi.fn(async () => undefined),
        }

        const result = runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.png' },
            'card.png',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async (prepared) => prepared),
                activate: (prepared, activeReceipt, signal) =>
                    activatePreparedNativeCharacterContent(prepared, activeReceipt, deps, signal),
            },
            { signal: controller.signal },
        )
        await decodeStarted.promise
        controller.abort()
        pending.resolve(content.metadata)

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(receipt.cancel).toHaveBeenCalledOnce()
        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.map).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('cancels the retained route when abort arrives during semantic mapping', async () => {
        const controller = new AbortController()
        const pending = deferred<character | false>()
        const mapStarted = deferred<void>()
        const deps = dependencies({
            decodePng: vi.fn(async () => content.metadata as any),
            map: vi.fn(async () => {
                mapStarted.resolve()
                return pending.promise
            }),
        })
        const session = lifecycle()
        const receipt = {
            jobId: 'content-1',
            content: {
                ...content,
                format: 'png-card',
                metadata: { ccv3: 'encoded-card' },
                portraitLogicalId: content.assets[0].logicalId,
            } as PreparedNativeContent,
            warningCodes: [],
            ...session,
            confirmActivated: vi.fn(async () => undefined),
            cancel: vi.fn(async () => undefined),
        }

        const result = runNativePreparedContentRoute(
            { type: 'desktopPath', path: 'C:\\chosen\\card.png' },
            'card.png',
            {
                prepare: vi.fn(async () => receipt),
                map: vi.fn(async (prepared) => prepared),
                activate: (prepared, activeReceipt, signal) =>
                    activatePreparedNativeCharacterContent(prepared, activeReceipt, deps, signal),
            },
            { signal: controller.signal },
        )
        await mapStarted.promise
        controller.abort()
        pending.resolve(mappedCharacter())

        await expect(result).rejects.toMatchObject({ name: 'AbortError' })
        expect(receipt.cancel).toHaveBeenCalledOnce()
        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('finishes atomic activation when abort arrives after owner-manifest sealing begins', async () => {
        const controller = new AbortController()
        const sealStarted = deferred<void>()
        const allowSeal = deferred<void>()
        const deps = dependencies()
        const session = lifecycle({
            prepareOwnerManifestAndSeal: vi.fn(async (bytes) => {
                sealStarted.resolve()
                await allowSeal.promise
                return {
                    contentHash: await ownerManifestIdentity(bytes),
                    byteSize: bytes.byteLength,
                    physicalKey: 'sealed-owner-manifest',
                    deduplicated: false,
                }
            }),
        })

        const result = activatePreparedNativeCharacterContent(
            content,
            session,
            deps,
            controller.signal,
        )
        await sealStarted.promise
        controller.abort()
        allowSeal.resolve()

        await expect(result).resolves.toEqual({ characterId: 'character-1' })
        expect(session.prepareOwnerManifestAndSeal).toHaveBeenCalledOnce()
        expect(deps.upsert).toHaveBeenCalledOnce()
    })

    it('returns a normal declined outcome without preparing or publishing anything', async () => {
        const deps = dependencies({ map: vi.fn(async (): Promise<false> => false) })
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent(content, session, deps)).resolves.toBeNull()

        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('classifies off-spec JSON before calling the character card mapper', async () => {
        const deps = dependencies()
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent({
            ...content,
            metadata: { name: 'Legacy Tavern Card' },
        }, session, deps)).rejects.toBeInstanceOf(UnsupportedPreparedNativeCharacterCardError)

        expect(deps.map).not.toHaveBeenCalled()
        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('keeps v2 JSON on the compatibility importer until native v2 payload extraction exists', async () => {
        const deps = dependencies()
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent({
            ...content,
            metadata: {
                spec: 'chara_card_v2',
                spec_version: '2.0',
                data: { extensions: {} },
            },
        }, session, deps)).rejects.toBeInstanceOf(UnsupportedPreparedNativeCharacterCardError)

        expect(deps.map).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('commits a present empty additional-assets manifest', async () => {
        const character = mappedCharacter()
        character.additionalAssets = []
        const deps = dependencies({ map: vi.fn(async () => character) })
        const session = lifecycle()

        await activatePreparedNativeCharacterContent(content, session, deps)

        const manifestBytes = vi.mocked(session.prepareOwnerManifestAndSeal).mock.calls[0][0]
        expect(decodeOwnerManifest(manifestBytes)).toEqual([])
        expect(deps.upsert).toHaveBeenCalledWith(
            'character-1',
            'native-content-import',
            expect.any(Function),
            expect.objectContaining({
                assetOwnerHeads: [expect.objectContaining({
                    present: true,
                    entryCount: 0,
                })],
            }),
        )
    })

    it('fails closed when an additional asset does not have a prepared ordinary alias', async () => {
        const character = mappedCharacter()
        character.additionalAssets = [['missing', 'assets/missing.bin', 'bin']]
        const deps = dependencies({ map: vi.fn(async () => character) })
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent(content, session, deps))
            .rejects.toThrow(/missing prepared asset alias/i)

        expect(session.prepareOwnerManifestAndSeal).not.toHaveBeenCalled()
        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('rejects a manifest CAS identity mismatch before the database commit', async () => {
        const deps = dependencies()
        const session = lifecycle({
            prepareOwnerManifestAndSeal: vi.fn(async (bytes) => ({
                contentHash: 'ff'.repeat(32),
                byteSize: bytes.byteLength,
                physicalKey: 'wrong-object',
                deduplicated: false,
            })),
        })

        await expect(activatePreparedNativeCharacterContent(content, session, deps))
            .rejects.toThrow(/manifest CAS identity mismatch/i)

        expect(deps.upsert).not.toHaveBeenCalled()
    })

    it('leaves activation failure visible to the retained native job coordinator', async () => {
        const deps = dependencies({
            upsert: vi.fn(async () => {
                throw new Error('revision conflict')
            }),
        })
        const session = lifecycle()

        await expect(activatePreparedNativeCharacterContent(content, session, deps))
            .rejects.toThrow('revision conflict')

        expect(session.prepareOwnerManifestAndSeal).toHaveBeenCalledOnce()
    })
})

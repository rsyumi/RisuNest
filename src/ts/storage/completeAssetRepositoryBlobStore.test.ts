import { describe, expect, it, vi } from 'vitest'
import type { BlobMetadata, InlayBlobMetadata } from './blobStore'
import { createTauriCasObjectUrl } from './platformBlobStore'
import {
    createCompleteAssetRepositoryBlobStore,
    createCompleteTypedAssetRepository,
    type CompleteAssetAliasStore,
    type DurableAssetWriteSessionFactory,
    type RemoteAssetReader,
} from './assetRepository'
import type { ImmutablePayloadCas } from './payloadCas'
import type { AssetAlias, AssetAliasIdentity } from './persistentDataStore'

const assetHash = 'a'.repeat(64)
const inlayHash = 'b'.repeat(64)

function assetAlias(overrides: Partial<AssetAlias> = {}): AssetAlias {
    return {
        kind: 'asset',
        key: 'assets/photo.bin',
        objectHash: assetHash,
        size: 4,
        mime: 'application/octet-stream',
        name: 'photo.bin',
        ext: 'bin',
        ...overrides,
    } as AssetAlias
}

function createCas(overrides: Partial<ImmutablePayloadCas> = {}): ImmutablePayloadCas {
    return {
        prepare: vi.fn(async (data: Uint8Array) => ({
            contentHash: assetHash,
            byteSize: data.byteLength,
            physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
            deduplicated: false,
        })),
        readObject: vi.fn(async () => null),
        readObjectRange: vi.fn(async () => null),
        statObject: vi.fn(async () => null),
        ...overrides,
    }
}

function createStore(overrides: Partial<CompleteAssetAliasStore> = {}): CompleteAssetAliasStore {
    return {
        readRoot: vi.fn(async () => ({ revision: 10, value: {} as never })),
        readAssetAlias: vi.fn(async () => null),
        commitAssetAlias: vi.fn(async () => ({ revision: 11 })),
        listAssetAliases: vi.fn(async () => ({ revision: 10, items: [] })),
        deleteAssetAlias: vi.fn(async () => ({ revision: 11 })),
        ...overrides,
    }
}

function createLegacy() {
    return {
        read: vi.fn(async (_identity: AssetAliasIdentity) => null as Uint8Array | null),
        stat: vi.fn(async (_identity: AssetAliasIdentity) => null as BlobMetadata | null),
        resolveUrl: vi.fn(async (_identity: AssetAliasIdentity) => null as string | null),
    }
}

function createFacade(input: {
    remote?: RemoteAssetReader
    store?: CompleteAssetAliasStore
    cas?: ImmutablePayloadCas
    legacyFallback?: boolean
    legacy?: ReturnType<typeof createLegacy>
    writeSessions?: DurableAssetWriteSessionFactory
    resolveObjectUrl?: (
        input: Parameters<typeof createTauriCasObjectUrl>[0],
    ) => Promise<string | null>
} = {}) {
    const store = input.store ?? createStore()
    const cas = input.cas ?? createCas()
    const legacy = input.legacy ?? createLegacy()
    const resolveObjectUrl = vi.fn(
        input.resolveObjectUrl ?? (async () => 'risuasset://cas-object'),
    )
    const encodeNewInlayImage = vi.fn(async () => ({
        data: new Uint8Array([8, 6, 7]),
        metadata: {
            kind: 'inlay' as const,
            mime: 'image/webp',
            name: 'fresh.webp',
            ext: 'webp',
            inlayType: 'image' as const,
            width: 19,
            height: 23,
            preservationReason: undefined as InlayBlobMetadata['preservationReason'],
        },
    }))
    const options = {
        remote: input.remote,
        store,
        cas,
        legacy,
        legacyFallback: input.legacyFallback ?? true,
        objectUrls: { resolveObjectUrl },
        newInlayImages: { encodeNewInlayImage },
        ...(input.writeSessions === undefined ? {} : { writeSessions: input.writeSessions }),
        listPageSize: 2,
    }
    return {
        store,
        cas,
        legacy,
        resolveObjectUrl,
        encodeNewInlayImage,
        facade: createCompleteAssetRepositoryBlobStore(options),
        typed: createCompleteTypedAssetRepository(options),
    }
}

describe('complete AssetRepository BlobStore facade', () => {
    it('resolves a remote asset without fetching its bytes and validates full byte reads', async () => {
        const { hashPayloadBytes } = await import('./payloadCas')
        const data = new Uint8Array([4, 2, 9, 8])
        const hash = await hashPayloadBytes(data)
        const alias = assetAlias({ objectHash: hash })
        const remote = {
            statObject: vi.fn(async () => 4),
            readObject: vi.fn(async () => data),
        }
        const { facade, typed, cas, legacy } = createFacade({
            remote,
            store: createStore({
                readAssetAlias: vi.fn(async () => ({ revision: 10, value: alias })),
            }),
        })
        expect(await facade.resolveUrl(alias.key)).toBe('risuasset://cas-object')
        expect(remote.readObject).not.toHaveBeenCalled()
        expect((await typed.stat({ kind: 'asset', key: alias.key }))?.size).toBe(4)
        expect(await cas.statObject(hash)).toBeNull()
        expect(await facade.read(alias.key)).toEqual(data)
        expect(legacy.read).not.toHaveBeenCalled()
        remote.readObject.mockResolvedValue(new Uint8Array([1, 1, 1, 1]))
        await expect(facade.read(alias.key)).rejects.toThrow(
            'Remote asset identity mismatch',
        )
    })
    it('rejects mismatched remote size and short range responses', async () => {
        const alias = assetAlias()
        const remote = {
            statObject: vi.fn(async () => 5),
            readObject: vi.fn(async () => new Uint8Array([1])),
        }
        const { facade } = createFacade({
            remote,
            store: createStore({
                readAssetAlias: vi.fn(async () => ({ revision: 10, value: alias })),
            }),
        })
        await expect(facade.resolveUrl(alias.key)).rejects.toThrow('size mismatch')
        await expect(
            facade.read(alias.key, { start: 1, endExclusive: 3 }),
        ).rejects.toThrow('Remote asset identity mismatch')
    })
    it('keeps same-key opposite-kind aliases independently reachable through the typed core', async () => {
        const key = 'shared-key'
        const asset = assetAlias({ key, size: 1 })
        const inlay: AssetAlias = {
            kind: 'inlay',
            key,
            objectHash: inlayHash,
            size: 1,
            mime: 'audio/ogg',
            name: 'shared.ogg',
            ext: 'ogg',
            inlayType: 'audio',
        }
        const readAssetAlias = vi.fn(async (identity: AssetAliasIdentity) => ({
            revision: 12,
            value: identity.kind === 'asset' ? asset : inlay,
        }))
        const deleteAssetAlias = vi.fn(async () => ({ revision: 13 }))
        const cas = createCas({
            readObject: vi.fn(async (hash) => new Uint8Array([
                hash === assetHash ? 1 : 2,
            ])),
        })
        const { typed } = createFacade({
            store: createStore({ readAssetAlias, deleteAssetAlias }),
            cas,
        })

        await expect(typed.read({ kind: 'asset', key })).resolves.toEqual(new Uint8Array([1]))
        await expect(typed.read({ kind: 'inlay', key })).resolves.toEqual(new Uint8Array([2]))
        await typed.remove({ kind: 'asset', key })

        expect(deleteAssetAlias).toHaveBeenCalledWith({ kind: 'asset', key }, 12)
    })

    it('rejects key and kind mismatches at the key-only BlobStore boundary', async () => {
        const { facade, cas } = createFacade()

        await expect(facade.put('assets/inlay.webp', new Uint8Array([1]), {
            kind: 'inlay',
            mime: 'image/webp',
            name: 'inlay.webp',
            ext: 'webp',
            inlayType: 'image',
        })).rejects.toThrow('namespace does not match')
        await expect(facade.putNewInlayImage(
            'assets/inlay.webp',
            new Uint8Array([1]),
            { name: 'inlay.webp' },
        )).rejects.toThrow('namespace does not match')
        expect(cas.prepare).not.toHaveBeenCalled()
    })

    it('publishes ordinary asset bytes exactly before committing a typed alias', async () => {
        const { facade, cas, store } = createFacade()
        const input = new Uint8Array([0, 255, 7, 42])

        const metadata = await facade.put('assets/photo.bin', input, {
            kind: 'asset',
            mime: 'application/x-exact',
            name: 'photo.bin',
            ext: 'bin',
        })
        input.fill(1)

        expect(cas.prepare).toHaveBeenCalledWith(new Uint8Array([0, 255, 7, 42]))
        expect(store.commitAssetAlias).toHaveBeenCalledWith({
            kind: 'asset',
            key: 'assets/photo.bin',
            objectHash: assetHash,
            size: 4,
            mime: 'application/x-exact',
            name: 'photo.bin',
            ext: 'bin',
        }, 10)
        expect(metadata).toEqual({
            kind: 'asset',
            key: 'assets/photo.bin',
            size: 4,
            mime: 'application/x-exact',
            name: 'photo.bin',
            ext: 'bin',
        })
    })

    it('keeps a native direct-object pin sealed until the exact alias is durable', async () => {
        const events: string[] = []
        const writeSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => ({
                prepare: vi.fn(async (data) => {
                    events.push(`prepare:${[...data].join(',')}:direct-object`)
                    return {
                        contentHash: assetHash,
                        byteSize: data.byteLength,
                        physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
                        deduplicated: false,
                    }
                }),
                seal: vi.fn(async () => { events.push('seal') }),
                release: vi.fn(async (outcome) => { events.push(`release:${outcome}`) }),
            })),
        }
        const store = createStore({
            commitAssetAlias: vi.fn(async () => {
                events.push('commit-alias')
                return { revision: 11 }
            }),
        })
        const { facade, cas } = createFacade({ store, writeSessions })

        await facade.put('assets/photo.bin', Uint8Array.of(0, 255, 7, 42), {
            kind: 'asset',
            mime: 'application/x-exact',
            name: 'photo.bin',
            ext: 'bin',
        })

        expect(events).toEqual([
            'prepare:0,255,7,42:direct-object',
            'seal',
            'commit-alias',
            'release:committed',
        ])
        expect(cas.prepare).not.toHaveBeenCalled()
    })

    it('prepares native bytes without sealing or committing until guarded activation', async () => {
        const events: string[] = []
        const writeSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => {
                events.push('begin')
                return {
                    prepare: vi.fn(async (data) => {
                        events.push(`prepare:${[...data].join(',')}`)
                        return {
                            contentHash: assetHash,
                            byteSize: data.byteLength,
                            physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
                            deduplicated: false,
                        }
                    }),
                    seal: vi.fn(async () => { events.push('seal') }),
                    release: vi.fn(async (outcome) => { events.push(`release:${outcome}`) }),
                }
            }),
        }
        const store = createStore({
            commitAssetAlias: vi.fn(async () => {
                events.push('commit-alias')
                return { revision: 11 }
            }),
        })
        const { facade } = createFacade({ store, writeSessions })

        const prepared = await facade.prepareOwnedPut(
            'assets/photo.bin',
            Uint8Array.of(1, 2, 3),
            {
                kind: 'asset',
                mime: 'application/octet-stream',
                name: 'photo.bin',
                ext: 'bin',
            },
        )

        expect(events).toEqual(['begin', 'prepare:1,2,3'])
        await prepared.activate()
        expect(events).toEqual([
            'begin',
            'prepare:1,2,3',
            'seal',
            'commit-alias',
            'release:committed',
        ])
    })

    it('aborts prepared but unactivated native bytes without sealing them', async () => {
        const seal = vi.fn(async () => undefined)
        const release = vi.fn(async () => undefined)
        const writeSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => ({
                prepare: vi.fn(async (data) => ({
                    contentHash: assetHash,
                    byteSize: data.byteLength,
                    physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
                    deduplicated: false,
                })),
                seal,
                release,
            })),
        }
        const { facade, store } = createFacade({ writeSessions })

        const prepared = await facade.prepareOwnedPut(
            'assets/aborted.bin',
            Uint8Array.of(9),
            {
                kind: 'asset',
                mime: 'application/octet-stream',
                name: 'aborted.bin',
                ext: 'bin',
            },
        )
        await prepared.abort()

        expect(seal).not.toHaveBeenCalled()
        expect(store.commitAssetAlias).not.toHaveBeenCalled()
        expect(release).toHaveBeenCalledExactlyOnceWith('aborted')
    })

    it('retains durable ownership when seal commits natively but its IPC response fails', async () => {
        const sealFailure = new Error('seal response lost after durable mutation')
        const release = vi.fn(async () => undefined)
        let durableSealed = false
        const writeSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => ({
                prepare: vi.fn(async (data) => ({
                    contentHash: assetHash,
                    byteSize: data.byteLength,
                    physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
                    deduplicated: false,
                })),
                seal: vi.fn(async () => {
                    durableSealed = true
                    throw sealFailure
                }),
                release,
            })),
        }
        const { facade, store } = createFacade({ writeSessions })
        const prepared = await facade.prepareOwnedPut(
            'assets/ambiguous-seal.bin',
            Uint8Array.of(5),
            {
                kind: 'asset',
                mime: 'application/octet-stream',
                name: 'ambiguous-seal.bin',
                ext: 'bin',
            },
        )

        await expect(prepared.activate()).rejects.toBe(sealFailure)

        expect(durableSealed).toBe(true)
        expect(release).not.toHaveBeenCalled()
        expect(store.commitAssetAlias).not.toHaveBeenCalled()
    })

    it('aborts an unsealed direct write but retains a sealed session on ambiguous activation failure', async () => {
        const preSealRelease = vi.fn(async () => undefined)
        const preSealFailure = new Error('prepare failed')
        const preSealSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => ({
                prepare: vi.fn(async () => { throw preSealFailure }),
                seal: vi.fn(async () => undefined),
                release: preSealRelease,
            })),
        }
        const preSeal = createFacade({ writeSessions: preSealSessions })

        await expect(preSeal.facade.put('assets/pre-seal.bin', Uint8Array.of(1), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'pre-seal.bin',
            ext: 'bin',
        })).rejects.toBe(preSealFailure)
        expect(preSealRelease).toHaveBeenCalledWith('aborted')

        const sealedRelease = vi.fn(async () => undefined)
        const activationFailure = new Error('activation outcome unknown')
        const sealedSessions: DurableAssetWriteSessionFactory = {
            begin: vi.fn(async () => ({
                prepare: vi.fn(async (data) => ({
                    contentHash: assetHash,
                    byteSize: data.byteLength,
                    physicalKey: `assets-v2/objects/aa/${'a'.repeat(62)}`,
                    deduplicated: false,
                })),
                seal: vi.fn(async () => undefined),
                release: sealedRelease,
            })),
        }
        const sealed = createFacade({
            writeSessions: sealedSessions,
            store: createStore({
                commitAssetAlias: vi.fn(async () => { throw activationFailure }),
            }),
        })

        await expect(sealed.facade.put('assets/sealed.bin', Uint8Array.of(2), {
            kind: 'asset',
            mime: 'application/octet-stream',
            name: 'sealed.bin',
            ext: 'bin',
        })).rejects.toBe(activationFailure)
        expect(sealedRelease).not.toHaveBeenCalled()
    })

    it('uses bounded CAS reads and never falls back for a non-null object hash', async () => {
        const alias = assetAlias()
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 4, value: alias })),
        })
        const cas = createCas({
            statObject: vi.fn(async () => 4),
            readObjectRange: vi.fn(async () => new Uint8Array([2, 3])),
        })
        const legacy = createLegacy()
        legacy.read.mockResolvedValue(new Uint8Array([9, 9, 9, 9]))
        const { facade } = createFacade({ store, cas, legacy })

        await expect(facade.read('assets/photo.bin', { start: 1, endExclusive: 3 }))
            .resolves.toEqual(new Uint8Array([2, 3]))
        expect(cas.readObjectRange).toHaveBeenCalledWith(assetHash, {
            start: 1,
            endExclusive: 3,
        })
        expect(legacy.read).not.toHaveBeenCalled()

        vi.mocked(cas.readObjectRange).mockResolvedValueOnce(null)
        await expect(facade.read('assets/photo.bin', { start: 1, endExclusive: 3 }))
            .resolves.toBeNull()
        expect(legacy.read).not.toHaveBeenCalled()
    })

    it('allows legacy fallback only for an explicit null-hash alias', async () => {
        const alias = assetAlias({ objectHash: null })
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 4, value: alias })),
        })
        const legacy = createLegacy()
        legacy.read.mockResolvedValue(new Uint8Array([1, 2, 3, 4]))
        legacy.resolveUrl.mockResolvedValue('risuasset://legacy')
        const { facade } = createFacade({ store, legacy })

        await expect(facade.read('assets/photo.bin')).resolves.toEqual(new Uint8Array([1, 2, 3, 4]))
        await expect(facade.resolveUrl('assets/photo.bin')).resolves.toBe('risuasset://legacy')
        expect(legacy.read).toHaveBeenCalledWith({ kind: 'asset', key: 'assets/photo.bin' })
        expect(legacy.resolveUrl).toHaveBeenCalledWith({ kind: 'asset', key: 'assets/photo.bin' })
    })

    it('keeps explicit null-hash legacy Range reads bounded', async () => {
        const alias = assetAlias({ objectHash: null })
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 4, value: alias })),
        })
        const legacy = createLegacy()
        legacy.read.mockResolvedValue(new Uint8Array([2, 3]))
        const { facade } = createFacade({ store, legacy })

        await expect(facade.read('assets/photo.bin', { start: 1, endExclusive: 3 }))
            .resolves.toEqual(new Uint8Array([2, 3]))
        expect(legacy.read).toHaveBeenCalledWith(
            { kind: 'asset', key: 'assets/photo.bin' },
            { start: 1, endExclusive: 3 },
        )
    })

    it('pages the typed alias catalog without enumerating CAS objects', async () => {
        const first = assetAlias({ key: 'assets/a' })
        const second = assetAlias({ key: 'assets/b' })
        const listAssetAliases = vi.fn(async ({ cursor }: { cursor?: string }) => cursor
            ? { revision: 8, items: [second] }
            : { revision: 8, items: [first], nextCursor: 'page-2' })
        const store = createStore({ listAssetAliases })
        const { facade } = createFacade({ store })

        await expect(facade.list({ kind: 'asset' })).resolves.toEqual([
            expect.objectContaining({ kind: 'asset', key: 'assets/a' }),
            expect.objectContaining({ kind: 'asset', key: 'assets/b' }),
        ])
        expect(listAssetAliases).toHaveBeenNthCalledWith(1, { kind: 'asset', limit: 2 })
        expect(listAssetAliases).toHaveBeenNthCalledWith(2, {
            kind: 'asset',
            limit: 2,
            cursor: 'page-2',
        })
    })

    it('fails closed if the catalog revision changes while paging', async () => {
        const listAssetAliases = vi.fn(async ({ cursor }: { cursor?: string }) => cursor
            ? { revision: 9, items: [] }
            : { revision: 8, items: [], nextCursor: 'page-2' })
        const { facade } = createFacade({ store: createStore({ listAssetAliases }) })

        await expect(facade.list()).rejects.toThrow('revision changed while listing')
    })

    it('deletes only the typed alias and never deletes physical CAS bytes', async () => {
        const alias = assetAlias()
        const readAssetAlias = vi.fn(async () => ({ revision: 15, value: alias }))
        const deleteAssetAlias = vi.fn(async () => ({ revision: 16 }))
        const store = createStore({ readAssetAlias, deleteAssetAlias })
        const { facade } = createFacade({ store })

        await facade.remove('assets/photo.bin')

        expect(readAssetAlias).toHaveBeenCalledWith({ kind: 'asset', key: 'assets/photo.bin' })
        expect(deleteAssetAlias).toHaveBeenCalledWith({ kind: 'asset', key: 'assets/photo.bin' }, 15)
    })

    it('resolves a validated CAS object descriptor and does not fall back when the object is missing', async () => {
        const alias = assetAlias()
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 3, value: alias })),
        })
        const cas = createCas({ statObject: vi.fn(async () => 4) })
        const legacy = createLegacy()
        legacy.resolveUrl.mockResolvedValue('risuasset://legacy')
        const { facade, resolveObjectUrl } = createFacade({ store, cas, legacy })

        await expect(facade.resolveUrl('assets/photo.bin')).resolves.toBe('risuasset://cas-object')
        expect(resolveObjectUrl).toHaveBeenCalledWith({
            contentHash: assetHash,
            mime: 'application/octet-stream',
            size: 4,
        })

        vi.mocked(cas.statObject).mockResolvedValueOnce(null)
        await expect(facade.resolveUrl('assets/photo.bin')).resolves.toBeNull()
        expect(legacy.resolveUrl).not.toHaveBeenCalled()
    })

    it('derives a display MIME for an empty-MIME CAS alias without mutating persisted metadata', async () => {
        const alias = assetAlias({
            key: 'assets/avatar.PNG',
            mime: '',
            ext: '.PNG',
            name: 'avatar.PNG',
        })
        const persistedAlias = structuredClone(alias)
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 3, value: alias })),
        })
        const cas = createCas({ statObject: vi.fn(async () => 4) })
        const endpoint =
            'http://127.0.0.1:12345/0123456789abcdef0123456789abcdef/'
        const { facade, resolveObjectUrl } = createFacade({
            store,
            cas,
            resolveObjectUrl: async (input) =>
                createTauriCasObjectUrl(input, endpoint),
        })

        const url = await facade.resolveUrl('assets/avatar.PNG')

        expect(url).toContain('?mime=image%2Fpng&size=4')
        expect(resolveObjectUrl).toHaveBeenCalledWith({
            contentHash: assetHash,
            mime: 'image/png',
            size: 4,
        })
        expect(alias).toEqual(persistedAlias)
        expect(alias.mime).toBe('')
        expect(cas.readObject).not.toHaveBeenCalled()
        expect(cas.readObjectRange).not.toHaveBeenCalled()
    })

    it('preserves native URL header rejection for an invalid nonempty alias MIME', async () => {
        const alias = assetAlias({ mime: 'image/png\0text/html', ext: 'png' })
        const store = createStore({
            readAssetAlias: vi.fn(async () => ({ revision: 3, value: alias })),
        })
        const cas = createCas({ statObject: vi.fn(async () => 4) })
        const { facade } = createFacade({
            store,
            cas,
            resolveObjectUrl: async (input) =>
                createTauriCasObjectUrl(
                    input,
                    'http://127.0.0.1:12345/0123456789abcdef0123456789abcdef/',
                ),
        })

        await expect(facade.resolveUrl('assets/photo.bin')).rejects.toThrow(
            'MIME',
        )
        expect(alias.mime).toBe('image/png\0text/html')
    })

    it('returns the preservation outcome after activation without storing it in the alias', async () => {
        const { facade, encodeNewInlayImage, store } = createFacade()
        const encoded = await encodeNewInlayImage()
        encodeNewInlayImage.mockResolvedValue({
            ...encoded, metadata: { ...encoded.metadata, preservationReason: 'animation-cost' },
        })
        const result = await facade.putNewInlayImage('preserved', new Uint8Array([1]), { name: 'loop.gif' })
        expect(result.preservationReason).toBe('animation-cost')
        expect(vi.mocked(store.commitAssetAlias).mock.calls[0][0]).not.toHaveProperty('preservationReason')
    })

    it('encodes a new Inlay once and publishes the returned encoded bytes without another transform', async () => {
        const cas = createCas({
            prepare: vi.fn(async (data) => ({
                contentHash: inlayHash,
                byteSize: data.byteLength,
                physicalKey: `assets-v2/objects/bb/${'b'.repeat(62)}`,
                deduplicated: false,
            })),
        })
        const { facade, encodeNewInlayImage, store } = createFacade({ cas })
        const source = new Uint8Array([1, 2, 3, 4])

        const metadata = await facade.putNewInlayImage('inlay-key', source, { name: 'fresh.png' })

        expect(encodeNewInlayImage).toHaveBeenCalledTimes(1)
        expect(encodeNewInlayImage).toHaveBeenCalledWith('inlay-key', source, { name: 'fresh.png' })
        expect(cas.prepare).toHaveBeenCalledWith(new Uint8Array([8, 6, 7]))
        expect(store.commitAssetAlias).toHaveBeenCalledWith({
            kind: 'inlay',
            key: 'inlay-key',
            objectHash: inlayHash,
            size: 3,
            mime: 'image/webp',
            name: 'fresh.webp',
            ext: 'webp',
            inlayType: 'image',
            width: 19,
            height: 23,
        }, 10)
        expect(metadata).toEqual(expect.objectContaining({
            kind: 'inlay',
            key: 'inlay-key',
            size: 3,
            width: 19,
            height: 23,
        }))
    })
})

import { expect, it, vi } from 'vitest'
vi.mock('../characterCards', () => ({
    decodePreparedNativePngCharacterCard: vi.fn(),
    mapPreparedNativeCharacterCard: vi.fn(),
}))
vi.mock('../characters', () => ({ createBlankChar: vi.fn() }))
vi.mock('../alert', () => ({ alertConfirm: vi.fn() }))
vi.mock('./persistentDataRuntime.svelte', () => ({
    upsertPersistentCompleteCharacter: vi.fn(),
    appendPersistentRootModule: vi.fn(),
}))
import { mapPreparedNativeCharacterCard } from '../characterCards'
import { upsertPersistentCompleteCharacter } from './persistentDataRuntime.svelte'
import { activatePreparedNativeModuleContent } from './nativeModuleContentActivation'
import {
    decodeOwnerManifest,
    ownerManifestIdentity,
} from './ownerManifestCodec'
import type { character } from './database.svelte'
import type { PreparedNativeContent } from './nativeFileJobs'

it('converts CharX to a module with its owner manifest, without committing an intermediate character', async () => {
    const hash = 'ab'.repeat(32)
    const key = `assets/${hash}.png`
    const character = {
        chaId: 'temporary',
        name: 'synthetic',
        additionalAssets: [
            ['first', key, 'png'],
            ['second', key, 'png'],
        ],
        globalLore: [{ content: 'synthetic lore' }],
    } as character
    vi.mocked(mapPreparedNativeCharacterCard).mockResolvedValue(character)
    const content: PreparedNativeContent = {
        casSessionId: 'job',
        format: 'charx-card',
        metadata: {
            spec: 'chara_card_v3',
            spec_version: '3.0',
            data: { extensions: {} },
        },
        assets: [
            {
                token: 'asset',
                referenceKey: 'asset',
                logicalId: key,
                objectHash: hash,
                byteSize: 4,
                ext: 'png',
                mime: 'image/png',
                name: 'first',
            },
        ],
    }
    let manifest!: Uint8Array
    const append = vi.fn()
    const result = await activatePreparedNativeModuleContent(
        content,
        {
            prepareOwnerManifestAndSeal: async (bytes) => {
                manifest = bytes
                return {
                    contentHash: await ownerManifestIdentity(bytes),
                    byteSize: bytes.length,
                    physicalKey: 'synthetic',
                    deduplicated: false,
                }
            },
        },
        { createId: () => 'module-id', confirmLowLevelAccess: vi.fn(), append },
    )
    expect(result).toEqual({ moduleId: 'module-id' })
    expect(upsertPersistentCompleteCharacter).not.toHaveBeenCalled()
    expect(append).toHaveBeenCalledOnce()
    const input = append.mock.calls[0][0]
    expect(input.module.assets).toEqual(character.additionalAssets)
    expect(input.module.lorebook).toEqual(character.globalLore)
    expect(input.assetAliases).toHaveLength(1)
    expect(input.ownerHead).toEqual({
        present: true,
        entryCount: 2,
        manifestHash: await ownerManifestIdentity(manifest),
    })
    expect(decodeOwnerManifest(manifest)).toHaveLength(2)
})

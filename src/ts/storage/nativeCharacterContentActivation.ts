import {
    mapPreparedNativeCharacterCard,
    type PreparedNativeCharacterCardInput,
    type PreparedNativeCharacterCardMetadata,
} from '../characterCards'
import type { character } from './database.svelte'
import { createNativeImmutablePayloadCas } from './nativeAssetRepository'
import type { PreparedNativeContent } from './nativeFileJobs'
import { encodeOwnerManifest, ownerManifestIdentity } from './ownerManifestCodec'
import type { PreparedImmutablePayload } from './payloadCas'
import { upsertPersistentCompleteCharacter } from './persistentDataRuntime.svelte'
import type {
    PersistentCharacterAssetAlias,
    PersistentCharacterAssetOwnerHead,
    PersistentCompleteCharacterUpsert,
    PersistentCompleteCharacterUpsertOptions,
} from './saveCoordinator'

export interface NativeCharacterContentActivationDependencies {
    map(input: PreparedNativeCharacterCardInput): Promise<character | false>
    prepareManifest(bytes: Uint8Array): Promise<PreparedImmutablePayload>
    upsert(
        characterId: string,
        reason: string,
        createOrMutate: PersistentCompleteCharacterUpsert,
        options?: PersistentCompleteCharacterUpsertOptions,
    ): Promise<boolean>
}

export interface NativeCharacterContentActivationResult {
    characterId: string
}

export class UnsupportedPreparedNativeCharacterCardError extends TypeError {
    readonly code = 'unsupported-character-card'

    constructor() {
        super('Prepared JSON is not a supported character card')
        this.name = 'UnsupportedPreparedNativeCharacterCardError'
    }
}

function isRecord(value: unknown): value is Record<string, unknown> {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
}

function requireCharacterCardMetadata(
    value: Record<string, unknown>,
): PreparedNativeCharacterCardMetadata {
    if (
        (value.spec !== 'chara_card_v2' && value.spec !== 'chara_card_v3')
        || typeof value.spec_version !== 'string'
        || !isRecord(value.data)
        || !isRecord(value.data.extensions)
        || (
            value.spec === 'chara_card_v3'
            && value.data.assets !== undefined
            && !Array.isArray(value.data.assets)
        )
    ) {
        throw new UnsupportedPreparedNativeCharacterCardError()
    }
    return value as unknown as PreparedNativeCharacterCardMetadata
}

function hashBytes(hash: string): Uint8Array {
    if (!/^[0-9a-f]{64}$/.test(hash)) {
        throw new TypeError('Prepared asset object hash is invalid')
    }
    return Uint8Array.from(
        { length: 32 },
        (_, index) => Number.parseInt(hash.slice(index * 2, index * 2 + 2), 16),
    )
}

function preparedAliases(
    content: PreparedNativeContent,
): PersistentCharacterAssetAlias[] {
    const aliases = new Map<string, PersistentCharacterAssetAlias>()
    for (const asset of content.assets) {
        const alias: PersistentCharacterAssetAlias = {
            kind: 'asset',
            key: asset.logicalId,
            objectHash: asset.objectHash,
            size: asset.byteSize,
            mime: asset.mime,
            name: asset.name,
            ext: asset.ext,
        }
        const existing = aliases.get(alias.key)
        if (existing) {
            if (
                existing.objectHash !== alias.objectHash
                || existing.size !== alias.size
                || existing.mime !== alias.mime
                || existing.ext !== alias.ext
            ) {
                throw new TypeError(`Conflicting prepared asset alias: ${alias.key}`)
            }
            continue
        }
        aliases.set(alias.key, alias)
    }
    return [...aliases.values()]
}

async function prepareAdditionalAssetOwnerHead(
    character: character,
    aliases: readonly PersistentCharacterAssetAlias[],
    prepareManifest: NativeCharacterContentActivationDependencies['prepareManifest'],
): Promise<PersistentCharacterAssetOwnerHead> {
    const aliasesByKey = new Map(aliases.map((alias) => [alias.key, alias]))
    const additionalAssets = character.additionalAssets ?? []
    const entries = additionalAssets.map((tuple) => {
        const alias = aliasesByKey.get(tuple[1])
        if (!alias?.objectHash) {
            throw new TypeError(`Missing prepared asset alias: ${tuple[1]}`)
        }
        return {
            tuple: [tuple[0], tuple[1], tuple[2]] as const,
            payloadHash: hashBytes(alias.objectHash),
        }
    })
    const bytes = encodeOwnerManifest(entries)
    const [prepared, manifestHash] = await Promise.all([
        prepareManifest(bytes),
        ownerManifestIdentity(bytes),
    ])
    if (
        prepared.contentHash !== manifestHash
        || prepared.byteSize !== bytes.byteLength
    ) {
        throw new Error('Owner manifest CAS identity mismatch')
    }
    return {
        owner: {
            kind: 'character-additional-assets',
            characterId: character.chaId,
        },
        present: true,
        manifestHash,
        entryCount: entries.length,
    }
}

const productionDependencies: NativeCharacterContentActivationDependencies = {
    map: mapPreparedNativeCharacterCard,
    prepareManifest: (bytes) => createNativeImmutablePayloadCas().prepare(bytes),
    upsert: upsertPersistentCompleteCharacter,
}

export async function activatePreparedNativeCharacterContent(
    content: PreparedNativeContent,
    dependencies: NativeCharacterContentActivationDependencies = productionDependencies,
): Promise<NativeCharacterContentActivationResult | null> {
    const card = requireCharacterCardMetadata(content.metadata)
    const character = await dependencies.map({
        card,
        assets: content.assets.map(({ token, logicalId }) => ({ token, logicalId })),
    })
    if (!character) return null

    const assetAliases = preparedAliases(content)
    const assetOwnerHead = await prepareAdditionalAssetOwnerHead(
        character,
        assetAliases,
        dependencies.prepareManifest,
    )
    await dependencies.upsert(
        character.chaId,
        'native-content-import',
        () => character,
        {
            assetAliases,
            assetOwnerHeads: [assetOwnerHead],
        },
    )
    return { characterId: character.chaId }
}

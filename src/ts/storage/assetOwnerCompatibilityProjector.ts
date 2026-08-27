import type { Database } from './database.svelte'
import {
    decodeOwnerManifest,
    ownerManifestIdentity,
    type AssetTuple,
} from './ownerManifestCodec'
import type {
    AssetOwnerHead,
    AssetOwnerLocator,
    PersistentRevisionReader,
} from './persistentDataStore'
import {
    assertPinnedRevision,
    iteratePinnedCharacters,
    iteratePinnedConversations,
} from './persistentRecordIterator'
import { defineOwnEnumerableProperty } from './ownEnumerableProperty'

export interface AssetRepositoryOwnerManifestView {
    readOwnerManifest(manifestHash: string): Promise<Uint8Array | null>
}

function matchesTupleArray(
    value: unknown,
    expected: readonly AssetTuple[],
    allowTrailingFields: boolean,
): boolean {
    if (!Array.isArray(value) || value.length !== expected.length) return false
    return value.every((tuple, index) => (
        Array.isArray(tuple)
        && (allowTrailingFields ? tuple.length >= 3 : tuple.length === 3)
        && tuple[0] === expected[index][0]
        && tuple[1] === expected[index][1]
        && tuple[2] === expected[index][2]
    ))
}

async function applyOwnerHead(
    reader: PersistentRevisionReader,
    repository: AssetRepositoryOwnerManifestView,
    owner: AssetOwnerLocator,
    target: Record<string, unknown>,
    property: string,
): Promise<void> {
    const record = await reader.readAssetOwnerHead(owner)
    if (!record) return
    assertPinnedRevision(reader.revision, record.revision, 'Asset owner head')
    const head: AssetOwnerHead = record.value
    const legacyPresent = Object.prototype.hasOwnProperty.call(target, property)
    if (legacyPresent !== head.present) {
        throw new Error('Asset owner head property presence does not match pinned legacy data')
    }
    if (!head.present) return

    const bytes = await repository.readOwnerManifest(head.manifestHash)
    if (bytes === null) {
        throw new Error(`Missing owner manifest ${head.manifestHash}`)
    }
    const actualHash = await ownerManifestIdentity(bytes)
    if (actualHash !== head.manifestHash) {
        throw new Error(`Owner manifest hash mismatch for ${head.manifestHash}`)
    }
    const entries = decodeOwnerManifest(bytes)
    if (entries.length !== head.entryCount) {
        throw new Error(`Owner manifest entry count mismatch for ${head.manifestHash}`)
    }
    const tuples = entries.map(({ tuple }) => [
        tuple[0],
        tuple[1],
        tuple[2],
    ] as [string, string, string])
    if (!matchesTupleArray(
        target[property],
        tuples,
        owner.kind !== 'character-additional-assets',
    )) {
        throw new Error('Owner manifest does not match pinned legacy tuples')
    }
}

async function pinnedPresets(
    reader: PersistentRevisionReader,
): Promise<Database['botPresets']> {
    const catalog = await reader.queryPresets()
    assertPinnedRevision(reader.revision, catalog.revision, 'Preset catalog')
    const presets: Database['botPresets'] = []
    for (const summary of catalog.items) {
        const preset = await reader.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
        presets.push(preset.value)
    }
    return presets
}

async function pinnedPluginStorage(
    reader: PersistentRevisionReader,
): Promise<Database['pluginCustomStorage']> {
    const catalog = await reader.queryPluginStorage()
    assertPinnedRevision(reader.revision, catalog.revision, 'Plugin storage catalog')
    const storage: Database['pluginCustomStorage'] = {}
    for (const summary of catalog.items) {
        const value = await reader.readPluginStorage(summary.key)
        if (!value) throw new Error(`Missing plugin storage value for ${summary.key}`)
        assertPinnedRevision(
            reader.revision,
            value.revision,
            `Plugin storage value ${summary.key}`,
        )
        defineOwnEnumerableProperty(storage, summary.key, value.value)
    }
    return storage
}

export async function projectPinnedCompatibilityDatabase(
    reader: PersistentRevisionReader,
    repository: AssetRepositoryOwnerManifestView,
): Promise<Database> {
    const rootRecord = await reader.readRoot()
    assertPinnedRevision(reader.revision, rootRecord.revision, 'Root')
    const root = structuredClone(rootRecord.value)
    for (const [index, module] of (root.modules ?? []).entries()) {
        await applyOwnerHead(
            reader,
            repository,
            { kind: 'root-module-assets', index },
            module as unknown as Record<string, unknown>,
            'assets',
        )
    }
    for (const [index, persona] of (root.personas ?? []).entries()) {
        if (!persona.embeddedModule) continue
        await applyOwnerHead(
            reader,
            repository,
            { kind: 'persona-embedded-module-assets', index },
            persona.embeddedModule as unknown as Record<string, unknown>,
            'assets',
        )
    }

    const characters: Database['characters'] = []
    for await (const record of iteratePinnedCharacters(reader)) {
        const detail = structuredClone(record.detail)
        await applyOwnerHead(
            reader,
            repository,
            {
                kind: 'character-additional-assets',
                characterId: record.summary.id,
            },
            detail as unknown as Record<string, unknown>,
            'additionalAssets',
        )
        const chats: Database['characters'][number]['chats'] = []
        for await (const conversation of iteratePinnedConversations(reader, record.summary.id)) {
            chats.push(conversation.value)
        }
        characters.push({ ...detail, chats } as Database['characters'][number])
    }

    return {
        ...root,
        characters,
        botPresets: await pinnedPresets(reader),
        pluginCustomStorage: await pinnedPluginStorage(reader),
    } as Database
}

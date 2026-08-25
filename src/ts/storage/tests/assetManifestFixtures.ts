import type { Database } from '../database.svelte'
import { fixtureDatabase } from './persistentDataFixtures'

export const ASSET_TUPLES_PER_OWNER = 154_448
export const ASSET_MANIFEST_OWNER_COUNT = 3

export interface AssetManifestFixtureMeasurement {
    ownerCount: number
    tuplesPerOwner: number
    retainedTuples: number
    retainedValues: number
    serializedRootBytes: number
    serializedRootWithoutAssetArraysBytes: number
    assetArrayBytes: number
    assetArrayPayloadShare: number
    stringifyDurationMs: number
}

function makeAssetTuples(ownerId: string): [string, string, string][] {
    return Array.from({ length: ASSET_TUPLES_PER_OWNER }, (_, index) => {
        const suffix = index.toString().padStart(6, '0')
        return [
            `${ownerId}-asset-${suffix}`,
            `assets/${ownerId}/${suffix}.webp`,
            'webp',
        ]
    })
}

export function makeAssetManifestFixture(): Database {
    const database = structuredClone(fixtureDatabase)
    database.modules = [{
        id: 'module-owner',
        name: 'Module owner',
        description: 'Deterministic large module asset owner',
        assets: makeAssetTuples('module-owner'),
    }]
    database.personas = [{
        id: 'persona-owner',
        name: 'Persona owner',
        personaPrompt: 'Deterministic large persona asset owner',
        icon: '',
        embeddedModule: {
            id: 'persona-module-owner',
            name: 'Persona module owner',
            description: 'Deterministic embedded module asset owner',
            assets: makeAssetTuples('persona-owner'),
        },
    }]
    database.characters[0].additionalAssets = makeAssetTuples('character-owner')
    return database
}

function withoutAssetTupleCollections(database: Database): Database {
    return {
        ...database,
        modules: (database.modules ?? []).map(({ assets: _assets, ...moduleValue }) => moduleValue),
        personas: (database.personas ?? []).map((persona) => ({
            ...persona,
            embeddedModule: persona.embeddedModule
                ? (({ assets: _assets, ...moduleValue }) => moduleValue)(persona.embeddedModule)
                : undefined,
        })),
        characters: database.characters.map((character) => {
            const { additionalAssets: _assets, ...characterValue } = character
            return characterValue
        }),
    } as Database
}

export function measureAssetManifestFixture(
    database: Database,
): AssetManifestFixtureMeasurement {
    const start = performance.now()
    const serializedRoot = JSON.stringify(database)
    const stringifyDurationMs = performance.now() - start
    const serializedRootWithoutAssetArrays = JSON.stringify(
        withoutAssetTupleCollections(database),
    )
    const serializedRootBytes = Buffer.byteLength(serializedRoot)
    const serializedRootWithoutAssetArraysBytes = Buffer.byteLength(
        serializedRootWithoutAssetArrays,
    )
    const retainedTuples = ASSET_TUPLES_PER_OWNER * ASSET_MANIFEST_OWNER_COUNT

    return {
        ownerCount: ASSET_MANIFEST_OWNER_COUNT,
        tuplesPerOwner: ASSET_TUPLES_PER_OWNER,
        retainedTuples,
        retainedValues: retainedTuples * 3,
        serializedRootBytes,
        serializedRootWithoutAssetArraysBytes,
        assetArrayBytes: serializedRootBytes - serializedRootWithoutAssetArraysBytes,
        assetArrayPayloadShare:
            (serializedRootBytes - serializedRootWithoutAssetArraysBytes) / serializedRootBytes,
        stringifyDurationMs,
    }
}

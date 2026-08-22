import { decompressSync } from 'fflate'
import { isColdStorageBackupData } from '../process/coldstorageData'
import type { BlobKeyValueBackend, BlobMetadata, BlobWriteMetadata } from './blobStore'
import type { RootedColdPayloadStoreFactory } from './coldPayloadStore'
import {
    createLosslessMigrationManifest,
    hashLosslessMigrationManifest,
    type DecodedLosslessMigrationEntry,
    type LosslessMigrationManifest,
    type LosslessMigrationManifestEntry,
} from './losslessMigrationPackage'
import type { RootedBlobStoreFactory } from './platformBlobStore'
import { assertGeneratedStorageRootId } from './storageRoot'

export interface MigrationPayloadSeal {
    version: 1
    payloadGeneration: string
    previousPayloadGeneration: string
    manifestHash: string
    manifest: LosslessMigrationManifest
}

export interface MigrationPayloadStage {
    readonly payloadGeneration: string
    put(entry: DecodedLosslessMigrationEntry): Promise<void>
    verifyEntry(entry: LosslessMigrationManifestEntry): Promise<void>
    readColdValue(id: string): Promise<unknown>
    seal(manifest: LosslessMigrationManifest, manifestHash: string): Promise<void>
    verifySeal(): Promise<MigrationPayloadSeal>
}

export interface MigrationPayloadStageFactory {
    create(previousPayloadGeneration: string): MigrationPayloadStage
    open(payloadGeneration: string, previousPayloadGeneration: string): MigrationPayloadStage
    readSeal(payloadGeneration: string): Promise<MigrationPayloadSeal | null>
    listGenerations(): Promise<string[]>
    removeGeneration(payloadGeneration: string, activePayloadGeneration: string): Promise<void>
}

const encoder = new TextEncoder()
const decoder = new TextDecoder('utf-8', { fatal: true })

async function sha256(data: Uint8Array): Promise<string> {
    const digest = await crypto.subtle.digest('SHA-256', data as BufferSource)
    return Array.from(new Uint8Array(digest), (value) => value.toString(16).padStart(2, '0')).join('')
}

function sealKey(generation: string): string {
    assertGeneratedStorageRootId(generation)
    return `blobstore/generations/${generation}/migration-manifest.json`
}

function generationPrefix(generation: string): string {
    assertGeneratedStorageRootId(generation)
    return `blobstore/generations/${generation}/`
}

function metadataMatches(actual: BlobMetadata, expected: LosslessMigrationManifestEntry): boolean {
    if (actual.key !== expected.id || actual.size !== expected.size || actual.kind !== expected.kind) return false
    const metadata = expected.metadata as BlobWriteMetadata
    return actual.mime === metadata.mime && actual.name === metadata.name && actual.ext === metadata.ext
        && (actual.kind !== 'inlay' || (metadata.kind === 'inlay'
            && actual.inlayType === metadata.inlayType
            && actual.width === metadata.width
            && actual.height === metadata.height))
}

function exactBytes(left: Uint8Array, right: Uint8Array): boolean {
    return left.byteLength === right.byteLength && left.every((value, index) => value === right[index])
}

function decodeCold(data: Uint8Array): unknown {
    let value: unknown
    try {
        value = JSON.parse(decoder.decode(decompressSync(data)))
    } catch {
        throw new Error('Cold migration payload is not valid compressed JSON')
    }
    if (!isColdStorageBackupData(value)) throw new Error('Cold migration payload has an unsupported value')
    return value
}

export function createMigrationPayloadStageFactory(input: {
    backend: BlobKeyValueBackend
    blobs: RootedBlobStoreFactory
    cold: RootedColdPayloadStoreFactory
    createGenerationId?: () => string
}): MigrationPayloadStageFactory {
    const allocate = input.createGenerationId ?? (() => crypto.randomUUID().replace(/-/g, ''))

    const readSeal = async (payloadGeneration: string): Promise<MigrationPayloadSeal | null> => {
        const bytes = await input.backend.read(sealKey(payloadGeneration))
        if (bytes === null) return null
        let raw: unknown
        try {
            raw = JSON.parse(decoder.decode(bytes))
        } catch {
            throw new Error('Migration payload seal is malformed')
        }
        if (!raw || typeof raw !== 'object') throw new Error('Migration payload seal is malformed')
        const value = raw as Record<string, unknown>
        if (Object.keys(value).sort().join(',') !== 'manifest,manifestHash,payloadGeneration,previousPayloadGeneration,version'
            || value.version !== 1 || value.payloadGeneration !== payloadGeneration
            || typeof value.previousPayloadGeneration !== 'string'
            || typeof value.manifestHash !== 'string'
            || !value.manifest || typeof value.manifest !== 'object') {
            throw new Error('Migration payload seal is malformed')
        }
        if (value.previousPayloadGeneration !== 'legacy') {
            assertGeneratedStorageRootId(value.previousPayloadGeneration)
        }
        const sourceManifest = value.manifest as LosslessMigrationManifest
        if (sourceManifest.version !== 1 || !Array.isArray(sourceManifest.entries)) {
            throw new Error('Migration payload seal manifest is malformed')
        }
        const manifest = createLosslessMigrationManifest(sourceManifest.entries)
        if (await hashLosslessMigrationManifest(manifest) !== value.manifestHash) {
            throw new Error('Migration payload seal hash mismatch')
        }
        return Object.freeze({
            version: 1,
            payloadGeneration,
            previousPayloadGeneration: value.previousPayloadGeneration,
            manifestHash: value.manifestHash,
            manifest,
        })
    }

    const open = (payloadGeneration: string, previousPayloadGeneration: string): MigrationPayloadStage => {
        assertGeneratedStorageRootId(payloadGeneration)
        if (previousPayloadGeneration !== 'legacy') assertGeneratedStorageRootId(previousPayloadGeneration)
        const root = { kind: 'generation' as const, id: payloadGeneration }
        const blobs = input.blobs.open(root)
        const cold = input.cold.open(root)
        const identities = new Set<string>()
        let sealed = false
        let sealStateChecked = false

        const requireMutable = async () => {
            if (sealed) throw new Error('Migration payload stage is sealed')
            if (!sealStateChecked) {
                sealed = await input.backend.read(sealKey(payloadGeneration)) !== null
                sealStateChecked = true
            }
            if (sealed) throw new Error('Migration payload stage is sealed')
        }

        const verifyEntry = async (entry: LosslessMigrationManifestEntry): Promise<void> => {
            if (entry.kind === 'database') return
            if (entry.kind === 'cold') {
                const data = await cold.read(entry.id)
                if (data === null) throw new Error(`Missing staged cold payload ${entry.id}`)
                decodeCold(data)
                if (data.byteLength !== entry.size || await sha256(data) !== entry.sha256) {
                    throw new Error(`Staged cold payload ${entry.id} does not match its manifest`)
                }
                return
            }
            const metadata = await blobs.stat(entry.id)
            const data = await blobs.read(entry.id)
            if (!metadata || data === null) throw new Error(`Missing staged ${entry.kind} ${entry.id}`)
            if (!metadataMatches(metadata, entry) || data.byteLength !== entry.size
                || await sha256(data) !== entry.sha256) {
                throw new Error(`Staged ${entry.kind} ${entry.id} does not match its manifest`)
            }
        }

        return {
            payloadGeneration,
            async put(entry) {
                await requireMutable()
                if (entry.kind === 'database') throw new Error('Database bytes are not payload-stage entries')
                const identity = `${entry.kind}\0${entry.id}`
                if (identities.has(identity)) throw new Error(`Duplicate staged migration entry ${entry.id}`)
                identities.add(identity)
                if (entry.kind === 'cold') {
                    decodeCold(entry.data)
                    await cold.write(entry.id, entry.data)
                } else {
                    await blobs.put(entry.id, entry.data, entry.metadata as BlobWriteMetadata)
                }
                await verifyEntry(entry)
                const reread = entry.kind === 'cold' ? await cold.read(entry.id) : await blobs.read(entry.id)
                if (reread === null || !exactBytes(reread, entry.data)) {
                    throw new Error(`Staged migration entry ${entry.id} failed exact read-back`)
                }
            },
            verifyEntry,
            async readColdValue(id) {
                const data = await cold.read(id)
                if (data === null) throw new Error(`Missing staged cold payload ${id}`)
                return decodeCold(data)
            },
            async seal(manifest, manifestHash) {
                await requireMutable()
                const canonical = createLosslessMigrationManifest(manifest.entries)
                if (await hashLosslessMigrationManifest(canonical) !== manifestHash) {
                    throw new Error('Migration manifest hash mismatch before sealing')
                }
                const declaredBlobs = canonical.entries
                    .filter((entry) => entry.kind === 'asset' || entry.kind === 'inlay')
                    .map((entry) => `${entry.kind}\0${entry.id}`).sort()
                const stagedBlobs = (await blobs.list()).map((entry) => `${entry.kind}\0${entry.key}`).sort()
                const declaredCold = canonical.entries.filter((entry) => entry.kind === 'cold').map((entry) => entry.id).sort()
                const stagedCold = await cold.list()
                if (JSON.stringify(declaredBlobs) !== JSON.stringify(stagedBlobs)
                    || JSON.stringify(declaredCold) !== JSON.stringify(stagedCold)) {
                    throw new Error('Staged payload list does not match declared migration entries')
                }
                for (const entry of canonical.entries) await verifyEntry(entry)
                const seal: MigrationPayloadSeal = {
                    version: 1,
                    payloadGeneration,
                    previousPayloadGeneration,
                    manifestHash,
                    manifest: canonical,
                }
                await input.backend.write(sealKey(payloadGeneration), encoder.encode(JSON.stringify(seal)))
                sealed = true
                sealStateChecked = true
                await this.verifySeal()
            },
            async verifySeal() {
                const seal = await readSeal(payloadGeneration)
                if (!seal || seal.previousPayloadGeneration !== previousPayloadGeneration) {
                    throw new Error('Migration payload seal does not match its stage')
                }
                sealed = true
                sealStateChecked = true
                return seal
            },
        }
    }

    return {
        create(previousPayloadGeneration) {
            const id = allocate()
            assertGeneratedStorageRootId(id)
            if (id === 'legacy') throw new TypeError('Generated migration root cannot use the reserved legacy identifier')
            return open(id, previousPayloadGeneration)
        },
        open,
        readSeal,
        async listGenerations() {
            const prefix = 'blobstore/generations/'
            const generations = new Set<string>()
            for (const key of await input.backend.keys()) {
                if (!key.startsWith(prefix)) continue
                const id = key.slice(prefix.length).split('/')[0]
                try {
                    assertGeneratedStorageRootId(id)
                    generations.add(id)
                } catch {}
            }
            return [...generations].sort()
        },
        async removeGeneration(payloadGeneration, activePayloadGeneration) {
            assertGeneratedStorageRootId(payloadGeneration)
            if (payloadGeneration === activePayloadGeneration) {
                throw new Error('Cannot remove the active payload generation')
            }
            const prefix = generationPrefix(payloadGeneration)
            for (const key of await input.backend.keys()) {
                if (key.startsWith(prefix)) await input.backend.remove(key)
            }
        },
    }
}

import type { BlobStore, BlobWriteMetadata, InlayBlobMetadata } from '../storage/blobStore'
import type { Database, botPreset } from '../storage/database.svelte'
import type { PersistentRevisionReader, PersistentRoot } from '../storage/persistentDataStore'
import type {
    PinnedCharacterRecord,
    PinnedConversationRecord,
} from '../storage/persistentRecordIterator'
import {
    assertPinnedRevision,
    iteratePinnedCharacters,
    iteratePinnedConversations,
} from '../storage/persistentRecordIterator'
import { readActiveAsset, storeActiveAsset } from '../storage/accountAssetAccess'
import { listCharacterResources, listDatabaseRootResources } from '../process/coldstorageData'

const INLAY_ENTRY_NAME = /^inlay_((?:[0-9a-f]{2})+)\.risuinlay$/
const INLAY_TYPES = new Set(['image', 'video', 'audio', 'signature'])

export function isLegacyBackupAssetKey(key: string): boolean {
    const normalized = key.replace(/\\/g, '/')
    return normalized.startsWith('assets/') && normalized.length > 'assets/'.length
}

function normalizeExactLegacyAssetReference(value: string): string | null {
    return isLegacyBackupAssetKey(value) ? value.replace(/\\/g, '/') : null
}

function normalizeExactPluginStorageBackupAssetReference(value: string): string | null {
    const normalized = normalizeExactLegacyAssetReference(value)
    if (!normalized) return null
    const segment = normalized.slice('assets/'.length)
    if (segment.includes('/') || segment === '.' || segment === '..') return null
    return normalized
}

export function collectExactPluginStorageAssetReferences(value: unknown): string[] {
    const references = new Set<string>()
    const seen = new WeakSet<object>()
    const visit = (candidate: unknown): void => {
        if (typeof candidate === 'string') {
            const normalized = normalizeExactPluginStorageBackupAssetReference(candidate)
            if (normalized) references.add(normalized)
            return
        }
        if (!candidate || typeof candidate !== 'object' || seen.has(candidate)) return
        seen.add(candidate)
        for (const child of Object.values(candidate)) visit(child)
    }
    visit(value)
    return [...references].sort()
}

export function replaceExactPluginStorageAssetReferences<T>(
    value: T,
    replacements: Readonly<Record<string, string>>,
): T {
    if (Object.keys(replacements).length === 0) return value
    const seen = new WeakMap<object, unknown>()
    const project = (candidate: unknown): unknown => {
        if (typeof candidate === 'string') {
            const normalized = normalizeExactLegacyAssetReference(candidate)
            if (!normalized) return candidate
            for (const key of candidate === normalized
                ? [candidate]
                : [candidate, normalized]) {
                if (Object.prototype.hasOwnProperty.call(replacements, key)) {
                    return replacements[key]
                }
            }
            return candidate
        }
        if (!candidate || typeof candidate !== 'object') return candidate
        const existing = seen.get(candidate)
        if (existing) return existing
        if (Array.isArray(candidate)) {
            const projected: unknown[] = []
            seen.set(candidate, projected)
            for (const child of candidate) projected.push(project(child))
            return projected
        }
        const projected: Record<string, unknown> = {}
        seen.set(candidate, projected)
        for (const key of Object.keys(candidate)) {
            Object.defineProperty(projected, key, {
                configurable: true,
                enumerable: true,
                value: project((candidate as Record<string, unknown>)[key]),
                writable: true,
            })
        }
        return projected
    }
    return project(value) as T
}

export function selectLegacyBackupAssetKeys(keys: readonly string[]): string[] {
    return keys.filter(isLegacyBackupAssetKey)
}

export async function collectBackupAssetKeys(
    _store: BlobStore,
    referencedKeys: Iterable<string>,
): Promise<string[]> {
    const keys = new Set<string>()
    for (const key of referencedKeys) {
        if (isLegacyBackupAssetKey(key)) keys.add(key.replace(/\\/g, '/'))
    }
    return Array.from(keys).sort()
}

export function collectPinnedBackupAssetReferences(
    database: Database,
    coldPayloadValues: readonly unknown[],
): string[] {
    const coldCharacters = new Map<string, Database['characters'][number]>()
    for (const value of coldPayloadValues) {
        if (!value || typeof value !== 'object' || Array.isArray(value) || !('character' in value)) {
            continue
        }
        const character = value.character
        if (!character || typeof character !== 'object' || !('chaId' in character)
            || typeof character.chaId !== 'string') {
            continue
        }
        coldCharacters.set(character.chaId, character as Database['characters'][number])
    }

    const references = new Set<string>()
    const add = (key: string) => {
        if (isLegacyBackupAssetKey(key)) references.add(key.replace(/\\/g, '/'))
    }
    for (const key of listDatabaseRootResources(database)) add(key)
    for (const character of database.characters) {
        for (const key of listCharacterResources(
            coldCharacters.get(character.chaId) ?? character,
        )) add(key)
    }
    return Array.from(references).sort()
}

function collectInlayReferences(value: unknown, references: Set<string>, seen: WeakSet<object>): void {
    if (typeof value === 'string') {
        for (const match of value.matchAll(/\{\{(?:inlay|inlayed|inlayeddata)::(.+?)\}\}/g)) {
            references.add(match[1])
        }
        return
    }
    if (!value || typeof value !== 'object' || seen.has(value)) return
    seen.add(value)
    for (const child of Object.values(value)) collectInlayReferences(child, references, seen)
}

export async function collectReferencedBackupInlays(
    store: BlobStore,
    referencedKeys: Iterable<string>,
): Promise<InlayBlobMetadata[]> {
    const references = new Set(referencedKeys)
    return (await store.list({ kind: 'inlay' })).filter(
        (metadata): metadata is InlayBlobMetadata =>
            metadata.kind === 'inlay'
            && !isLegacyBackupAssetKey(metadata.key)
            && references.has(metadata.key),
    )
}

export type PinnedBackupReferenceMode = 'full' | 'partial'

export interface ColdCharacterReference {
    characterId: string
    characterName: string
    keys: string[]
}

export interface PinnedBackupReferences {
    assetKeys: string[]
    inlayKeys: string[]
    coldKeys: string[]
    assetLabels: ReadonlyMap<string, { charName: string; assetName: string }>
    coldCharacterReferences: ColdCharacterReference[]
}

export interface PinnedBackupReferenceAccumulator {
    visitRoot(root: PersistentRoot): void
    visitPreset(preset: botPreset): void
    visitCharacter(record: PinnedCharacterRecord): void
    visitConversation(record: PinnedConversationRecord): void
    visitPluginStorage(value: unknown): void
    visitColdPayload(value: unknown): void
    finish(): PinnedBackupReferences
}

function addInlayReferences(value: unknown, references: Set<string>): void {
    collectInlayReferences(value, references, new WeakSet())
}

function coldPointer(messageData: unknown): string | null {
    const header = '\uEF01COLDSTORAGE\uEF01'
    return typeof messageData === 'string' && messageData.startsWith(header)
        ? messageData.slice(header.length)
        : null
}

export function createPinnedBackupReferenceAccumulator(
    mode: PinnedBackupReferenceMode,
): PinnedBackupReferenceAccumulator {
    const rootAssets = new Set<string>()
    const partialUserAssets = new Set<string>()
    const partialPersonaAssets = new Set<string>()
    const partialBackgroundAssets = new Set<string>()
    const partialFolderAssets = new Set<string>()
    const partialPresetAssets = new Set<string>()
    const characterAssets = new Map<string, Set<string>>()
    type AssetLabel = { charName: string; assetName: string }
    const characterLabels = new Map<string, AssetLabel>()
    const userLabels = new Map<string, AssetLabel>()
    const personaLabels = new Map<string, AssetLabel>()
    const backgroundLabels = new Map<string, AssetLabel>()
    const folderLabels = new Map<string, AssetLabel>()
    const presetLabels = new Map<string, AssetLabel>()
    const inlayKeys = new Set<string>()
    const coldKeys = new Set<string>()
    const coldCharacters = new Map<string, ColdCharacterReference>()
    let currentCharacterId: string | undefined
    let currentCharacterName: string | undefined

    const addAsset = (
        target: Set<string>,
        key: string | undefined,
        label?: AssetLabel,
        labels?: Map<string, AssetLabel>,
    ) => {
        if (!key || !isLegacyBackupAssetKey(key)) return
        const normalized = key.replace(/\\/g, '/')
        target.add(mode === 'full' ? normalized : key)
        if (label) labels?.set(key, label)
    }
    const addColdKey = (
        characterId: string,
        characterName: string,
        key: string | undefined,
    ) => {
        if (key === undefined) return
        coldKeys.add(key)
        let character = coldCharacters.get(characterId)
        if (!character) {
            character = {
                characterId,
                characterName,
                keys: [],
            }
            coldCharacters.set(characterId, character)
        }
        if (!character.keys.includes(key)) character.keys.push(key)
    }
    const addCharacterAssets = (
        id: string,
        value: Database['characters'][number],
        partial: boolean,
        includeLabels: boolean,
    ) => {
        const assets = new Set<string>()
        const charName = value.name ?? 'Unknown Character'
        const label = (assetName: string) => includeLabels ? { charName, assetName } : undefined
        addAsset(
            assets,
            value.image,
            label(partial ? 'Profile Image' : 'Main Image'),
            characterLabels,
        )
        if (!partial) {
            for (const item of value.emotionImages ?? []) {
                addAsset(
                    assets,
                    item?.[1],
                    label(item?.[0] ?? 'Emotion Image'),
                    characterLabels,
                )
            }
            if (value.type !== 'group') {
                for (const item of value.additionalAssets ?? []) {
                    addAsset(
                        assets,
                        item?.[1],
                        label(item?.[0] ?? 'Asset'),
                        characterLabels,
                    )
                }
                for (const [name, key] of Object.entries(value.vits?.files ?? {})) {
                    addAsset(assets, key, label(name), characterLabels)
                }
                for (const item of value.ccAssets ?? []) {
                    addAsset(
                        assets,
                        item?.uri,
                        label(item?.name ?? 'Asset'),
                        characterLabels,
                    )
                }
            }
        }
        if (assets.size > 0) characterAssets.set(id, assets)
        else characterAssets.delete(id)
    }

    return {
        visitRoot(root) {
            if (mode === 'full') {
                for (const key of listDatabaseRootResources(root)) addAsset(rootAssets, key)
                addAsset(rootAssets, root.userIcon, {
                    charName: 'User Settings', assetName: 'User Icon',
                }, userLabels)
                addAsset(rootAssets, root.customBackground, {
                    charName: 'User Settings', assetName: 'Custom Background',
                }, backgroundLabels)
                addInlayReferences(root, inlayKeys)
                return
            }
            addAsset(partialUserAssets, root.userIcon, {
                charName: 'User Settings', assetName: 'User Icon',
            }, userLabels)
            for (const persona of root.personas ?? []) {
                addAsset(partialPersonaAssets, persona?.icon, {
                    charName: 'Persona', assetName: `${persona?.name} Icon`,
                }, personaLabels)
            }
            addAsset(partialBackgroundAssets, root.customBackground, {
                charName: 'User Settings', assetName: 'Custom Background',
            }, backgroundLabels)
            for (const item of root.characterOrder ?? []) {
                if (typeof item === 'string') continue
                const label = { charName: 'Folder', assetName: `${item.name} Folder Image` }
                addAsset(partialFolderAssets, item.img, label, folderLabels)
                addAsset(partialFolderAssets, item.imgFile, {
                    ...label,
                    assetName: `${item.name} Folder Image File`,
                }, folderLabels)
            }
        },
        visitPreset(preset) {
            if (mode === 'full') {
                addInlayReferences(preset, inlayKeys)
                return
            }
            addAsset(partialPresetAssets, preset.image, {
                charName: 'Preset', assetName: `${preset.name} Preset Image`,
            }, presetLabels)
        },
        visitCharacter(record) {
            currentCharacterId = record.summary.id
            currentCharacterName = record.summary.name
            const character = {
                ...record.detail,
                chats: [],
            } as Database['characters'][number]
            addCharacterAssets(record.summary.id, character, mode === 'partial', true)
            if (character.coldstorage) {
                addColdKey(record.summary.id, record.summary.name, character.coldstorage)
            }
            for (const key of character.coldStoragedChats ?? []) {
                addColdKey(record.summary.id, record.summary.name, key)
            }
            if (mode === 'full') addInlayReferences(record.detail, inlayKeys)
        },
        visitConversation(record) {
            const key = coldPointer(record.value.message?.[0]?.data)
            const characterName = currentCharacterId === record.summary.characterId
                ? currentCharacterName ?? record.summary.characterId
                : record.summary.characterId
            addColdKey(record.summary.characterId, characterName, key ?? undefined)
            if (mode === 'full') {
                addInlayReferences(record.value, inlayKeys)
                for (const key of collectExactPluginStorageAssetReferences(record.value)) rootAssets.add(key)
            }
        },
        visitPluginStorage(value) {
            if (mode !== 'full') return
            for (const key of collectExactPluginStorageAssetReferences(value)) {
                rootAssets.add(key)
            }
            addInlayReferences(value, inlayKeys)
        },
        visitColdPayload(value) {
            if (mode !== 'full') return
            addInlayReferences(value, inlayKeys)
            for (const key of collectExactPluginStorageAssetReferences(value)) rootAssets.add(key)
            if (
                value
                && typeof value === 'object'
                && !Array.isArray(value)
                && 'character' in value
                && value.character
                && typeof value.character === 'object'
                && 'chaId' in value.character
                && typeof value.character.chaId === 'string'
            ) {
                addCharacterAssets(
                    value.character.chaId,
                    value.character as Database['characters'][number],
                    false,
                    false,
                )
            }
        },
        finish() {
            const assets = new Set(rootAssets)
            const assetLabels = new Map<string, AssetLabel>()
            if (mode === 'partial') {
                for (const values of characterAssets.values()) {
                    for (const key of values) assets.add(key)
                }
                for (const values of [
                    partialUserAssets,
                    partialPersonaAssets,
                    partialBackgroundAssets,
                    partialFolderAssets,
                    partialPresetAssets,
                ]) {
                    for (const key of values) assets.add(key)
                }
                for (const labels of [
                    characterLabels,
                    userLabels,
                    personaLabels,
                    backgroundLabels,
                    folderLabels,
                    presetLabels,
                ]) {
                    for (const [key, label] of labels) assetLabels.set(key, label)
                }
            } else {
                for (const values of characterAssets.values()) {
                    for (const key of values) assets.add(key)
                }
                for (const labels of [characterLabels, userLabels, backgroundLabels]) {
                    for (const [key, label] of labels) assetLabels.set(key, label)
                }
            }
            return {
                assetKeys: mode === 'full' ? Array.from(assets).sort() : Array.from(assets),
                inlayKeys: Array.from(inlayKeys).sort(),
                coldKeys: Array.from(coldKeys),
                assetLabels,
                coldCharacterReferences: Array.from(coldCharacters.values()),
            }
        },
    }
}

export async function scanPinnedBackupRecords(
    reader: PersistentRevisionReader,
    mode: PinnedBackupReferenceMode,
): Promise<{
    root: PersistentRoot
    accumulator: PinnedBackupReferenceAccumulator
}> {
    const accumulator = createPinnedBackupReferenceAccumulator(mode)
    const root = await reader.readRoot()
    assertPinnedRevision(reader.revision, root.revision, 'Root')
    accumulator.visitRoot(root.value)

    const presets = await reader.queryPresets()
    assertPinnedRevision(reader.revision, presets.revision, 'Preset catalog')
    for (const summary of presets.items) {
        const preset = await reader.readPreset(summary.id)
        if (!preset) throw new Error(`Missing preset ${summary.id}`)
        assertPinnedRevision(reader.revision, preset.revision, `Preset ${summary.id}`)
        accumulator.visitPreset(preset.value)
    }

    if (mode === 'full') {
        const pluginStorage = await reader.queryPluginStorage()
        assertPinnedRevision(reader.revision, pluginStorage.revision, 'Plugin storage catalog')
        for (const summary of pluginStorage.items) {
            const value = await reader.readPluginStorage(summary.key)
            if (!value) throw new Error(`Missing plugin storage value for ${summary.key}`)
            assertPinnedRevision(
                reader.revision,
                value.revision,
                `Plugin storage value ${summary.key}`,
            )
            accumulator.visitPluginStorage(value.value)
        }
    }

    for await (const character of iteratePinnedCharacters(reader)) {
        accumulator.visitCharacter(character)
        for await (const conversation of iteratePinnedConversations(
            reader,
            character.summary.id,
        )) {
            accumulator.visitConversation(conversation)
        }
    }
    return { root: root.value, accumulator }
}

export function createColdStorageReferenceDatabase(
    references: readonly ColdCharacterReference[],
): Pick<Database, 'characters'> {
    return {
        characters: references.map((reference) => ({
            chaId: reference.characterId,
            name: reference.characterName,
            type: 'character',
            coldStoragedChats: reference.keys,
            chats: [],
        })) as Database['characters'],
    }
}

export function readBackupAsset(
    store: BlobStore,
    key: string,
    officialAccount: boolean,
): Promise<Uint8Array | null> {
    return readActiveAsset(store, key, { officialAccount, tauri: false })
}

export async function writeBackupAsset(
    store: BlobStore,
    key: string,
    data: Uint8Array,
): Promise<void> {
    const name = key.replace(/\\/g, '/').split('/').pop() ?? key
    await storeActiveAsset(store, key, data, {
        kind: 'asset',
        mime: '',
        name,
        ext: name.split('.').pop() ?? '',
    })
}

export interface BackupInlayEntry {
    key: string
    metadata: BlobWriteMetadata
    data: Uint8Array
}

/** Hex keeps the id single segment, so the basename the writer applies cannot truncate it. */
export function getBackupInlayName(key: string): string {
    return `inlay_${Buffer.from(key, 'utf-8').toString('hex')}.risuinlay`
}

export function encodeBackupInlayEntry(metadata: InlayBlobMetadata, data: Uint8Array): Uint8Array {
    const header = new TextEncoder().encode(JSON.stringify(metadata))
    const entry = new Uint8Array(4 + header.byteLength + data.byteLength)
    new DataView(entry.buffer).setUint32(0, header.byteLength, true)
    entry.set(header, 4)
    entry.set(data, 4 + header.byteLength)
    return entry
}

export function decodeBackupInlayEntry(name: string, entry: Uint8Array): BackupInlayEntry | null {
    if (!INLAY_ENTRY_NAME.test(name) || entry.byteLength < 4) return null
    const headerLength = new DataView(entry.buffer, entry.byteOffset, entry.byteLength).getUint32(0, true)
    if (headerLength === 0 || 4 + headerLength > entry.byteLength) return null
    let header: unknown
    try {
        header = JSON.parse(new TextDecoder().decode(entry.subarray(4, 4 + headerLength)))
    } catch {
        return null
    }
    if (!header || typeof header !== 'object' || Array.isArray(header)) return null
    const { key, size: _size, ...metadata } = header as Partial<InlayBlobMetadata>
    if (typeof key !== 'string' || key === '' || isLegacyBackupAssetKey(key)) return null
    if (metadata.kind !== 'inlay' || !INLAY_TYPES.has(metadata.inlayType as string)) return null
    if (typeof metadata.mime !== 'string' || typeof metadata.name !== 'string'
        || typeof metadata.ext !== 'string') {
        return null
    }
    return {
        key,
        metadata: metadata as BlobWriteMetadata,
        data: entry.slice(4 + headerLength),
    }
}

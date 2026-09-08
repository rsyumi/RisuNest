import {
    selectPluginCompatibilityProfile,
    type PluginCompatibilityProfile,
} from '../plugins/pluginCompatibility'
import type { Database, botPreset } from './database.svelte'
import type { PreparedBootstrapDatabase } from './databasePreparation'
import {
    RevisionConflictError,
    type CharacterSummary,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRoot,
    type PresetCatalog,
    type PresetSummary,
} from './persistentDataStore'
import { canonicalJson } from './saveCoordinator'
import { removeCharacterIdFromOrder } from './characterOrderMutation'
import { yieldToMainThread } from '../ui/yieldToUi'

const BOOTSTRAP_CATALOG_PAGE_SIZE = 200
const TRASH_EXPIRY_MS = 3 * 24 * 60 * 60 * 1000
const NEW_DATABASE_SEED: Partial<Database> = {
    streamingDisplayOptimizationMode: 'balanced',
}

export interface PersistentBootstrapDependencies {
    store: PersistentDataStore
    onPhase?(
        phase: 'storage' | 'data' | 'compatibility',
        language?: string,
    ): void | Promise<void>
    prepareDatabase(database: Database): Promise<PreparedBootstrapDatabase>
    prepareRoot?(root: PersistentRoot): Promise<PersistentRoot>
    projectScalableWorkingSet?(input: ScalableBootstrapProjection): Database
    now?(): number
}

export interface ScalableBootstrapProjection {
    root: PersistentRoot
    characters: CharacterSummary[]
    presetCatalog: PresetCatalog
    activePreset: {
        summary: PresetSummary
        value: botPreset
    } | null
}

export interface PersistentBootstrapResult {
    database: Database
    revision: DataRevision
    profile: PluginCompatibilityProfile
}

/**
 * Opens the authoritative local store. A store that has never been written starts from an empty
 * database, because existing RisuAI data only enters the app through backup import.
 */
export async function bootstrapPersistentDatabase(
    dependencies: PersistentBootstrapDependencies,
): Promise<PersistentBootstrapResult> {
    await dependencies.onPhase?.('storage')
    await dependencies.store.open()
    const active = await dependencies.store.readRoot()
    await dependencies.onPhase?.('data', active.value.language)

    if (active.revision === 0) {
        const { database } = await dependencies.prepareDatabase({ ...NEW_DATABASE_SEED } as Database)
        const { revision } = await dependencies.store.replaceFromDatabase(database, 0)
        const profile = selectPluginCompatibilityProfile(database.plugins ?? [])
        if (profile === 'scalable-v3' && dependencies.projectScalableWorkingSet) {
            const {
                characters: _characters,
                botPresets: _botPresets,
                pluginCustomStorage: _pluginCustomStorage,
                ...storedRoot
            } = database
            const root = await canonicalizePresetSelection(
                dependencies.store,
                revision,
                storedRoot,
            )
            const projectedRevision = canonicalJson(root) === canonicalJson(storedRoot)
                ? revision
                : (await dependencies.store.commit({
                    expectedRevision: revision,
                    root,
                })).revision
            const projected = await projectScalableRevision(
                dependencies,
                root,
                projectedRevision,
            )
            return { ...projected, profile }
        }
        return { database, revision, profile }
    }

    const profile = selectPluginCompatibilityProfile(active.value.plugins ?? [])
    if (
        profile === 'maximum-compatibility' ||
        !dependencies.prepareRoot ||
        !dependencies.projectScalableWorkingSet
    ) {
        await dependencies.onPhase?.('compatibility', active.value.language)
        const persistent = await dependencies.store.materializeDatabase(active.revision)
        const { database, changed } = await dependencies.prepareDatabase(persistent)
        if (!changed) {
            return { database, revision: active.revision, profile }
        }
        const { revision } = await dependencies.store.replaceFromDatabase(
            database,
            active.revision,
        )
        return { database, revision, profile }
    }

    const preparedRoot = await dependencies.prepareRoot(active.value)
    const root = await canonicalizePresetSelection(
        dependencies.store,
        active.revision,
        preparedRoot,
    )
    let revision = active.revision
    if (canonicalJson(root) !== canonicalJson(active.value)) {
        revision = (await dependencies.store.commit({
            expectedRevision: revision,
            root,
        })).revision
    }

    const projected = await projectScalableRevision(dependencies, root, revision)
    return { ...projected, profile }
}

async function canonicalizePresetSelection(
    store: PersistentDataStore,
    revision: DataRevision,
    root: PersistentRoot,
): Promise<PersistentRoot> {
    const catalog = await store.queryPresets()
    assertRevision(revision, catalog.revision)
    if (
        root.botPresetsId < 0 ||
        catalog.items.some((item) => item.configuredIndex === root.botPresetsId)
    ) {
        return root
    }
    const first = [...catalog.items].sort(
        (left, right) => left.configuredIndex - right.configuredIndex,
    )[0]
    return {
        ...root,
        botPresetsId: first?.configuredIndex ?? 0,
    }
}

async function projectScalableRevision(
    dependencies: PersistentBootstrapDependencies,
    root: PersistentRoot,
    revision: DataRevision,
): Promise<{ database: Database; revision: DataRevision }> {
    let currentRoot = root
    let currentRevision = revision
    let characters = await queryAllCharacterSummaries(dependencies.store, currentRevision)
    const expiryCutoff = (dependencies.now?.() ?? Date.now()) - TRASH_EXPIRY_MS
    const expiredIds = characters
        .filter((summary) => summary.trashTime !== undefined && summary.trashTime < expiryCutoff)
        .map((summary) => summary.id)
    for (const characterId of expiredIds) {
        const nextRoot = structuredClone(currentRoot)
        removeCharacterIdFromOrder(nextRoot, characterId)
        const commit = {
            expectedRevision: currentRevision,
            deleteCharacterId: characterId,
        } as Parameters<PersistentDataStore['commit']>[0]
        if (canonicalJson(nextRoot) !== canonicalJson(currentRoot)) commit.root = nextRoot
        const result = await dependencies.store.commit(commit)
        currentRoot = nextRoot
        currentRevision = result.revision
    }
    if (expiredIds.length > 0) {
        characters = await queryAllCharacterSummaries(dependencies.store, currentRevision)
    }
    const presets = await readSelectedPreset(
        dependencies.store,
        currentRevision,
        currentRoot.botPresetsId,
    )
    return {
        revision: currentRevision,
        database: dependencies.projectScalableWorkingSet!({
        root: currentRoot,
        characters,
        presetCatalog: presets.catalog,
        activePreset: presets.active,
        }),
    }
}

async function queryAllCharacterSummaries(
    store: PersistentDataStore,
    revision: DataRevision,
): Promise<CharacterSummary[]> {
    const characters: CharacterSummary[] = []
    const trashStates = [false, true] as const
    for (const [trashIndex, trash] of trashStates.entries()) {
        let cursor: string | undefined
        do {
            const page = await store.queryCharacters({
                order: 'configured',
                trash,
                limit: BOOTSTRAP_CATALOG_PAGE_SIZE,
                cursor,
            })
            assertRevision(revision, page.revision)
            characters.push(...page.items)
            cursor = page.nextCursor
            const hasAnotherPage =
                cursor !== undefined || trashIndex < trashStates.length - 1
            if (hasAnotherPage) await yieldToMainThread()
        } while (cursor !== undefined)
    }
    return characters
}

async function readSelectedPreset(
    store: PersistentDataStore,
    revision: DataRevision,
    configuredIndex: number,
): Promise<{
    catalog: PresetCatalog
    active: ScalableBootstrapProjection['activePreset']
}> {
    const catalog = await store.queryPresets()
    assertRevision(revision, catalog.revision)
    const summary = catalog.items.find((item) => item.configuredIndex === configuredIndex)
    if (!summary) return { catalog, active: null }
    const preset = await store.readPreset(summary.id)
    if (!preset) throw new Error(`Preset ${summary.id} was not found`)
    assertRevision(revision, preset.revision)
    return {
        catalog,
        active: { summary, value: preset.value },
    }
}

function assertRevision(expected: DataRevision, actual: DataRevision): void {
    if (actual !== expected) throw new RevisionConflictError(expected, actual)
}

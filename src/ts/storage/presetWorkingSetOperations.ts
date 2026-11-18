import type { Database, botPreset } from './database.svelte'
import {
    RevisionConflictError,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRoot,
} from './persistentDataStore'
import { safeStructuredClone } from '../safeStructuredClone'
import { v4 } from 'uuid'
import { presetMirrorMap, protectedPresetGroups, type ProtectedPresetField } from './effectiveIdentityState'

export class PresetListChangedError extends Error {
    constructor() { super('Preset list changed'); this.name = 'PresetListChangedError' }
}

export interface PresetListSnapshot {
    names: readonly string[]
    presets: readonly Pick<botPreset, 'name'>[]
}

export type PresetListExpectation = readonly string[] | PresetListSnapshot

export function capturePresetNames(presets: readonly Pick<botPreset, 'name'>[]): PresetListSnapshot {
    return { names: presets.map(preset => preset.name), presets: [...presets] }
}

function presetNames(expected: PresetListExpectation): readonly string[] {
    return 'names' in expected ? expected.names : expected
}

export function assertPresetNames(presets: readonly Pick<botPreset, 'name'>[], expected: PresetListExpectation): void {
    const names = presetNames(expected)
    if (presets.length !== names.length || presets.some((preset, index) => preset.name !== names[index]
        || ('presets' in expected && preset !== expected.presets[index]))) throw new PresetListChangedError()
}

export interface PresetWorkingSetMutationState {
    root: Omit<PersistentRoot, 'botPresetsId' | 'selectedPersona'> & Pick<Database, 'botPresetsId' | 'selectedPersona'>
    presets: botPreset[]
}

export interface PresetWorkingSetControllerDependencies {
    getDatabase(): Database
    captureCurrentPreset(database: Database): botPreset | null
    applyPreset(root: PersistentRoot, preset: botPreset): void
    getEffectivePresetId?(): string | undefined
    getEffectivePresetOverride?(): string | null
    clearEffectivePresetOverride?(): void
    mutatePersistentPresets(
        reason: string,
        mutate: (state: PresetWorkingSetMutationState) => void | Promise<void>,
    ): Promise<void>
}

export interface PresetWorkingSetController {
    saveCurrentPreset(expectedNames?: PresetListExpectation): Promise<void>
    changeToPreset(id?: number, saveCurrent?: boolean, expectedNames?: PresetListExpectation): Promise<void>
    addPreset(preset: botPreset, activate?: boolean): Promise<number>
    copyPreset(id: number, expectedNames?: PresetListExpectation): Promise<number>
    removePreset(id: number, expectedNames?: PresetListExpectation): Promise<void>
    movePreset(fromIndex: number, toIndex: number, expectedNames?: PresetListExpectation): Promise<void>
    renamePreset(id: number, name: string, expectedNames?: PresetListExpectation): Promise<void>
    updateActivePresetImage(image: string): Promise<void>
    clearPluginProvider(owner: string): Promise<void>
}

function clonePreset(preset: botPreset): botPreset {
    return safeStructuredClone(preset)
}

function assertPresetIndex(presets: readonly botPreset[], id: number): void {
    if (!Number.isInteger(id) || id < 0 || id >= presets.length) {
        throw new Error(`Preset index ${id} is out of bounds`)
    }
}

export async function readPersistentPresetBody(
    store: Pick<PersistentDataStore, 'queryPresets' | 'readPreset'>,
    revision: DataRevision,
    configuredIndex: number,
): Promise<botPreset> {
    return (await readPersistentPresetBodies(store, revision, [configuredIndex]))[0]
}

export async function readPersistentPresetBodies(
    store: Pick<PersistentDataStore, 'queryPresets' | 'readPreset'>,
    revision: DataRevision,
    configuredIndices: readonly number[],
): Promise<botPreset[]> {
    const catalog = await store.queryPresets()
    if (catalog.revision !== revision) {
        throw new RevisionConflictError(revision, catalog.revision)
    }
    return Promise.all(configuredIndices.map(async (configuredIndex) => {
        const summary = catalog.items.find((item) => item.configuredIndex === configuredIndex)
        if (!summary) throw new Error(`Preset index ${configuredIndex} is out of bounds`)
        const preset = await store.readPreset(summary.id)
        if (!preset) throw new Error(`Preset ${summary.id} was not found`)
        if (preset.revision !== revision) {
            throw new RevisionConflictError(revision, preset.revision)
        }
        return clonePreset(preset.value)
    }))
}

export function createPresetWorkingSetController(
    dependencies: PresetWorkingSetControllerDependencies,
): PresetWorkingSetController {
    const saveActive = (state: PresetWorkingSetMutationState): void => {
        const database = dependencies.getDatabase()
        const effectiveId = dependencies.getEffectivePresetId?.()
        const activeIndex = effectiveId ? state.presets.findIndex(preset => preset.id === effectiveId) : database.botPresetsId
        if (!Number.isInteger(activeIndex) || activeIndex < 0) return
        assertPresetIndex(state.presets, activeIndex)
        const captured = dependencies.captureCurrentPreset(database)
        if (captured) {
            const record = state.presets[activeIndex] as unknown as Record<string, unknown>
            const value = captured as unknown as Record<string, unknown>
            for (const field of Object.values(presetMirrorMap)) {
                const flag = protectedPresetGroups[field as ProtectedPresetField]
                if (flag && database[flag]) continue
                if (Object.hasOwn(value, field)) record[field] = safeStructuredClone(value[field])
                else delete record[field]
            }
        }
    }

    const select = (state: PresetWorkingSetMutationState, id: number): void => {
        assertPresetIndex(state.presets, id)
        state.root.botPresetsId = id
        dependencies.applyPreset(state.root, state.presets[id])
    }

    return {
        saveCurrentPreset: (expectedNames) => dependencies.mutatePersistentPresets(
            'save-current-preset',
            (state) => {
                if (expectedNames) assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                saveActive(state)
            },
        ),
        async changeToPreset(id = 0, saveCurrent = true, expectedNames = capturePresetNames(dependencies.getDatabase().botPresets)) {
            const database = dependencies.getDatabase()
            assertPresetNames(database.botPresets, expectedNames)
            assertPresetIndex(database.botPresets, id)
            // A conversation's binding may apply another preset; the list still changes the selection.
            if (database.botPresetsId === id && !dependencies.getEffectivePresetOverride?.()) return
            const before = capturePresetNames(database.botPresets)
            await dependencies.mutatePersistentPresets('change-preset', (state) => {
                assertPresetNames(dependencies.getDatabase().botPresets, before)
                assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                assertPresetNames(state.presets, presetNames(expectedNames))
                if (saveCurrent) saveActive(state)
                select(state, id)
            })
            dependencies.clearEffectivePresetOverride?.()
        },
        async addPreset(preset, activate = false) {
            let addedIndex = -1
            await dependencies.mutatePersistentPresets('add-preset', (state) => {
                saveActive(state)
                addedIndex = state.presets.length
                state.presets.push({ ...clonePreset(preset), id: v4() })
                if (activate) select(state, addedIndex)
            })
            if (activate) dependencies.clearEffectivePresetOverride?.()
            return addedIndex
        },
        async copyPreset(id, expectedNames = capturePresetNames(dependencies.getDatabase().botPresets)) {
            const before = capturePresetNames(dependencies.getDatabase().botPresets)
            let addedIndex = -1
            await dependencies.mutatePersistentPresets('copy-preset', (state) => {
                assertPresetNames(dependencies.getDatabase().botPresets, before)
                assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                assertPresetNames(state.presets, presetNames(expectedNames))
                saveActive(state)
                assertPresetIndex(state.presets, id)
                const copied = clonePreset(state.presets[id])
                copied.id = v4()
                copied.name = `${copied.name} Copy`
                addedIndex = state.presets.length
                state.presets.push(copied)
            })
            return addedIndex
        },
        async removePreset(id, expectedNames = capturePresetNames(dependencies.getDatabase().botPresets)) {
            const before = capturePresetNames(dependencies.getDatabase().botPresets)
            let removedId: string | undefined
            await dependencies.mutatePersistentPresets('remove-preset', (state) => {
                assertPresetNames(dependencies.getDatabase().botPresets, before)
                assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                assertPresetNames(state.presets, presetNames(expectedNames))
                if (state.presets.length <= 1) {
                    throw new Error('There must be at least one preset')
                }
                saveActive(state)
                assertPresetIndex(state.presets, id)
                const selected = state.root.botPresetsId
                removedId = state.presets.splice(id, 1)[0]?.id
                if (id === selected) select(state, 0)
                else if (id < selected) state.root.botPresetsId = selected - 1
            })
            if (removedId && dependencies.getEffectivePresetOverride?.() === removedId) {
                dependencies.clearEffectivePresetOverride?.()
            }
        },
        movePreset(fromIndex, toIndex, expectedNames = capturePresetNames(dependencies.getDatabase().botPresets)) {
            const before = capturePresetNames(dependencies.getDatabase().botPresets)
            return dependencies.mutatePersistentPresets('move-preset', (state) => {
                assertPresetNames(dependencies.getDatabase().botPresets, before)
                assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                assertPresetNames(state.presets, presetNames(expectedNames))
                saveActive(state)
                assertPresetIndex(state.presets, fromIndex)
                if (!Number.isInteger(toIndex) || toIndex < 0 || toIndex > state.presets.length) {
                    throw new Error(`Preset destination ${toIndex} is out of bounds`)
                }
                const activeIndex = state.root.botPresetsId
                const [moved] = state.presets.splice(fromIndex, 1)
                const adjustedToIndex = toIndex > fromIndex ? toIndex - 1 : toIndex
                state.presets.splice(adjustedToIndex, 0, moved)
                if (activeIndex === fromIndex) state.root.botPresetsId = adjustedToIndex
                else if (fromIndex < activeIndex && adjustedToIndex >= activeIndex) {
                    state.root.botPresetsId = activeIndex - 1
                } else if (fromIndex > activeIndex && adjustedToIndex <= activeIndex) {
                    state.root.botPresetsId = activeIndex + 1
                }
            })
        },
        renamePreset(id, name, expectedNames = capturePresetNames(dependencies.getDatabase().botPresets)) {
            const before = capturePresetNames(dependencies.getDatabase().botPresets)
            return dependencies.mutatePersistentPresets('rename-preset', (state) => {
                assertPresetNames(dependencies.getDatabase().botPresets, before)
                assertPresetNames(dependencies.getDatabase().botPresets, expectedNames)
                assertPresetNames(state.presets, presetNames(expectedNames))
                saveActive(state)
                assertPresetIndex(state.presets, id)
                state.presets[id].name = name
            })
        },
        updateActivePresetImage(image) {
            return dependencies.mutatePersistentPresets('update-preset-image', (state) => {
                saveActive(state)
                // The settings window names the selection, also while a binding or chain pick applies another preset.
                assertPresetIndex(state.presets, state.root.botPresetsId)
                state.presets[state.root.botPresetsId].image = image
            })
        },
        clearPluginProvider(owner) {
            return dependencies.mutatePersistentPresets('clear-plugin-provider', (state) => {
                saveActive(state)
                if (state.root.currentPluginProvider === owner) state.root.currentPluginProvider = ''
                for (const preset of state.presets) {
                    if (preset?.currentPluginProvider === owner) preset.currentPluginProvider = ''
                }
            })
        },
    }
}

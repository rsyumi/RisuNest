import type { Database, botPreset } from './database.svelte'
import {
    RevisionConflictError,
    type DataRevision,
    type PersistentDataStore,
    type PersistentRoot,
} from './persistentDataStore'
import { safeStructuredClone } from '../polyfill'

export interface PresetWorkingSetMutationState {
    root: PersistentRoot
    presets: botPreset[]
}

export interface PresetWorkingSetControllerDependencies {
    getDatabase(): Database
    captureCurrentPreset(database: Database): botPreset | null
    applyPreset(root: PersistentRoot, preset: botPreset): void
    mutatePersistentPresets(
        reason: string,
        mutate: (state: PresetWorkingSetMutationState) => void | Promise<void>,
    ): Promise<void>
}

export interface PresetWorkingSetController {
    saveCurrentPreset(): Promise<void>
    changeToPreset(id?: number, saveCurrent?: boolean): Promise<void>
    addPreset(preset: botPreset, activate?: boolean): Promise<number>
    copyPreset(id: number): Promise<number>
    removePreset(id: number): Promise<void>
    movePreset(fromIndex: number, toIndex: number): Promise<void>
    renamePreset(id: number, name: string): Promise<void>
    updateActivePresetImage(image: string): Promise<void>
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
        const activeIndex = database.botPresetsId
        if (!Number.isInteger(activeIndex) || activeIndex < 0) return
        assertPresetIndex(state.presets, activeIndex)
        const captured = dependencies.captureCurrentPreset(database)
        if (captured) state.presets[activeIndex] = clonePreset(captured)
    }

    const select = (state: PresetWorkingSetMutationState, id: number): void => {
        assertPresetIndex(state.presets, id)
        state.root.botPresetsId = id
        dependencies.applyPreset(state.root, state.presets[id])
    }

    return {
        saveCurrentPreset: () => dependencies.mutatePersistentPresets(
            'save-current-preset',
            saveActive,
        ),
        changeToPreset(id = 0, saveCurrent = true) {
            return dependencies.mutatePersistentPresets('change-preset', (state) => {
                if (saveCurrent) saveActive(state)
                select(state, id)
            })
        },
        async addPreset(preset, activate = false) {
            let addedIndex = -1
            await dependencies.mutatePersistentPresets('add-preset', (state) => {
                saveActive(state)
                addedIndex = state.presets.length
                state.presets.push(clonePreset(preset))
                if (activate) select(state, addedIndex)
            })
            return addedIndex
        },
        async copyPreset(id) {
            let addedIndex = -1
            await dependencies.mutatePersistentPresets('copy-preset', (state) => {
                saveActive(state)
                assertPresetIndex(state.presets, id)
                const copied = clonePreset(state.presets[id])
                copied.name = `${copied.name} Copy`
                addedIndex = state.presets.length
                state.presets.push(copied)
            })
            return addedIndex
        },
        removePreset(id) {
            return dependencies.mutatePersistentPresets('remove-preset', (state) => {
                if (state.presets.length <= 1) {
                    throw new Error('There must be at least one preset')
                }
                saveActive(state)
                assertPresetIndex(state.presets, id)
                state.presets.splice(id, 1)
                select(state, 0)
            })
        },
        movePreset(fromIndex, toIndex) {
            return dependencies.mutatePersistentPresets('move-preset', (state) => {
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
        renamePreset(id, name) {
            return dependencies.mutatePersistentPresets('rename-preset', (state) => {
                saveActive(state)
                assertPresetIndex(state.presets, id)
                state.presets[id].name = name
            })
        },
        updateActivePresetImage(image) {
            return dependencies.mutatePersistentPresets('update-preset-image', (state) => {
                saveActive(state)
                const activeIndex = state.root.botPresetsId
                assertPresetIndex(state.presets, activeIndex)
                state.presets[activeIndex].image = image
            })
        },
    }
}

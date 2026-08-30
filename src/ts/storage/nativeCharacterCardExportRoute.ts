import { save } from '@tauri-apps/plugin-dialog'

import { isTauriAndroid, isTauriDesktop } from '../platform'
import type { CharacterDetail, DataRevision } from './persistentDataStore'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import { assertPinnedRevision, withPersistentRevisionLease } from './persistentRecordIterator'
import {
    runNativeCharacterCardExport,
    type NativeCharacterCardExportInput,
    type NativeFileJobOptions,
    type NativeFileJobResult,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

interface NativeCharacterCardPickerInput {
    characterId: string
    suggestedName: string
    format: 'json-card' | 'png-card'
    projectCharacter(character: CharacterDetail): Record<string, unknown>
}

interface NativeCharacterCardExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

interface NativeCharacterCardExportRouteDependencies {
    isDesktop(): boolean
    isAndroid(): boolean
    chooseDestination(
        suggestedName: string,
        format: NativeCharacterCardPickerInput['format'],
    ): Promise<string | null>
    runtime(): NativeCharacterCardExportRuntime
    readCharacter(characterId: string, revision: DataRevision): Promise<CharacterDetail>
    runExport(
        input: NativeCharacterCardExportInput,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
}

const productionDependencies: NativeCharacterCardExportRouteDependencies = {
    isDesktop: () => isTauriDesktop,
    isAndroid: () => isTauriAndroid,
    chooseDestination: (suggestedName, format) => save({
        defaultPath: suggestedName,
        filters: [{
            name: format === 'json-card' ? 'JSON character card' : 'PNG character card',
            extensions: [format === 'json-card' ? 'json' : 'png'],
        }],
    }),
    runtime: getPersistentDataRuntime,
    readCharacter: async (characterId, revision) => {
        const lease = await getPersistentDataStore().acquireRevision(revision)
        return withPersistentRevisionLease(lease, async (reader) => {
            const found = await reader.readCharacter(characterId)
            if (!found) throw new Error(`Character ${characterId} is missing from revision ${revision}`)
            assertPinnedRevision(revision, found.revision, `Character ${characterId}`)
            return found.value
        })
    },
    runExport: runNativeCharacterCardExport,
}

export async function exportNativeCharacterCardFromPicker(
    input: NativeCharacterCardPickerInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeCharacterCardExportRouteDependencies = productionDependencies,
): Promise<NativeFileJobResult | null | undefined> {
    if (!dependencies.isDesktop() && !dependencies.isAndroid()) return undefined
    const destination = dependencies.isDesktop()
        ? await dependencies.chooseDestination(input.suggestedName, input.format)
        : undefined
    if (dependencies.isDesktop() && !destination) return null
    const runtime = dependencies.runtime()
    await runtime.flushPendingData('native-character-card-export')
    if (options.signal?.aborted) throw new DOMException('Native file job was cancelled', 'AbortError')
    const expectedRevision = runtime.revision
    const character = await dependencies.readCharacter(input.characterId, expectedRevision)
    const metadata = input.projectCharacter(character)
    return dependencies.runExport({
        characterId: input.characterId,
        destination: destination
            ? { type: 'desktopPath', path: destination }
            : { type: 'androidSaf', suggestedName: input.suggestedName },
        expectedRevision,
        format: input.format,
        metadata,
    }, options)
}

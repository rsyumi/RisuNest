import { save } from '@tauri-apps/plugin-dialog'

import { isTauriAndroid, isTauriDesktop } from '../platform'
import type { CharacterDetail, DataRevision } from './persistentDataStore'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import { readPinnedCharacterDetail } from './persistentRecordIterator'
import {
    runNativeCharacterCardExport,
    type NativeCharacterCardExportInput,
    type NativeFileJobOptions,
    type NativeFileJobResult,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { prepareNativeContentExportFromPicker } from './nativeContentExportPicker'

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
    readCharacter: (characterId, revision) =>
        readPinnedCharacterDetail(getPersistentDataStore(), characterId, revision),
    runExport: runNativeCharacterCardExport,
}

export async function exportNativeCharacterCardFromPicker(
    input: NativeCharacterCardPickerInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeCharacterCardExportRouteDependencies = productionDependencies,
): Promise<NativeFileJobResult | null | undefined> {
    const flow = await prepareNativeContentExportFromPicker({
        suggestedName: input.suggestedName,
        flushReason: 'native-character-card-export',
        chooseDestination: () =>
            dependencies.chooseDestination(input.suggestedName, input.format),
    }, options, dependencies)
    if (flow.kind === 'unsupported') return undefined
    if (flow.kind === 'cancelled') return null
    const character = await dependencies.readCharacter(input.characterId, flow.expectedRevision)
    const metadata = input.projectCharacter(character)
    return dependencies.runExport({
        characterId: input.characterId,
        destination: flow.destination,
        expectedRevision: flow.expectedRevision,
        format: input.format,
        metadata,
    }, options)
}

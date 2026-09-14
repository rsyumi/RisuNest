import { save } from '@tauri-apps/plugin-dialog'

import { isTauriIOS, isTauriAndroid, isTauriDesktop } from '../platform'
import type { CharacterDetail, DataRevision } from './persistentDataStore'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import { readPinnedCharacterDetail } from './persistentRecordIterator'
import {
    runNativeCharacterCharxExport,
    type NativeCharacterCharxExportInput,
    type NativeFileJobOptions,
    type NativeFileJobResult,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { prepareNativeContentExportFromPicker } from './nativeContentExportPicker'

interface NativeCharacterCharxPickerInput {
    characterId: string
    suggestedName: string
    container?: 'appended-charx-jpeg'
    projectCharacter(character: CharacterDetail): {
        card: Record<string, unknown>
        module: Record<string, unknown>
    }
}

interface NativeCharacterCharxExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

interface NativeCharacterCharxExportRouteDependencies {
    isDesktop(): boolean
    isAndroid(): boolean
    isIOS?(): boolean
    chooseDestination(
        suggestedName: string,
        container: NativeCharacterCharxPickerInput['container'],
    ): Promise<string | null>
    runtime(): NativeCharacterCharxExportRuntime
    readCharacter(
        characterId: string,
        revision: DataRevision,
    ): Promise<CharacterDetail>
    runExport(
        input: NativeCharacterCharxExportInput,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
}

const productionDependencies: NativeCharacterCharxExportRouteDependencies = {
    isDesktop: () => isTauriDesktop,
    isAndroid: () => isTauriAndroid,
    isIOS: () => isTauriIOS,
    chooseDestination: (suggestedName, container) =>
        save({
            defaultPath: suggestedName,
            filters: [
                container === 'appended-charx-jpeg'
                    ? { name: 'CharX JPEG', extensions: ['jpeg'] }
                    : { name: 'CharX', extensions: ['charx'] },
            ],
        }),
    runtime: getPersistentDataRuntime,
    readCharacter: (characterId, revision) =>
        readPinnedCharacterDetail(
            getPersistentDataStore(),
            characterId,
            revision,
        ),
    runExport: runNativeCharacterCharxExport,
}

export async function exportNativeCharacterCharxFromPicker(
    input: NativeCharacterCharxPickerInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeCharacterCharxExportRouteDependencies = productionDependencies,
): Promise<NativeFileJobResult | null | undefined> {
    const flow = await prepareNativeContentExportFromPicker({
        suggestedName: input.suggestedName,
        flushReason: 'native-character-charx-export',
        chooseDestination: () =>
            dependencies.chooseDestination(input.suggestedName, input.container),
    }, options, dependencies)
    if (flow.kind === 'unsupported') return undefined
    if (flow.kind === 'cancelled') return null
    const character = await dependencies.readCharacter(input.characterId, flow.expectedRevision)
    const projected = input.projectCharacter(character)
    return dependencies.runExport(
        {
            characterId: input.characterId,
            destination: flow.destination,
            expectedRevision: flow.expectedRevision,
            ...(input.container ? { container: input.container } : {}),
            card: projected.card,
            module: projected.module,
        },
        options,
    )
}

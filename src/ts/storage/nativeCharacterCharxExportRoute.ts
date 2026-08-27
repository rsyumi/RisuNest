import { save } from '@tauri-apps/plugin-dialog'

import { isTauriDesktop } from '../platform'
import {
    runNativeCharacterCharxExport,
    type NativeCharacterCharxExportInput,
    type NativeFileJobOptions,
    type NativeFileJobResult,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

interface NativeCharacterCharxPickerInput {
    characterId: string
    suggestedName: string
    card: Record<string, unknown>
    module: Record<string, unknown>
}

interface NativeCharacterCharxExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

interface NativeCharacterCharxExportRouteDependencies {
    isDesktop(): boolean
    chooseDestination(suggestedName: string): Promise<string | null>
    runtime(): NativeCharacterCharxExportRuntime
    runExport(
        runtime: NativeCharacterCharxExportRuntime,
        input: NativeCharacterCharxExportInput,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
}

const productionDependencies: NativeCharacterCharxExportRouteDependencies = {
    isDesktop: () => isTauriDesktop,
    chooseDestination: (suggestedName) => save({
        defaultPath: suggestedName,
        filters: [{ name: 'CharX', extensions: ['charx'] }],
    }),
    runtime: getPersistentDataRuntime,
    runExport: runNativeCharacterCharxExport,
}

export async function exportNativeCharacterCharxFromPicker(
    input: NativeCharacterCharxPickerInput,
    options: NativeFileJobOptions = {},
    dependencies: NativeCharacterCharxExportRouteDependencies = productionDependencies,
): Promise<NativeFileJobResult | null | undefined> {
    if (!dependencies.isDesktop()) return undefined
    const destination = await dependencies.chooseDestination(input.suggestedName)
    if (!destination) return null
    return dependencies.runExport(
        dependencies.runtime(),
        {
            characterId: input.characterId,
            destination,
            card: input.card,
            module: input.module,
        },
        options,
    )
}

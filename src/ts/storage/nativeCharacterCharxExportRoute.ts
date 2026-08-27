import { save } from '@tauri-apps/plugin-dialog'

import { isTauriDesktop } from '../platform'
import type { CharacterDetail, DataRevision } from './persistentDataStore'
import { getPersistentDataStore } from './persistentDataStoreFactory'
import { assertPinnedRevision, withPersistentRevisionLease } from './persistentRecordIterator'
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
    chooseDestination(suggestedName: string): Promise<string | null>
    runtime(): NativeCharacterCharxExportRuntime
    readCharacter(characterId: string, revision: DataRevision): Promise<CharacterDetail>
    runExport(
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
    readCharacter: async (characterId, revision) => {
        const lease = await getPersistentDataStore().acquireRevision(revision)
        return withPersistentRevisionLease(lease, async (reader) => {
            const found = await reader.readCharacter(characterId)
            if (!found) throw new Error(`Character ${characterId} is missing from revision ${revision}`)
            assertPinnedRevision(revision, found.revision, `Character ${characterId}`)
            return found.value
        })
    },
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
    const runtime = dependencies.runtime()
    await runtime.flushPendingData('native-character-charx-export')
    if (options.signal?.aborted) throw new DOMException('Native file job was cancelled', 'AbortError')
    const expectedRevision = runtime.revision
    const character = await dependencies.readCharacter(input.characterId, expectedRevision)
    const projected = input.projectCharacter(character)
    return dependencies.runExport(
        {
            characterId: input.characterId,
            destination,
            expectedRevision,
            card: projected.card,
            module: projected.module,
        },
        options,
    )
}

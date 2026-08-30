import { save } from '@tauri-apps/plugin-dialog'

import { isTauriAndroid, isTauriDesktop } from '../platform'
import type { RisuModule } from '../process/modules'
import { getDatabase } from './database.svelte'
import {
    runNativeRisuModuleExport,
    type NativeFileJobOptions,
    type NativeFileJobResult,
    type NativeRisuModuleExportInput,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'

interface NativeRisumExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

interface NativeModuleRisumExportRouteDependencies {
    isDesktop(): boolean
    isAndroid(): boolean
    chooseDestination(suggestedName: string): Promise<string | null>
    runtime(): NativeRisumExportRuntime
    modules(): RisuModule[]
    runExport(
        input: NativeRisuModuleExportInput,
        options?: NativeFileJobOptions,
    ): Promise<NativeFileJobResult>
}

const productionDependencies: NativeModuleRisumExportRouteDependencies = {
    isDesktop: () => isTauriDesktop,
    isAndroid: () => isTauriAndroid,
    chooseDestination: (suggestedName) => save({
        defaultPath: suggestedName,
        filters: [{ name: 'Risu module', extensions: ['risum'] }],
    }),
    runtime: getPersistentDataRuntime,
    modules: () => getDatabase().modules,
    runExport: runNativeRisuModuleExport,
}

export async function exportNativeModuleRisumFromPicker(
    module: RisuModule,
    options: NativeFileJobOptions = {},
    dependencies: NativeModuleRisumExportRouteDependencies = productionDependencies,
): Promise<NativeFileJobResult | null | undefined> {
    if (!dependencies.isDesktop() && !dependencies.isAndroid()) return undefined
    const suggestedName = `${module.name || 'module'}.risum`
    const destination = dependencies.isDesktop()
        ? await dependencies.chooseDestination(suggestedName)
        : undefined
    if (dependencies.isDesktop() && !destination) return null
    const runtime = dependencies.runtime()
    await runtime.flushPendingData('native-risum-export')
    if (options.signal?.aborted) {
        throw new DOMException('Native file job was cancelled', 'AbortError')
    }
    const moduleIndex = dependencies.modules().findIndex((candidate) => candidate === module)
    if (moduleIndex < 0) throw new Error('Native RISUM export requires the exact root module object')
    return dependencies.runExport({
        moduleIndex,
        expectedRevision: runtime.revision,
        destination: destination
            ? { type: 'desktopPath', path: destination }
            : { type: 'androidSaf', suggestedName },
    }, options)
}

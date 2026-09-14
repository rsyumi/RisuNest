import { save } from '@tauri-apps/plugin-dialog'

import { isTauriIOS, isTauriAndroid, isTauriDesktop } from '../platform'
import type { RisuModule } from '../process/modules'
import { getDatabase } from './database.svelte'
import {
    runNativeRisuModuleExport,
    type NativeFileJobOptions,
    type NativeFileJobResult,
    type NativeRisuModuleExportInput,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { prepareNativeContentExportFromPicker } from './nativeContentExportPicker'

interface NativeRisumExportRuntime {
    readonly revision: number
    flushPendingData(reason: string): Promise<void>
}

interface NativeModuleRisumExportRouteDependencies {
    isDesktop(): boolean
    isAndroid(): boolean
    isIOS?(): boolean
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
    isIOS: () => isTauriIOS,
    chooseDestination: (suggestedName) =>
        save({
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
    const suggestedName = `${module.name || 'module'}.risum`
    const flow = await prepareNativeContentExportFromPicker({
        suggestedName,
        flushReason: 'native-risum-export',
        chooseDestination: () => dependencies.chooseDestination(suggestedName),
    }, options, dependencies)
    if (flow.kind === 'unsupported') return undefined
    if (flow.kind === 'cancelled') return null
    const moduleIndex = dependencies.modules().findIndex((candidate) => candidate === module)
    if (moduleIndex < 0) throw new Error('Native RISUM export requires the exact root module object')
    return dependencies.runExport({
        moduleIndex,
        expectedRevision: flow.expectedRevision,
        destination: flow.destination,
    }, options)
}

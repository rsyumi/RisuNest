import { open, save } from '@tauri-apps/plugin-dialog'

import { downloadFile } from '../globalApi.svelte'
import { isTauriDesktop } from '../platform'
import { loadPlugins } from '../plugins/plugins.svelte'
import { selectFileByDom } from '../util'
import {
    runNativeBlockRisuSaveExport,
    runNativeBlockRisuSaveRestore,
} from './nativeFileJobs'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { decodeRisuSave } from './risuSave'
import {
    exportRisuSaveFromPicker,
    importRisuSaveFromPicker,
    type RisuSaveFileRouteDependencies,
    type RisuSaveFileRouteOptions,
} from './risuSaveFileRoute'
import { withFlushedRisuSaveExport } from './risuSaveStoreAdapter'

const productionDependencies: RisuSaveFileRouteDependencies = {
    platform: () => isTauriDesktop ? 'native-desktop' : 'web',
    runtime: getPersistentDataRuntime,
    chooseNativeImport: async () => {
        const selected = await open({
            multiple: false,
            directory: false,
            filters: [{ name: 'RisuSave', extensions: ['risudat'] }],
        })
        return typeof selected === 'string' ? selected : null
    },
    chooseNativeExport: async (name) => save({
        defaultPath: name,
        filters: [{ name: 'RisuSave', extensions: ['risudat'] }],
    }),
    chooseWebImport: () => selectFileByDom(['risudat'], 'single'),
    runNativeImport: (runtime, source, options) => runNativeBlockRisuSaveRestore(
        {
            get revision() { return runtime.revision },
            flushPendingData: (reason) => runtime.flushPendingData(reason),
            refreshActiveWorkingSet: (revision) =>
                runtime.refreshActiveWorkingSetFromStore(revision),
        },
        source,
        options,
    ),
    runNativeExport: runNativeBlockRisuSaveExport,
    decodeRisuSave,
    collectWebExport: (omitAccount) => withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'risu-save-file-export',
        (pinned) => pinned.collectBytes({ omitAccount }),
    ),
    downloadWebExport: downloadFile,
    reloadPlugins: loadPlugins,
}

export const importRisuSaveFromSystemPicker = (options: RisuSaveFileRouteOptions = {}) =>
    importRisuSaveFromPicker(options, productionDependencies)

export const exportRisuSaveFromSystemPicker = (options: RisuSaveFileRouteOptions = {}) =>
    exportRisuSaveFromPicker(options, productionDependencies)

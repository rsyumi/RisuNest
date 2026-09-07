import { open, save } from '@tauri-apps/plugin-dialog'

import { downloadFile } from '../globalApi.svelte'
import { isTauriAndroid, isTauriDesktop } from '../platform'
import {
    acknowledgeAndroidSafExport,
    copyNativeExportToAndroidSaf,
    markAndroidSafExportPublicationReady,
} from './androidSafBridge'
import {
    loadPlugins,
    loadPluginsAfterAuthoritativeRestore,
} from '../plugins/plugins.svelte'
import { selectFileByDom } from '../util'
import {
    nativeFileOperation,
    runSharedNativeFileOperation,
} from './nativeFileJobManager'
import {
    runNativeBlockRisuSaveExport,
    runNativeBlockRisuSaveRestore,
} from './nativeFileJobs'
import { describeDesktopSource } from './nativeFileSourceInfo'
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
    platform: () => isTauriDesktop
        ? 'native-desktop'
        : isTauriAndroid
            ? 'native-android'
            : 'web',
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
    runNativeImport: runNativeBlockRisuSaveRestore,
    runNativeExport: runNativeBlockRisuSaveExport,
    decodeRisuSave,
    collectWebExport: (omitAccount) => withFlushedRisuSaveExport(
        getPersistentDataRuntime(),
        'risu-save-file-export',
        (pinned) => pinned.collectBytes({ omitAccount }),
    ),
    downloadWebExport: async (name, bytes) => {
        await downloadFile(name, bytes)
    },
    withFlushedExport: withFlushedRisuSaveExport,
    copyAndroidExport: copyNativeExportToAndroidSaf,
    markAndroidExportReady: markAndroidSafExportPublicationReady,
    acknowledgeAndroidExport: acknowledgeAndroidSafExport,
    reloadPlugins: loadPlugins,
    reloadPluginsAfterNativeRestore: loadPluginsAfterAuthoritativeRestore,
    describeNativeSource: describeDesktopSource,
}

export { nativeFileOperation }

export const importRisuSaveFromSystemPicker = (options: RisuSaveFileRouteOptions = {}) =>
    runSharedNativeFileOperation(
        'import',
        'risu-save-import',
        ({ signal, onStatus, setBlocking, setSource }) =>
            importRisuSaveFromPicker({
                ...options,
                signal,
                onStatus: (status) => {
                    onStatus(status)
                    options.onStatus?.(status)
                },
                onBlockingChange: (blocking) => {
                    setBlocking(blocking)
                    options.onBlockingChange?.(blocking)
                },
                onSource: (source) => {
                    setSource(source)
                    options.onSource?.(source)
                },
            }, productionDependencies),
        { presentation: 'dialog', format: 'risu-save' },
    )

export const exportRisuSaveFromSystemPicker = (options: RisuSaveFileRouteOptions = {}) =>
    runSharedNativeFileOperation('export', 'risu-save-export', ({ signal, onStatus }) =>
        exportRisuSaveFromPicker({
            ...options,
            signal,
            onStatus: (status) => {
                onStatus(status)
                options.onStatus?.(status)
            },
        }, productionDependencies))

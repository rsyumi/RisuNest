import type { ExportExclusions } from './exportExcludedReport'
import { open, save } from '@tauri-apps/plugin-dialog'

import { downloadFile } from '../globalApi.svelte'
import { appDataDir, join } from '@tauri-apps/api/path'
import { mkdir, remove } from '@tauri-apps/plugin-fs'
import { pickIOSFile, discardIOSFile, exportIOSFile } from './iosFiles'
import { isTauriIOS, isTauriAndroid, isTauriDesktop } from '../platform'
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
    type NativeFileJobResult,
    runNativeBlockRisuSaveExport,
    runNativeBlockRisuSaveRestore,
} from './nativeFileJobs'
import { describeDesktopSource } from './nativeFileSourceInfo'
import { getPersistentDataRuntime } from './persistentDataRuntime.svelte'
import { decodeRisuSave } from './risuSave'
import { selectPluginValueAssignment } from './pluginValueAssignDialog'
import {
    exportRisuSaveFromPicker,
    importRisuSaveFromPicker,
    type RisuSaveFileRouteDependencies,
    type RisuSaveFileRouteOptions,
} from './risuSaveFileRoute'
import { withFlushedRisuSaveExport } from './risuSaveStoreAdapter'

const productionDependencies: RisuSaveFileRouteDependencies = {
    platform: () =>
        isTauriIOS
            ? 'native-ios'
            : isTauriDesktop
              ? 'native-desktop'
              : isTauriAndroid
                ? 'native-android'
                : 'web',
    runtime: getPersistentDataRuntime,
    chooseNativeImport: async () => {
        if (isTauriIOS) return (await pickIOSFile())?.path ?? null
        const selected = await open({
            multiple: false,
            directory: false,
            filters: [{ name: 'RisuSave', extensions: ['risudat'] }],
        })
        return typeof selected === 'string' ? selected : null
    },
    chooseNativeExport: async (name) => {
        if (isTauriIOS) {
            const folder = await join(
                await appDataDir(),
                'ios-file-staging',
                crypto.randomUUID(),
            )
            await mkdir(folder, { recursive: true })
            return join(folder, name)
        }
        return save({
            defaultPath: name,
            filters: [{ name: 'RisuSave', extensions: ['risudat'] }],
        })
    },
    chooseWebImport: () => selectFileByDom(['risudat'], 'single'),
    runNativeImport: runNativeBlockRisuSaveRestore,
    assignPluginValues: selectPluginValueAssignment,
    cleanupNativeImport: async (path) => {
        if (isTauriIOS)
            await discardIOSFile(path).catch((error) =>
                console.error('iOS import cleanup failed', error),
            )
    },
    runNativeExport: async (runtime, destination, options) => {
        let result: NativeFileJobResult | undefined
        try {
            result = await runNativeBlockRisuSaveExport(
                runtime,
                destination,
                options,
            )
            if (isTauriIOS) {
                const published = await exportIOSFile({
                    sourcePath: destination,
                    suggestedName: destination.split('/').at(-1)!,
                    signal: options?.signal,
                })
                result.warningCodes = [...new Set([...result.warningCodes, ...(published.warningCodes ?? [])])]
                if (published.bytes !== result.sourceBytes)
                    throw new Error(
                        'Published RisuSave length differs from its source',
                    )
            }
            return result
        } finally {
            if (isTauriIOS)
                await remove(destination.substring(0, destination.lastIndexOf('/')), { recursive: true }).catch((error) => {
                    if (result) result.warningCodes = [...new Set([...result.warningCodes, 'cleanup-failed'])]
                    else console.error('iOS export cleanup failed', error)
                })
        }
    },
    decodeRisuSave,
    collectWebExport: (omitAccount) =>
        withFlushedRisuSaveExport(
            getPersistentDataRuntime(),
            'risu-save-file-export',
            async (pinned) => {
                let exclusions: ExportExclusions | undefined
                const bytes = await pinned.collectBytes({ omitAccount, onExclusions: report => { exclusions = report } })
                if (!exclusions) throw new Error('The export did not return its exclusion report')
                return { bytes, exclusions }
            },
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

export const importRisuSaveFromSystemPicker = (
    options: RisuSaveFileRouteOptions = {},
) =>
    runSharedNativeFileOperation(
        'import',
        'risu-save-import',
        ({ signal, onStatus, setBlocking, setSource }) =>
            importRisuSaveFromPicker(
                {
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
                },
                productionDependencies,
            ),
        { presentation: 'dialog', format: 'risu-save' },
    )

export const exportRisuSaveFromSystemPicker = (
    options: RisuSaveFileRouteOptions = {},
) =>
    runSharedNativeFileOperation(
        'export',
        'risu-save-export',
        ({ signal, onStatus }) =>
            exportRisuSaveFromPicker(
                {
                    ...options,
                    signal,
                    onStatus: (status) => {
                        onStatus(status)
                        options.onStatus?.(status)
                    },
                },
                productionDependencies,
            ),
        { presentation: 'dialog', format: 'risu-save' },
    )

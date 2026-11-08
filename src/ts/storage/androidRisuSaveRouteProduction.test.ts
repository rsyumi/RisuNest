import { describe, expect, it, vi } from 'vitest'

const backupMocks = vi.hoisted(() => ({
    presented: vi.fn(),
    reason: vi.fn((code: string) => `localized:${code || "unknown"}`),
    replay: vi.fn(),
    restore: vi.fn(),
    externalManager: vi.fn(),
    openedBatch: vi.fn(),
    prepare: vi.fn(),
    activateCharacter: vi.fn(),
    activateModule: vi.fn(),
}))

vi.mock('../gui/nativeFileJobDialogModel', () => ({ failureReason: backupMocks.reason }))
vi.mock('./fileOperationErrorPresentation', () => ({ fileOperationErrorWasPresented: backupMocks.presented }))
vi.mock('./androidContentPicker', async original => ({
    ...await original<typeof import('./androidContentPicker')>(),
    importReplayedAndroidContentSpool: backupMocks.replay,
    importOpenedAndroidContentSpools: backupMocks.openedBatch,
}))
vi.mock('./nativeFileJobs', async original => ({
    ...await original<typeof import('./nativeFileJobs')>(),
    prepareNativeContentImport: backupMocks.prepare,
}))
vi.mock('./nativeCharacterContentActivation', () => ({
    activatePreparedNativeCharacterContent: backupMocks.activateCharacter,
}))
vi.mock('./nativeModuleContentActivation', () => ({
    activatePreparedNativeModuleContent: backupMocks.activateModule,
}))
vi.mock('../alert', () => ({
    alertConfirm: vi.fn(),
    alertError: vi.fn(),
    alertNormal: vi.fn(),
}))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule({ runtime: 'tauri', os: 'android' }))
vi.mock('../plugins/plugins.svelte', () => ({
    loadPluginsAfterAuthoritativeRestore: vi.fn(),
}))
vi.mock('./persistentDataRuntime.svelte', () => ({
    getPersistentDataRuntime: vi.fn(),
}))
vi.mock('./portableBackupFileRouteProduction.svelte', () => ({
    restoreBackupFromNativeSource: backupMocks.restore,
}))
vi.mock('./nativeFileJobManager', () => ({
    runExternalAndroidNativeFileOperation: backupMocks.externalManager,
}))

import { language } from 'src/lang'
import { alertConfirm, alertNormal, alertError } from '../alert'

import {
    showAndroidFileError,
    showDestinationRequired,
    importAndroidCharacterSpool,
    importAndroidContentSpoolBatch,
    createAndroidOpenedSpoolDispatcher,
    dispatchAndroidOpenedSpoolBatch,
    importAndroidOpenedPreparedContent,
    restoreAndroidOpenedBackupSource,
    registerAndroidRisuSaveRoute,
    type AndroidOpenedSpoolDispatchDependencies,
} from './androidRisuSaveRouteProduction.svelte'
import type { PreparedNativeContent, PreparedNativeContentReceipt } from './nativeFileJobs'
import type { AndroidSafJavascriptBridge } from './androidSafBridge'
import { createPortableExportIntentStore, rememberPortableExport, resumePendingPortableExport, portableAndroidPublicationDependencies } from './deviceBackup/job'

describe('Android opened spool production route', () => {
    it('leaves portable intent receipts to recovery and alerts unowned partial files once', async () => {
        const terminal = {
            requestId: 'aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa',
            exportId: 'bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb', sourceKind: 'risuSave',
            state: 'failed', code: 'destination-interrupted', copyStarted: true,
            publicationPrerequisitesComplete: true, warningCodes: ['partial-destination-may-remain'],
        }
        const owned = {...terminal, requestId: 'dddddddd-dddd-4ddd-8ddd-dddddddddddd',
            state: 'succeeded', code: undefined, bytes: 1234, warningCodes: []}
        let receipt: typeof terminal | typeof owned | null = owned
        const acknowledge = vi.fn(async (requestId: string) => {
            if (receipt?.requestId === requestId) receipt = null
            return true
        })
        const getStatus = vi.fn(() => receipt ? JSON.stringify(receipt) : null)
        const bridge = {copyExport: () => {}, getExportStatus: getStatus,
            acknowledgeExport: acknowledge} satisfies AndroidSafJavascriptBridge
        const target = Object.assign(new EventTarget(), {RisuSafBridge: bridge})
        vi.stubGlobal('window', target)
        const values = new Map<string, string>()
        vi.stubGlobal('localStorage', {
            getItem: (key: string) => values.get(key) ?? null,
            setItem: (key: string, value: string) => { values.set(key, value) },
            removeItem: (key: string) => { values.delete(key) },
        })
        vi.mocked(alertError).mockClear()
        try {
            const store = createPortableExportIntentStore()
            rememberPortableExport('synthetic-job', {type: 'androidSaf', suggestedName: 'synthetic.risunest'}, store, owned.requestId)
            registerAndroidRisuSaveRoute()
            await vi.waitFor(() => expect(getStatus).toHaveBeenCalled())
            window.dispatchEvent(new CustomEvent('risu-android-saf-destination', {detail: {
                ...owned, state: 'failed', code: 'destination-write-failed', warningCodes: ['partial-destination-may-remain'],
            }}))
            await Promise.resolve()
            expect(receipt).toEqual(owned)
            expect(acknowledge).not.toHaveBeenCalled()
            expect(alertError).not.toHaveBeenCalled()
            const result = {revision: 1, sourceBytes: 1234, characterCount: 0, presetCount: 0,
                warningCodes: [], handoffPath: '/synthetic/risunest-backup-synthetic-job.risunest'}
            const cleanup = vi.fn(async () => {
                expect(receipt).toBeNull()
            })
            const onResult = vi.fn()
            const dependencies = {
                store, ...portableAndroidPublicationDependencies(),
                invoke: async <T,>(command: string): Promise<T> => {
                    if (command === 'native_file_job_status') return {
                        jobId: 'synthetic-job', kind: 'export-portable-backup', state: 'succeeded', phase: 'done', result,
                    } as T
                    if (command === 'native_file_job_forget') return true as T
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => {}, cleanupHandoff: cleanup, onResult,
            }
            for (const state of ['failed', 'cancelled']) {
                receipt = {...owned, state, code: 'destination-write-failed', warningCodes: ['partial-destination-may-remain']}
                await expect(resumePendingPortableExport(dependencies)).rejects.toMatchObject({
                    code: 'destination-write-failed', warningCodes: ['partial-destination-may-remain'],
                })
                expect(store.read()?.requestId).toBe(owned.requestId)
                expect(cleanup).not.toHaveBeenCalled()
                expect(acknowledge).not.toHaveBeenCalled()
                expect(alertError).not.toHaveBeenCalled()
            }
            receipt = owned
            await expect(resumePendingPortableExport(dependencies)).resolves.toEqual(result)
            expect(onResult).toHaveBeenCalledExactlyOnceWith(result)
            expect(cleanup).toHaveBeenCalledExactlyOnceWith(result.handoffPath)
            expect(store.read()).toBeNull()
            expect(alertError).not.toHaveBeenCalled()
            acknowledge.mockClear()
            window.dispatchEvent(new CustomEvent('risu-android-saf-destination', {detail: terminal}))
            await vi.waitFor(() => expect(alertError).toHaveBeenCalledExactlyOnceWith(language.screenshotPartialDestinationMayRemain))
            window.dispatchEvent(new CustomEvent('risu-android-saf-destination', {detail: terminal}))
            window.dispatchEvent(new CustomEvent('risu-android-saf-destination', {detail: {...terminal,
                requestId: 'cccccccc-cccc-4ccc-8ccc-cccccccccccc', state: 'succeeded', code: undefined, bytes: 1234, warningCodes: []}}))
            await vi.waitFor(() => expect(acknowledge).toHaveBeenCalledTimes(2))
            expect(alertError).toHaveBeenCalledExactlyOnceWith(language.screenshotPartialDestinationMayRemain)
            for (const [index, corrupt] of ['{broken', '{"version":999}'].entries()) {
                rememberPortableExport('synthetic-job', {type: 'androidSaf', suggestedName: 'synthetic.risunest'}, store, owned.requestId)
                for (const key of values.keys()) values.set(key, corrupt)
                window.dispatchEvent(new CustomEvent('risu-android-saf-destination', {detail: {...terminal,
                    requestId: index === 0 ? 'eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee' : 'ffffffff-ffff-4fff-8fff-ffffffffffff'}}))
                await vi.waitFor(() => expect(acknowledge).toHaveBeenCalledTimes(3 + index))
                expect(alertError).toHaveBeenCalledTimes(2 + index)
                expect(alertError).toHaveBeenLastCalledWith(language.screenshotPartialDestinationMayRemain)
                values.clear()
            }
        } finally {
            values.clear()
            createPortableExportIntentStore().clear('synthetic-job')
            vi.unstubAllGlobals()
        }
    })
    it.each(['backup.RISUNEST', 'compatible.BIN', 'block.RISUDAT'])(
        'delegates %s token and source metadata to the common restore owner without nesting operations',
        async (displayName) => {
            backupMocks.restore.mockReset()
            backupMocks.externalManager.mockClear()
            vi.mocked(alertConfirm).mockClear()
            vi.mocked(alertNormal).mockClear()
            const onSource = vi.fn()
            const source = {
                type: 'androidSpool' as const,
                token: '11111111-1111-4111-8111-111111111111',
            }
            backupMocks.restore.mockImplementation(async (factory) => {
                expect(
                    await factory({
                        signal: new AbortController().signal,
                        onStatus: vi.fn(),
                        onSource,
                    }),
                ).toEqual(source)
                return { warningCodes: [] }
            })

            await restoreAndroidOpenedBackupSource({ source, displayName })

            expect(backupMocks.restore).toHaveBeenCalledExactlyOnceWith(expect.any(Function))
            expect(onSource).toHaveBeenCalledExactlyOnceWith({ name: displayName })
            expect(backupMocks.externalManager).not.toHaveBeenCalled()
            expect(alertConfirm).not.toHaveBeenCalled()
            expect(alertNormal).toHaveBeenCalledExactlyOnceWith(language.risuSaveImportComplete)
        },
    )

    it('does not announce completion or discard source ownership after common restore cancellation', async () => {
        backupMocks.restore.mockReset().mockResolvedValue(null)
        vi.mocked(alertNormal).mockClear()
        await restoreAndroidOpenedBackupSource({
            source: { type: 'androidSpool', token: '11111111-1111-4111-8111-111111111111' },
            displayName: 'cancelled.risunest',
        })
        expect(alertNormal).not.toHaveBeenCalled()
    })

    it('preserves the committed cleanup warning from the common restore result', async () => {
        backupMocks.restore.mockReset().mockResolvedValue({ warningCodes: ['cleanup-failed'] })
        vi.mocked(alertNormal).mockClear()
        await restoreAndroidOpenedBackupSource({
            source: { type: 'androidSpool', token: '11111111-1111-4111-8111-111111111111' },
            displayName: 'restored.risunest',
        })
        expect(alertNormal).toHaveBeenCalledExactlyOnceWith(language.risuSaveCleanupWarning)
    })

    it('keeps the inactive content capability fallback out of the restore discard path', async () => {
        const enqueueRestore = vi.fn(async () => undefined)
        const importCharacter = vi.fn(async () => ({
            kind: 'capability-unavailable' as const,
        }))
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore,
            importCharacter,
            importContentBatch: vi.fn(),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const character = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.charx',
            bytes: 20,
        }
        const save = {
            token: '22222222-2222-4222-8222-222222222222',
            displayName: 'backup.risudat',
            bytes: 30,
        }

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-1',
            ready: [character, save],
            failures: [],
        }, dependencies)

        expect(importCharacter).toHaveBeenCalledExactlyOnceWith(character)
        expect(dependencies.importContentBatch).not.toHaveBeenCalled()
        expect(enqueueRestore).toHaveBeenCalledExactlyOnceWith({
            requestId: 'opened-1',
            ready: [save],
            failures: [],
        })
        expect(dependencies.reportCharacterError).not.toHaveBeenCalled()
        expect(dependencies.reportDestinationRequired).not.toHaveBeenCalled()
    })

    it('restores shared backups first and imports the other files together in one batch', async () => {
        const calls: string[] = []
        const error = new Error('synthetic batch failure')
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => { calls.push('restore') }),
            importCharacter: vi.fn(async () => ({ kind: 'declined' as const })),
            importContentBatch: vi.fn(async () => {
                calls.push('batch')
                throw error
            }),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const card = { token: '11111111-1111-4111-8111-111111111111', displayName: 'card.charx', bytes: 20 }
        const save = { token: '22222222-2222-4222-8222-222222222222', displayName: 'backup.risudat', bytes: 30 }
        const brokenCard = { displayName: 'broken.png', code: 'source-copy-failed' }
        const brokenSave = { displayName: 'old.risunest', code: 'source-copy-failed' }

        // One shared card and one Android could not copy are still two files of one batch.
        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-batch',
            ready: [card, save],
            failures: [brokenCard, brokenSave],
        }, dependencies)

        expect(calls).toEqual(['restore', 'batch'])
        expect(dependencies.enqueueRestore).toHaveBeenCalledExactlyOnceWith({
            requestId: 'opened-batch',
            ready: [save],
            failures: [brokenSave],
        })
        expect(dependencies.importContentBatch).toHaveBeenCalledExactlyOnceWith([card], [brokenCard])
        expect(dependencies.importCharacter).not.toHaveBeenCalled()
        expect(dependencies.reportCharacterError).toHaveBeenCalledExactlyOnceWith(error, expect.any(Number))

        // A lone file Android could not copy keeps the restore route's own alert.
        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-failure',
            ready: [],
            failures: [brokenCard],
        }, dependencies)
        expect(dependencies.enqueueRestore).toHaveBeenLastCalledWith({
            requestId: 'opened-failure',
            ready: [],
            failures: [brokenCard],
        })
        expect(dependencies.importContentBatch).toHaveBeenCalledOnce()
    })

    it('sends every recognized Android content extension to the content import', async () => {
        const enqueueRestore = vi.fn(async () => undefined)
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore,
            importCharacter: vi.fn(async () => ({ kind: 'declined' as const })),
            importContentBatch: vi.fn(async () => null),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const ready = [
            `${'a'.repeat(174)}.charx`,
            'card.json',
            'card.jpg',
            'card.JPEG',
            `${'p'.repeat(174)}.PnG`,
            `${'m'.repeat(174)}.RiSuM`,
            'book.LoReBoOk',
        ].map((displayName, index) => ({
            token: `00000000-0000-4000-8000-00000000000${index + 1}`,
            displayName,
            bytes: 10,
        }))

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-2',
            ready,
            failures: [],
        }, dependencies)

        expect(dependencies.importContentBatch).toHaveBeenCalledExactlyOnceWith(ready, [])
        expect(dependencies.importCharacter).not.toHaveBeenCalled()
        expect(enqueueRestore).toHaveBeenCalledExactlyOnceWith({
            requestId: 'opened-2',
            ready: [],
            failures: [],
        })
    })

    it('reports the explicit destination result for an ordinary Android JPEG', async () => {
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async () => ({ kind: 'destination-required' as const })),
            importContentBatch: vi.fn(),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'portrait.jpeg',
            bytes: 10,
        }

        await dispatchAndroidOpenedSpoolBatch({
            requestId: 'opened-jpeg',
            ready: [source],
            failures: [],
        }, dependencies)

        expect(dependencies.reportDestinationRequired).toHaveBeenCalledExactlyOnceWith(source, expect.any(Number))
        expect(dependencies.reportCharacterError).not.toHaveBeenCalled()
    })

    it('deduplicates a replayed character token for one dispatcher lifetime', async () => {
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async () => ({ kind: 'declined' as const })),
            importContentBatch: vi.fn(async () => null),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const dispatcher = createAndroidOpenedSpoolDispatcher(dependencies)
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'module.RISUM',
            bytes: 10,
        }
        const [first, second, third] = ['a', 'b', 'c'].map((name, index) => ({
            token: `0000000${index}-0000-4000-8000-000000000000`,
            displayName: `${name}.charx`,
            bytes: 10,
        }))

        await dispatcher.enqueue({ requestId: 'opened-1', ready: [source], failures: [] })
        await dispatcher.enqueue({ requestId: 'replayed-1', ready: [source], failures: [] })
        await dispatcher.enqueue({ requestId: 'opened-2', ready: [first, second], failures: [] })
        await dispatcher.enqueue({ requestId: 'replayed-2', ready: [first, second, third], failures: [] })

        expect(vi.mocked(dependencies.importCharacter).mock.calls).toEqual([[source], [third]])
        expect(dependencies.importContentBatch).toHaveBeenCalledExactlyOnceWith([first, second], [])
    })

    it('keeps distinct character tokens with the same name in FIFO order', async () => {
        let releaseFirst!: () => void
        const calls: string[] = []
        const dependencies: AndroidOpenedSpoolDispatchDependencies = {
            enqueueRestore: vi.fn(async () => undefined),
            importCharacter: vi.fn(async (source) => {
                calls.push(source.token)
                if (calls.length === 1) {
                    await new Promise<void>((resolve) => releaseFirst = resolve)
                }
                return { kind: 'declined' as const }
            }),
            importContentBatch: vi.fn(),
            reportCharacterError: vi.fn(),
            reportDestinationRequired: vi.fn(),
        }
        const dispatcher = createAndroidOpenedSpoolDispatcher(dependencies)
        const first = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'card.PNG',
            bytes: 10,
        }
        const second = {
            token: '22222222-2222-4222-8222-222222222222',
            displayName: 'card.PNG',
            bytes: 10,
        }

        const firstBatch = dispatcher.enqueue({
            requestId: 'opened-1',
            ready: [first],
            failures: [],
        })
        await vi.waitFor(() => expect(calls).toEqual([first.token]))
        const secondBatch = dispatcher.enqueue({
            requestId: 'opened-2',
            ready: [second],
            failures: [],
        })
        await Promise.resolve()
        expect(calls).toEqual([first.token])

        releaseFirst()
        await Promise.all([firstBatch, secondBatch])
        expect(calls).toEqual([first.token, second.token])
    })

    it.each([
        ['png-card', 'character'] as const,
        ['risu-module', 'module'] as const,
    ])('prepares one Android token once and activates %s as %s', async (format, expected) => {
        const cancel = vi.fn(async () => undefined)
        const confirmActivated = vi.fn(async () => undefined)
        const receipt = {
            content: {
                casSessionId: 'session-1',
                format,
                metadata: {},
                assets: [],
                ...(format === 'risu-module'
                    ? { ownerHead: { present: false, manifestHash: null, entryCount: 0 } }
                    : {}),
            } as PreparedNativeContent,
            cancel,
            confirmActivated,
        } as unknown as PreparedNativeContentReceipt
        const prepare = vi.fn(async () => receipt)
        const activateCharacter = vi.fn(async () => ({ characterId: 'character-1' }))
        const activateModule = vi.fn(async () => ({ moduleId: 'module-1' }))
        const source = {
            token: '11111111-1111-4111-8111-111111111111',
            displayName: format === 'risu-module' ? 'module.RISUM' : 'card.PNG',
            bytes: 10,
        }

        const result = await importAndroidOpenedPreparedContent(source, {
            prepare,
            activateCharacter,
            activateModule,
        })

        expect(prepare).toHaveBeenCalledExactlyOnceWith(
            { type: 'androidSpool', token: source.token },
            source.displayName,
            { onStatus: expect.any(Function) },
        )
        expect(activateCharacter).toHaveBeenCalledTimes(expected === 'character' ? 1 : 0)
        expect(activateModule).toHaveBeenCalledTimes(expected === 'module' ? 1 : 0)
        expect(result).toEqual({
            kind: 'imported',
            value: expected === 'character' ? 'character-1' : 'module-1',
        })
        expect(confirmActivated).toHaveBeenCalledOnce()
        expect(cancel).not.toHaveBeenCalled()
    })

    it.each([
        ['card.PNG', 'risu-module', 'module'] as const,
        ['module.RISUM', 'png-card', 'character'] as const,
    ])(
        'uses native %s classifier result %s to activate %s',
        async (displayName, format, expected) => {
            const cancel = vi.fn(async () => undefined)
            const confirmActivated = vi.fn(async () => undefined)
            const receipt = {
                content: {
                    casSessionId: 'session-crossed',
                    format,
                    metadata: {},
                    assets: [],
                    ...(format === 'risu-module'
                        ? { ownerHead: { present: false, manifestHash: null, entryCount: 0 } }
                        : {}),
                } as PreparedNativeContent,
                cancel,
                confirmActivated,
            } as unknown as PreparedNativeContentReceipt
            const prepare = vi.fn(async () => receipt)
            const activateCharacter = vi.fn(async () => ({ characterId: 'character-crossed' }))
            const activateModule = vi.fn(async () => ({ moduleId: 'module-crossed' }))
            const enqueueRestore = vi.fn(async () => undefined)
            const source = {
                token: '55555555-5555-4555-8555-555555555555',
                displayName,
                bytes: 10,
            }
            const importCharacter = vi.fn(async (openedSource) =>
                await importAndroidOpenedPreparedContent(openedSource, {
                    prepare,
                    activateCharacter,
                    activateModule,
                }))

            await dispatchAndroidOpenedSpoolBatch({
                requestId: 'opened-crossed-classifier',
                ready: [source],
                failures: [],
            }, {
                enqueueRestore,
                importCharacter,
                importContentBatch: vi.fn(),
                reportCharacterError: vi.fn(),
                reportDestinationRequired: vi.fn(),
            })

            expect(enqueueRestore).toHaveBeenCalledExactlyOnceWith({
                requestId: 'opened-crossed-classifier',
                ready: [],
                failures: [],
            })
            expect(importCharacter).toHaveBeenCalledExactlyOnceWith(source)
            expect(prepare).toHaveBeenCalledExactlyOnceWith(
                { type: 'androidSpool', token: source.token },
                displayName,
                { onStatus: expect.any(Function) },
            )
            expect(activateCharacter).toHaveBeenCalledTimes(expected === 'character' ? 1 : 0)
            expect(activateModule).toHaveBeenCalledTimes(expected === 'module' ? 1 : 0)
            expect(confirmActivated).toHaveBeenCalledOnce()
            expect(cancel).not.toHaveBeenCalled()
        },
    )

    it('prepares and activates the native spools of a shared batch inside the batch operation', async () => {
        const confirmActivated = vi.fn(async () => undefined)
        backupMocks.prepare.mockReset().mockResolvedValue({
            content: { casSessionId: 'session-batch', format: 'risu-module', metadata: {}, assets: [],
                ownerHead: { present: false, manifestHash: null, entryCount: 0 } },
            cancel: vi.fn(async () => undefined),
            confirmActivated,
        })
        backupMocks.activateModule.mockReset().mockResolvedValue({ moduleId: 'module-batch' })
        backupMocks.activateCharacter.mockReset()
        backupMocks.externalManager.mockClear()
        const signal = new AbortController().signal
        const source = { token: '11111111-1111-4111-8111-111111111111', displayName: 'module.risum', bytes: 10 }
        const failures = [{ displayName: 'broken.charx', code: 'source-copy-failed' }]
        backupMocks.openedBatch.mockReset().mockImplementation(async (_sources, _failures, importPrepared) =>
            await importPrepared(source, { signal, onStatus: vi.fn() }))

        await expect(importAndroidContentSpoolBatch([source], failures)).resolves.toBe('module-batch')

        expect(backupMocks.openedBatch).toHaveBeenCalledExactlyOnceWith([source], failures, expect.any(Function))
        expect(backupMocks.prepare).toHaveBeenCalledExactlyOnceWith(
            { type: 'androidSpool', token: source.token },
            source.displayName,
            expect.objectContaining({ signal }),
        )
        expect(backupMocks.activateModule).toHaveBeenCalledOnce()
        expect(confirmActivated).toHaveBeenCalledOnce()
        // The batch owns the operation; a native file does not open one of its own.
        expect(backupMocks.externalManager).not.toHaveBeenCalled()
    })

    it('cancels a prepared Android token when activation is declined', async () => {
        const cancel = vi.fn(async () => undefined)
        const confirmActivated = vi.fn(async () => undefined)
        const prepare = vi.fn(async () => ({
            content: {
                casSessionId: 'session-1',
                format: 'risu-module',
                metadata: {},
                assets: [],
                ownerHead: { present: false, manifestHash: null, entryCount: 0 },
            },
            cancel,
            confirmActivated,
        } as unknown as PreparedNativeContentReceipt))

        const result = await importAndroidOpenedPreparedContent({
            token: '11111111-1111-4111-8111-111111111111',
            displayName: 'module.risum',
            bytes: 10,
        }, {
            prepare,
            activateCharacter: vi.fn(async () => ({ characterId: 'unused' })),
            activateModule: vi.fn(async () => null),
        })

        expect(result).toEqual({ kind: 'declined' })
        expect(prepare).toHaveBeenCalledOnce()
        expect(cancel).toHaveBeenCalledOnce()
        expect(confirmActivated).not.toHaveBeenCalled()
    })
})

it.each([['synthetic.JSON', 'auto'], ['synthetic.lorebook', 'module']] as const)(
    'classifies external %s before a native job can claim its token', async (displayName, destination) => {
        const source = { token: 'synthetic-token', displayName, bytes: 100 }
        backupMocks.replay.mockReset().mockResolvedValue('imported-id')
        backupMocks.externalManager.mockClear()
        await expect(importAndroidCharacterSpool(source)).resolves.toEqual({ kind: 'imported', value: 'imported-id' })
        expect(backupMocks.replay).toHaveBeenCalledExactlyOnceWith(source, destination)
        expect(backupMocks.externalManager).not.toHaveBeenCalled()
    },
)

it('localizes external preparation failures without exposing exception diagnostics', () => {
    backupMocks.presented.mockReturnValue(false)
    vi.mocked(alertError).mockClear()
    showAndroidFileError(Object.assign(new Error('private diagnostic source-busy'), { code: 'source-busy' }))
    expect(alertError).toHaveBeenCalledExactlyOnceWith('localized:source-busy')
    vi.mocked(alertError).mockClear()
    showAndroidFileError(new Error('private diagnostic'))
    expect(alertError).toHaveBeenCalledExactlyOnceWith('localized:unknown')
})
it('does not duplicate a fresh managed dialog failure', () => {
    backupMocks.presented.mockReturnValue(true)
    vi.mocked(alertError).mockClear()
    showAndroidFileError(new Error('managed'), 123)
    expect(backupMocks.presented).toHaveBeenLastCalledWith('import', 123)
    expect(alertError).not.toHaveBeenCalled()
    backupMocks.presented.mockReturnValue(false)
})

it('keeps the managed plain-JPEG failure as the sole outcome', () => {
    const source = { token: 'synthetic-token', displayName: 'photo.jpg', bytes: 100 }
    backupMocks.presented.mockReturnValue(true)
    vi.mocked(alertError).mockClear()
    showDestinationRequired(source, 123)
    expect(backupMocks.presented).toHaveBeenLastCalledWith('import', 123)
    expect(alertError).not.toHaveBeenCalled()
    backupMocks.presented.mockReturnValue(false)
    showDestinationRequired(source, 456)
    expect(alertError).toHaveBeenCalledExactlyOnceWith('photo.jpg: localized:destination-required')
})

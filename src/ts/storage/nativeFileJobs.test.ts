import { describe, expect, it, vi } from 'vitest'

import {
    continueNativeOfficialPublication,
    NativeFileJobActivationCommittedError,
    NativeFileJobError,
    runNativeOfficialAccountSnapshotRestore,
    runNativeBlockRisuSaveRestore,
    runNativeArchiveExport,
    runNativeArchiveRestore,
    runNativeArchiveReferenceExport,
    runNativeBlockRisuSaveExport,
    runNativeLegacyLocalBackupExport,
    runNativeCompatibleLocalBackupExport,
    runNativeLegacyLocalBackupRestore,
    runNativeCharacterCharxExport,
    runNativeCharacterCardExport,
    runNativeRisuModuleExport,
    runNativeOfficialPublicationAttempt,
    resumeNativeOfficialPublication,
    type NativeFileJobStatus,
    type NativeOfficialPublicationAttemptResult,
    type NativeOfficialPublicationRetryRequest,
    type NativeStagedPluginChoice,
    type NativeStagedPluginPreview,
} from './nativeFileJobs'
import {
    continueCommittedWorkingSetRefresh,
    retryCommittedWorkingSetRefreshWithContinuation,
} from './committedWorkingSetContinuation'

function status(
    state: NativeFileJobStatus['state'],
    result?: NativeFileJobStatus['result'],
): NativeFileJobStatus {
    return {
        jobId: 'job-1',
        kind: 'restore-block-risu-save',
        state,
        phase: state === 'succeeded' ? 'complete' : 'reading-source',
        progress: {
            completedBytes: state === 'queued' ? 0 : 128,
            totalBytes: 128,
            completedItems: 0,
        },
        result,
    }
}

function restoreRuntime(
    revision: number,
    options: {
        capture?: (reason: string) => void | Promise<void>
        refresh?: (revision: number) => void | Promise<void>
        markRefreshRequired?: (revision: number, error: unknown) => void
        acquire?: () => void | Promise<void>
        release?: () => void
        projection?: 'applied' | 'refresh-required'
        /** Revision captured after explicit restore preparation and user choices. */
        fencedRevision?: number
    } = {},
) {
    let captures = 0
    return {
        getStorageAuthorityEpoch: () => 2,
        retryCommittedWorkingSetRefresh: async () => {
            await options.refresh?.(revision + 1)
            return {
                kind: 'committed' as const,
                revision: revision + 1,
                projection: options.projection ?? 'applied' as const,
            }
        },
        capturePersistentMutationToken: async (reason: string) => {
            await options.capture?.(reason)
            return {
                revision: captures++ > 0 ? options.fencedRevision ?? revision : revision,
                mutationGeneration: 1,
            }
        },
        markCommittedWorkingSetRefreshRequired: (
            committedRevision: number,
            error: unknown,
        ) => options.markRefreshRequired?.(committedRevision, error),
        acquireDestructiveReplacementFence: async (token: { revision: number; mutationGeneration: number }) => {
            await options.acquire?.()
            expect(token.revision).toBe(options.fencedRevision ?? revision)
            return {
                revision: token.revision,
                refreshCommittedWorkingSet: async (
                    committedRevision: number,
                ) => {
                    await options.refresh?.(committedRevision)
                    return {
                        kind: 'committed',
                        revision: committedRevision,
                        projection: options.projection ?? 'applied',
                    } as const
                },
                release: () => options.release?.(),
            }
        },
    }
}

function pluginValuePreview(keys: readonly string[]): NativeStagedPluginPreview {
    return {
        values: keys.map((key, index) => ({
            key,
            byteSize: 12 + index,
            valueType: 'json',
        })),
        pluginNames: ['provider-manager', 'yumi-translator'],
    }
}

/** One import that reaches the plugin value pass, answers it, and ends. */
async function restoreOverPluginValues(options: {
    jobId: string
    preview: NativeStagedPluginPreview
    fenceFails?: boolean
    answer: NativeStagedPluginChoice | null
    onOffer(remembered: NativeStagedPluginChoice | null | undefined): void
}): Promise<void> {
    const cancels = options.fenceFails === true || options.answer === null
    const statuses: NativeFileJobStatus[] = [
        {
            ...status('waitingForInput'),
            jobId: options.jobId,
            phase: 'awaiting-activation',
            pluginValuePreview: options.preview,
        },
        cancels
            ? {
                  ...status('cancelled'),
                  jobId: options.jobId,
                  phase: 'awaiting-activation',
              }
            : {
                  ...status('succeeded', {
                      revision: 9,
                      sourceBytes: 128,
                      sourceSha256: 'c'.repeat(64),
                      characterCount: 1,
                      presetCount: 0,
                      warningCodes: [],
                  }),
                  jobId: options.jobId,
              },
    ]
    const runtime = restoreRuntime(
        8,
        options.fenceFails
            ? {
                  acquire: () => {
                      throw new Error('another mutation advanced the revision')
                  },
              }
            : {},
    )
    const running = runNativeBlockRisuSaveRestore(
        runtime,
        { type: 'desktopPath', path: 'save.risudat' },
        {
            assignPluginValues: async (_preview, remembered) => {
                options.onOffer(remembered)
                return options.answer
            },
        },
        {
            isTauri: () => true,
            invoke: async (command) => {
                if (command === 'native_file_job_start')
                    return { jobId: options.jobId }
                if (command === 'native_file_job_status') return statuses.shift()
                if (command === 'native_plugin_values_assign') return undefined
                if (command === 'native_file_job_cancel') return 'requested'
                if (command === 'native_file_job_finalize') return 'requested'
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            },
            wait: async () => undefined,
        },
    )
    if (cancels) await expect(running).rejects.toThrow()
    else await running
}

describe('native file jobs', () => {
    it('flushes a device-section export without acquiring the old renderer replacement fence', async () => {
        localStorage.removeItem('risuNestPortableExportIntent')
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        let revision = 20
        const runtime = {
            ...restoreRuntime(20, {
                capture: () => {
                    throw new Error('old mutation token must not be captured')
                },
                acquire: () => {
                    throw new Error('old replacement fence must not be acquired')
                },
            }),
            get revision() {
                return revision
            },
            flushPendingData: async (reason: string) => {
                calls.push([`flush:${reason}`, undefined])
                revision = 21
            },
        }
        const result = {
            revision: 21,
            sourceBytes: 128,
            sourceSha256: 'd'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        await expect(
            runNativeArchiveExport(
                runtime,
                { type: 'desktopPath', path: 'C:\\chosen\\device.risunest' },
                { library: true, deviceSections: ['hypa'] },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command, args) => {
                        calls.push([command, args])
                        if (command === 'native_file_job_start')
                            return { jobId: 'device-export-1' }
                        if (command === 'native_file_job_status')
                            return {
                                ...status('succeeded', result),
                                jobId: 'device-export-1',
                                kind: 'export-portable-backup',
                            }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
                },
            ),
        ).resolves.toEqual(result)

        expect(calls.slice(0, 2)).toEqual([
            ['flush:native-portable-export', undefined],
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-portable-backup',
                        expectedRevision: 21,
                        selection: { library: true, deviceSections: ['hypa'] },
                        destination: 'C:\\chosen\\device.risunest',
                    },
                },
            ],
        ])
        expect(calls.map(([command]) => command)).not.toContain(
            'native_device_backup_bootstrap',
        )
        expect(localStorage.getItem('risuNestPortableExportIntent')).toBeNull()
    })

    it('starts a library-only portable export from an opaque conflict source', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const result = {
            revision: 0,
            sourceBytes: 128,
            sourceSha256: 'a'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        await expect(runNativeArchiveReferenceExport(
            { type: 'conflictReference', token: 'external:source-token' },
            { type: 'desktopPath', path: 'C:\\chosen\\conflict.risunest' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'export-1' }
                    if (command === 'native_file_job_status') return {
                        ...status('succeeded', result),
                        jobId: 'export-1',
                        kind: 'export-portable-backup',
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )).resolves.toEqual(result)

        expect(calls[0]).toEqual([
            'native_file_job_start',
            {
                request: {
                    kind: 'export-portable-backup',
                    source: { type: 'conflictReference', token: 'external:source-token' },
                    selection: { library: true, deviceSections: [] },
                    destination: 'C:\\chosen\\conflict.risunest',
                },
            },
        ])
        expect(JSON.stringify(calls[0])).not.toContain('expectedRevision')
    })

    it.each(['none', 'ack', 'open'] as const)(
        'acknowledges and reopens a committed portable device session before refresh (failure: %s)',
        async (failure) => {
            const calls: string[] = []
            const events: string[] = []
            let staleWritesBlocked = false
            let storeOpen = false
            let failAck = failure === 'ack'
            let failOpen = failure === 'open'
            const refresh = vi.fn(async () => {
                if (!storeOpen) throw new Error('persistent store has not been opened')
                events.push('working-set-refresh')
            })
            const markRefreshRequired = vi.fn((revision: number) => {
                expect(revision).toBe(4)
                staleWritesBlocked = true
                events.push('refresh-required')
            })
            const committed = {
                revision: 4,
                sourceBytes: 128,
                sourceSha256: 'b'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
            }
            const statuses: NativeFileJobStatus[] = [
                {
                    ...status('waitingForInput'),
                    kind: 'restore-portable-backup',
                    phase: 'awaiting-backup-selection',
                    restorePreview: {
                        libraryIncluded: true,
                        repairRequired: false,
                        deviceSections: ['hypa'],
                    },
                },
                {
                    ...status('waitingForInput'),
                    kind: 'restore-portable-backup',
                    phase: 'awaiting-activation',
                },
                {
                    ...status('succeeded', committed),
                    kind: 'restore-portable-backup',
                    deviceSessionId: 'device-session',
                },
            ]
            const invoke = async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_start') return { jobId: 'job-1' }
                if (command === 'native_file_job_status') return statuses.shift()
                if (command === 'native_portable_select_sections') return undefined
                if (command === 'native_file_job_finalize') return undefined
                if (command === 'native_device_backup_recovery_complete') {
                    events.push('device-ack')
                    if (failAck) throw new Error('synthetic ack failure')
                    return undefined
                }
                if (command === 'pds_open') {
                    events.push('persistent-store-open')
                    if (failOpen) throw new Error('synthetic open failure')
                    storeOpen = true
                    return { revision: 4 }
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }
            const runtime = restoreRuntime(3, {
                refresh,
                markRefreshRequired,
                release: () => {
                    if (failure !== 'none') expect(staleWritesBlocked).toBe(true)
                    events.push('fence-released')
                },
            })
            const running = runNativeArchiveRestore(
                runtime,
                { type: 'desktopPath', path: 'C:\\synthetic\\portable.risunest' },
                {
                    choosePortableSections: async () => ({
                        library: true,
                        deviceSections: ['hypa'],
                    }),
                    onNativeStatus: async (value) => {
                        if (value.state === 'succeeded') events.push('library-hold')
                    },
                },
                {
                    isTauri: () => true,
                    invoke,
                    wait: async () => undefined,
                },
            )

            if (failure !== 'none') {
                await expect(running).rejects.toBeInstanceOf(
                    NativeFileJobActivationCommittedError,
                )
                expect(refresh).not.toHaveBeenCalled()
                expect(markRefreshRequired).toHaveBeenCalledOnce()
                expect(staleWritesBlocked).toBe(true)
                expect(calls).not.toContain('native_file_job_forget')
                expect(calls.filter(command => command === 'native_file_job_finalize'))
                    .toHaveLength(1)
                failAck = false
                failOpen = false
                await expect(
                    retryCommittedWorkingSetRefreshWithContinuation(runtime),
                ).resolves.toMatchObject({ revision: 4, projection: 'applied' })
                expect(calls.filter(command => command === 'native_file_job_finalize'))
                    .toHaveLength(1)
                expect(calls.filter(command => command === 'native_file_job_start'))
                    .toHaveLength(1)
                expect(refresh).toHaveBeenCalledOnce()
                expect(calls).toContain('native_file_job_forget')
                expect(calls.filter(command => command === 'native_device_backup_recovery_complete'))
                    .toHaveLength(failure === 'ack' ? 2 : 1)
                expect(calls.filter(command => command === 'pds_open'))
                    .toHaveLength(failure === 'open' ? 2 : 1)
            } else {
                await expect(running).resolves.toEqual(committed)
                expect(refresh).toHaveBeenCalledOnce()
                expect(markRefreshRequired).not.toHaveBeenCalled()
                expect(calls).toContain('native_file_job_forget')
            }
            expect(events).toEqual(
                failure === 'ack'
                    ? [
                        'library-hold',
                        'device-ack',
                        'refresh-required',
                        'fence-released',
                        'device-ack',
                        'persistent-store-open',
                        'working-set-refresh',
                    ]
                    : failure === 'open'
                      ? [
                            'library-hold',
                            'device-ack',
                            'persistent-store-open',
                            'refresh-required',
                            'fence-released',
                            'persistent-store-open',
                            'working-set-refresh',
                        ]
                    : [
                        'library-hold',
                        'device-ack',
                        'persistent-store-open',
                        'working-set-refresh',
                        'fence-released',
                    ],
            )
        },
    )

    it.each(['chooser', 'fence'])(
        'keeps portable selection outside the replacement fence when aborted during %s',
        async (point) => {
            const abort = new AbortController()
            const commands: string[] = []
            const events: string[] = []
            const released = vi.fn()
            let polls = 0
            const preview = {
                ...status('waitingForInput'),
                kind: 'restore-portable-backup' as const,
                phase: 'awaiting-backup-selection' as const,
                restorePreview: {
                    libraryIncluded: true,
                    repairRequired: false,
                    deviceSections: ['hypa'],
                },
            }
            await expect(
                runNativeArchiveRestore(
                    restoreRuntime(3, {
                        acquire() {
                            if (point === 'fence') abort.abort()
                        },
                        release: released,
                    }),
                    {
                        type: 'desktopPath',
                        path: 'C:\\synthetic\\backup.risunest',
                    },
                    {
                        signal: abort.signal,
                        onBlockingChange: (blocking) => {
                            events.push(`blocking:${blocking}`)
                        },
                        choosePortableSections: async () => {
                            if (point === 'chooser') abort.abort()
                            return {
                                library: true,
                                deviceSections: ['hypa'],
                            }
                        },
                    },
                    {
                        isTauri: () => true,
                        wait: async () => {},
                        invoke: async (command) => {
                            commands.push(command)
                            if (command === 'native_file_job_start')
                                return { jobId: 'job-1' }
                            if (command === 'native_file_job_status') {
                                polls += 1
                                if (polls === 1) {
                                    return preview
                                }
                                if (point === 'fence' && polls === 2) {
                                    return {
                                        ...status('waitingForInput'),
                                        kind: 'restore-portable-backup',
                                        phase: 'awaiting-activation',
                                    }
                                }
                                return {
                                    ...status('cancelled'),
                                    kind: 'restore-portable-backup',
                                }
                            }
                            return true
                        },
                    },
                ),
            ).rejects.toMatchObject({ name: 'AbortError' })
            expect(commands).toContain('native_file_job_cancel')
            expect(commands.includes('native_portable_select_sections')).toBe(
                point === 'fence',
            )
            if (point === 'fence') {
                expect(events).toEqual(['blocking:true', 'blocking:false'])
                expect(commands.indexOf('native_portable_select_sections')).toBeLessThan(
                    commands.indexOf('native_file_job_cancel'),
                )
            } else {
                expect(events).toEqual([])
            }
            expect(released).toHaveBeenCalledTimes(point === 'fence' ? 1 : 0)
        },
    )
    it('exports one leased character CharX without payload bytes in IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const terminal: NativeFileJobStatus = {
            jobId: 'character-export',
            kind: 'export-character-charx',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 8192,
                totalBytes: 8192,
                completedItems: 3,
                totalItems: 3,
            },
            result: {
                revision: 31,
                sourceBytes: 8192,
                sourceSha256: 'c'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
            },
        }
        const card = {
            spec: 'chara_card_v3',
            spec_version: '3.0',
            data: {
                name: 'Leased',
                extensions: { risuai: {} },
                assets: [
                    {
                        type: 'icon',
                        uri: 'ccdefault:',
                        name: 'main',
                        ext: 'png',
                    },
                ],
            },
        }
        const module = {
            name: 'Leased Module',
            description: 'Module for Leased',
            id: 'module-id',
            trigger: [],
            regex: [],
            lorebook: [],
        }

        const result = await runNativeCharacterCharxExport(
            {
                characterId: 'character-id',
                destination: {
                    type: 'desktopPath',
                    path: 'C:\\chosen\\Leased.charx',
                },
                expectedRevision: 31,
                card,
                module,
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'character-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )

        expect(result).toEqual(terminal.result)
        expect(calls).toEqual([
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-character-charx',
                        destination: 'C:\\chosen\\Leased.charx',
                        expectedRevision: 31,
                        characterId: 'character-id',
                        card,
                        module,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'character-export' }],
            ['native_file_job_forget', { jobId: 'character-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('hands a managed character CharX export to Android SAF and cleans the native source', async () => {
        const events: string[] = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174002.charx'
        const terminal: NativeFileJobStatus = {
            jobId: 'character-export',
            kind: 'export-character-charx',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4096, completedItems: 3 },
            result: {
                revision: 31,
                sourceBytes: 4096,
                sourceSha256: 'c'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }

        const result = await runNativeCharacterCharxExport(
            {
                characterId: 'character-id',
                destination: {
                    type: 'androidSaf',
                    suggestedName: 'Leased.charx',
                },
                expectedRevision: 31,
                card: { spec: 'chara_card_v3' },
                module: {},
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    events.push(`${command}:${JSON.stringify(args ?? {})}`)
                    if (command === 'native_file_job_start')
                        return { jobId: 'character-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_character_charx_handoff_cleanup')
                        return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async (request) => {
                    events.push(
                        `saf:${request.sourcePath}:${request.suggestedName}`,
                    )
                    return {
                        bytes: 4096,
                        warningCodes: ['android-saf-provider-not-atomic'],
                    }
                },
            },
        )

        expect(result.handoffPath).toBeUndefined()
        expect(result.warningCodes).toEqual(['android-saf-provider-not-atomic'])
        expect(events).toEqual([
            'native_file_job_start:{"request":{"kind":"export-character-charx","expectedRevision":31,"characterId":"character-id","card":{"spec":"chara_card_v3"},"module":{}}}',
            'native_file_job_status:{"jobId":"character-export"}',
            `saf:${handoffPath}:Leased.charx`,
            `native_character_charx_handoff_cleanup:{"path":"${handoffPath.replaceAll('\\', '\\\\')}"}`,
            'native_file_job_forget:{"jobId":"character-export"}',
        ])
    })

    it('retains a successful Android CharX job when native source cleanup fails', async () => {
        const commands: string[] = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174005.charx'
        const terminal: NativeFileJobStatus = {
            jobId: 'character-export',
            kind: 'export-character-charx',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4096, completedItems: 3 },
            result: {
                revision: 31,
                sourceBytes: 4096,
                sourceSha256: 'c'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }

        const result = await runNativeCharacterCharxExport(
            {
                characterId: 'character-id',
                destination: {
                    type: 'androidSaf',
                    suggestedName: 'Leased.charx',
                },
                expectedRevision: 31,
                card: { spec: 'chara_card_v3' },
                module: {},
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'character-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_character_charx_handoff_cleanup') {
                        throw new Error('handoff is still in use')
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({
                    bytes: 4096,
                    warningCodes: [],
                }),
            },
        )

        expect(result.warningCodes).toEqual(['cleanup-failed'])
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_character_charx_handoff_cleanup',
        ])
    })

    it('rejects a short Android CharX handoff and still cleans both native receipts', async () => {
        const commands: string[] = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174003.charx'
        const terminal: NativeFileJobStatus = {
            jobId: 'character-export',
            kind: 'export-character-charx',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4096, completedItems: 3 },
            result: {
                revision: 31,
                sourceBytes: 4096,
                sourceSha256: 'c'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }

        await expect(
            runNativeCharacterCharxExport(
                {
                    characterId: 'character-id',
                    destination: {
                        type: 'androidSaf',
                        suggestedName: 'Leased.charx',
                    },
                    expectedRevision: 31,
                    card: { spec: 'chara_card_v3' },
                    module: {},
                },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'character-export' }
                        if (command === 'native_file_job_status')
                            return terminal
                        if (
                            command === 'native_character_charx_handoff_cleanup'
                        )
                            return undefined
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    copyToAndroidSaf: async () => ({
                        bytes: 2048,
                        warningCodes: ['provider-warning'],
                    }),
                },
            ),
        ).rejects.toMatchObject({
            code: 'length-mismatch',
            warningCodes: ['provider-warning', 'partial-destination-may-remain'],
        })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_character_charx_handoff_cleanup',
            'native_file_job_forget',
        ])
    })

    it('hands a descriptor-only JSON character card to Android SAF and cleans both receipts', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-character-card-123e4567-e89b-42d3-a456-426614174006.json'
        const terminal: NativeFileJobStatus = {
            jobId: 'json-export',
            kind: 'export-character-card',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 2048, completedItems: 2 },
            result: {
                revision: 44,
                sourceBytes: 2048,
                sourceSha256: 'd'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }
        const metadata = {
            spec: 'chara_card_v3',
            spec_version: '3.0',
            data: { name: 'JSON', assets: [{ uri: 'asset-key' }] },
        }

        const result = await runNativeCharacterCardExport(
            {
                characterId: 'json-character',
                destination: { type: 'androidSaf', suggestedName: 'JSON.json' },
                expectedRevision: 44,
                format: 'json-card',
                metadata,
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'json-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_character_card_handoff_cleanup')
                        return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({
                    bytes: 2048,
                    warningCodes: ['android-saf-provider-not-atomic'],
                }),
            },
        )

        expect(result.handoffPath).toBeUndefined()
        expect(result.warningCodes).toEqual(['android-saf-provider-not-atomic'])
        expect(calls).toEqual([
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-character-card',
                        expectedRevision: 44,
                        characterId: 'json-character',
                        format: 'json-card',
                        metadata,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'json-export' }],
            ['native_character_card_handoff_cleanup', { path: handoffPath }],
            ['native_file_job_forget', { jobId: 'json-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        expect(JSON.stringify(calls)).not.toContain('data:image')
    })

    it('hands an exact-index RISUM descriptor to Android SAF and cleans both receipts', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-module-123e4567-e89b-42d3-a456-426614174006.risum'
        const result = await runNativeRisuModuleExport(
            {
                moduleIndex: 7,
                destination: {
                    type: 'androidSaf',
                    suggestedName: 'Module.risum',
                },
                expectedRevision: 44,
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'risum-export' }
                    if (command === 'native_file_job_status')
                        return {
                            jobId: 'risum-export',
                            kind: 'export-risu-module',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: { completedBytes: 9, completedItems: 3 },
                            result: {
                                revision: 44,
                                sourceBytes: 9,
                                sourceSha256: 'e'.repeat(64),
                                characterCount: 0,
                                presetCount: 0,
                                warningCodes: [],
                                handoffPath,
                            },
                        }
                    if (command === 'native_risu_module_handoff_cleanup')
                        return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 9, warningCodes: [] }),
            },
        )
        expect(result.handoffPath).toBeUndefined()
        expect(calls).toEqual([
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-risu-module',
                        expectedRevision: 44,
                        moduleIndex: 7,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'risum-export' }],
            ['native_risu_module_handoff_cleanup', { path: handoffPath }],
            ['native_file_job_forget', { jobId: 'risum-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('assigns the plugin values a save left unowned before the replacement is applied', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation',
                pluginValuePreview: {
                    values: [
                        { key: 'pm_store', byteSize: 24, valueType: 'json' },
                        { key: 'yt_glossary', byteSize: 12, valueType: 'string' },
                    ],
                    pluginNames: ['provider-manager', 'yumi-translator'],
                },
            },
            {
                ...status('succeeded', {
                    revision: 9,
                    sourceBytes: 128,
                    sourceSha256: 'c'.repeat(64),
                    characterCount: 1,
                    presetCount: 0,
                    warningCodes: [],
                }),
            },
        ]

        await runNativeBlockRisuSaveRestore(
            restoreRuntime(8),
            { type: 'desktopPath', path: 'save.risudat' },
            {
                assignPluginValues: async (preview) => {
                    expect(preview.values.map((value) => value.key)).toEqual([
                        'pm_store',
                        'yt_glossary',
                    ])
                    expect(preview.pluginNames).toEqual([
                        'provider-manager',
                        'yumi-translator',
                    ])
                    return {
                        assignments: [
                            { owner: 'provider-manager', keys: ['pm_store'] },
                        ],
                        automatic: false,
                    }
                },
            },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') return statuses.shift()
                    if (command === 'native_plugin_values_assign') return undefined
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        const assigned = calls.find(([command]) => command === 'native_plugin_values_assign')
        expect(assigned?.[1]).toEqual({
            jobId: 'job-1',
            assignments: [{ owner: 'provider-manager', keys: ['pm_store'] }],
            automatic: false,
        })
        const order = calls.map(([command]) => command)
        expect(order.indexOf('native_plugin_values_assign')).toBeLessThan(
            order.indexOf('native_file_job_finalize'),
        )
    })

    it('cancels the import when the plugin value pass is closed without an answer', async () => {
        const commands: string[] = []
        const acquire = vi.fn()
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation',
                pluginValuePreview: {
                    values: [{ key: 'pm_store', byteSize: 24, valueType: 'json' }],
                    pluginNames: [],
                },
            },
            { ...status('cancelled'), phase: 'awaiting-activation' },
        ]

        await expect(
            runNativeBlockRisuSaveRestore(
                restoreRuntime(8, { acquire }),
                { type: 'desktopPath', path: 'save.risudat' },
                { assignPluginValues: async () => null },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start') return { jobId: 'job-1' }
                        if (command === 'native_file_job_status') return statuses.shift()
                        if (command === 'native_file_job_cancel') return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toThrow()

        expect(commands).toContain('native_file_job_cancel')
        expect(commands).not.toContain('native_plugin_values_assign')
        expect(commands).not.toContain('native_file_job_finalize')
        expect(acquire).not.toHaveBeenCalled()
    })

    it('keeps the plugin value answers for the attempt after a lost fence', async () => {
        const preview = pluginValuePreview(['pm_store', 'pm_keys'])
        const answer: NativeStagedPluginChoice = {
            assignments: [{ owner: 'provider-manager', keys: ['pm_store'] }],
            automatic: false,
        }
        const offered: (NativeStagedPluginChoice | null | undefined)[] = []

        await restoreOverPluginValues({
            jobId: 'job-1',
            preview,
            fenceFails: true,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })
        await restoreOverPluginValues({
            jobId: 'job-2',
            preview,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })

        expect(offered[1]).toEqual(answer)
        expect(offered[0]).toBeNull()
    })

    it('drops the plugin value answers once the import carrying them is applied', async () => {
        const preview = pluginValuePreview(['yt_glossary', 'yt_terms'])
        const answer: NativeStagedPluginChoice = {
            assignments: [{ owner: 'yumi-translator', keys: ['yt_glossary'] }],
            automatic: true,
        }
        const offered: (NativeStagedPluginChoice | null | undefined)[] = []

        await restoreOverPluginValues({
            jobId: 'job-1',
            preview,
            fenceFails: true,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })
        await restoreOverPluginValues({
            jobId: 'job-2',
            preview,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })
        await restoreOverPluginValues({
            jobId: 'job-3',
            preview,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })

        expect(offered[1]).toEqual(answer)
        expect(offered[2]).toBeNull()
    })

    it('offers nothing to a save that left a different set of values unowned', async () => {
        const answered = pluginValuePreview(['hp_index', 'hp_chunks'])
        const other = pluginValuePreview(['hp_index', 'hp_vectors'])
        const answer: NativeStagedPluginChoice = {
            assignments: [{ owner: 'provider-manager', keys: ['hp_index'] }],
            automatic: false,
        }
        const offered: (NativeStagedPluginChoice | null | undefined)[] = []

        await restoreOverPluginValues({
            jobId: 'job-1',
            preview: answered,
            fenceFails: true,
            answer,
            onOffer: (remembered) => offered.push(remembered),
        })
        await restoreOverPluginValues({
            jobId: 'job-2',
            preview: other,
            answer: null,
            onOffer: (remembered) => offered.push(remembered),
        })

        expect(offered[1]).toBeNull()
    })

    it('restores an official snapshot without transferring its database bytes through IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const events: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-official-account-snapshot',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 12,
                    sourceBytes: 16_384,
                    sourceSha256: 'b'.repeat(64),
                    characterCount: 4,
                    presetCount: 2,
                    warningCodes: [],
                }),
                kind: 'restore-official-account-snapshot',
            },
        ]

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, {
                acquire: () => {
                    events.push('fence-acquired')
                },
                refresh: () => {
                    events.push('refreshed')
                },
                release: () => {
                    events.push('fence-released')
                },
            }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            {
                afterRefresh: () => {
                    events.push('plugins-reloaded')
                },
            },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'official-restore' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toMatchObject({
            kind: 'activated',
            revision: 12,
        })
        expect(events).toEqual([
            'fence-acquired',
            'refreshed',
            'fence-released',
            'plugins-reloaded',
        ])
        expect(calls[0]).toEqual([
            'native_file_job_start',
            {
                request: {
                    kind: 'restore-official-account-snapshot',
                    baseUrl: 'https://hub.example',
                    credential: { kind: 'risu-auth', token: 'secret-token' },
                    expectedRevision: 11,
                },
            },
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        expect(JSON.stringify(calls)).not.toContain('databaseBytes')
    })

    it('returns a missing official snapshot without taking the replacement fence', async () => {
        const acquire = vi.fn()
        const commands: string[] = []

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, { acquire }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'official-missing' }
                    if (command === 'native_file_job_status')
                        return {
                            ...status('failed'),
                            kind: 'restore-official-account-snapshot',
                            state: 'failed',
                            phase: 'complete',
                            error: {
                                code: 'remote-missing',
                                message: 'No official account snapshot exists',
                            },
                        }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({ kind: 'missing' })
        expect(acquire).not.toHaveBeenCalled()
        expect(commands).not.toContain('native_file_job_finalize')
    })

    it('maps a legacy compatibility result without taking the replacement fence', async () => {
        const acquire = vi.fn()

        const result = await runNativeOfficialAccountSnapshotRestore(
            restoreRuntime(11, { acquire }),
            {
                baseUrl: 'https://hub.example',
                credential: { kind: 'risu-auth', token: 'secret-token' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    if (command === 'native_file_job_start')
                        return { jobId: 'official-legacy' }
                    if (command === 'native_file_job_status')
                        return {
                            ...status('failed'),
                            kind: 'restore-official-account-snapshot',
                            state: 'failed',
                            phase: 'complete',
                            error: {
                                code: 'compatibility-required',
                                message: 'Legacy snapshot requires preparation',
                            },
                        }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({ kind: 'compatibility-fallback' })
        expect(acquire).not.toHaveBeenCalled()
    })

    it('restores a legacy local backup through the destructive replacement fence without payload IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-legacy-local-backup',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 12,
                    sourceBytes: 8192,
                    sourceSha256: 'c'.repeat(64),
                    characterCount: 4,
                    presetCount: 2,
                    warningCodes: [],
                }),
                kind: 'restore-legacy-local-backup',
            },
        ]

        const result = await runNativeLegacyLocalBackupRestore(
            restoreRuntime(11),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'legacy-restore' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(12)
        expect(calls[0]).toEqual([
            'native_file_job_start',
            {
                request: {
                    kind: 'restore-legacy-local-backup',
                    source: {
                        type: 'desktopPath',
                        path: 'C:\\chosen\\backup.bin',
                    },
                    expectedRevision: 11,
                },
            },
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it.each(['risuai', 'pocketrisu'] as const)(
        'exports %s with the flushed revision and reports losses through status',
        async (target) => {
            for (const destination of [
                {
                    type: 'desktopPath',
                    path: 'C:\\chosen\\compatible.bin',
                } as const,
                {
                    type: 'androidSaf',
                    suggestedName: `${target}-backup.bin`,
                } as const,
            ]) {
                const calls: Array<
                    [string, Record<string, unknown> | undefined]
                > = []
                const onStatus = vi.fn()
                const runtime = {
                    revision: 20,
                    flushPendingData: async () => {
                        runtime.revision = 21
                    },
                }
                const report = {
                    target,
                    preserved: [],
                    converted: [],
                    excluded: [
                        {
                            code: 'unsupported-field',
                            items: '1',
                            bytes: '0',
                            affectedConversations:
                                target === 'risuai' ? '1' : null,
                        },
                    ],
                }
                const handoffPath = 'C:\\app\\handoffs\\risu-backup-123.bin'
                const copyToAndroidSaf = vi.fn(async () => ({
                    bytes: 4096,
                    warningCodes: [],
                }))
                const result = await runNativeCompatibleLocalBackupExport(
                    runtime,
                    target,
                    destination,
                    { onStatus },
                    {
                        isTauri: () => true,
                        invoke: async (command, args) => {
                            calls.push([command, args])
                            if (command === 'native_file_job_start')
                                return { jobId: 'compatible-export' }
                            if (command === 'native_file_job_status')
                                return {
                                    ...status('succeeded', {
                                        revision: 21,
                                        sourceBytes: 4096,
                                        sourceSha256: 'd'.repeat(64),
                                        characterCount: 1,
                                        presetCount: 0,
                                        warningCodes: ['compatibility-losses'],
                                        ...(destination.type === 'androidSaf'
                                            ? { handoffPath }
                                            : {}),
                                    }),
                                    jobId: 'compatible-export',
                                    kind: 'export-compatible-local-backup',
                                    compatibilityReport: report,
                                } satisfies NativeFileJobStatus
                            if (
                                command ===
                                    'native_legacy_backup_handoff_cleanup' ||
                                command === 'native_file_job_forget'
                            )
                                return true
                            throw new Error(`Unexpected command: ${command}`)
                        },
                        wait: async () => undefined,
                        copyToAndroidSaf,
                    },
                )
                expect(calls[0]).toEqual([
                    'native_file_job_start',
                    {
                        request: {
                            kind: 'export-compatible-local-backup',
                            target,
                            expectedRevision: 21,
                            ...(destination.type === 'desktopPath'
                                ? { destination: destination.path }
                                : {}),
                        },
                    },
                ])
                expect(onStatus).toHaveBeenCalledWith(
                    expect.objectContaining({ compatibilityReport: report }),
                )
                expect(result.revision).toBe(21)
                expect(result.handoffPath).toBeUndefined()
                if (destination.type === 'androidSaf') {
                    expect(copyToAndroidSaf).toHaveBeenCalledWith(
                        expect.objectContaining({
                            sourcePath: handoffPath,
                            suggestedName: destination.suggestedName,
                        }),
                    )
                    expect(calls).toContainEqual([
                        'native_legacy_backup_handoff_cleanup',
                        { path: handoffPath },
                    ])
                } else {
                    expect(copyToAndroidSaf).not.toHaveBeenCalled()
                }
            }
        },
    )

    it('exports a legacy local backup using only a destination path and revision over IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const result = await runNativeLegacyLocalBackupExport(
            {
                revision: 21,
                flushPendingData: async (reason) => {
                    calls.push([`flush:${reason}`, undefined])
                },
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'legacy-export' }
                    if (command === 'native_file_job_status') {
                        return {
                            jobId: 'legacy-export',
                            kind: 'export-legacy-local-backup',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: {
                                completedBytes: 4096,
                                completedItems: 3,
                            },
                            result: {
                                revision: 21,
                                sourceBytes: 4096,
                                sourceSha256: 'd'.repeat(64),
                                characterCount: 2,
                                presetCount: 1,
                                warningCodes: [],
                            },
                        } satisfies NativeFileJobStatus
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )

        expect(result.sourceBytes).toBe(4096)
        expect(calls).toEqual([
            ['flush:native-legacy-local-backup-export', undefined],
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-legacy-local-backup',
                        destination: 'C:\\chosen\\backup.bin',
                        expectedRevision: 21,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'legacy-export' }],
            ['native_file_job_forget', { jobId: 'legacy-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('publishes a native legacy backup handoff through Android SAF without archive bytes over IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const copyToAndroidSaf = vi.fn(async () => ({
            bytes: 4096,
            warningCodes: [],
        }))

        const result = await runNativeLegacyLocalBackupExport(
            { revision: 21, flushPendingData: async () => undefined },
            { type: 'androidSaf', suggestedName: 'risu-backup.bin' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'legacy-export' }
                    if (command === 'native_file_job_status') {
                        return {
                            jobId: 'legacy-export',
                            kind: 'export-legacy-local-backup',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: {
                                completedBytes: 4096,
                                completedItems: 3,
                            },
                            result: {
                                revision: 21,
                                sourceBytes: 4096,
                                sourceSha256: 'd'.repeat(64),
                                characterCount: 2,
                                presetCount: 1,
                                warningCodes: [],
                                handoffPath:
                                    'C:\\app\\handoffs\\risu-backup.bin',
                            },
                        } satisfies NativeFileJobStatus
                    }
                    if (command === 'native_legacy_backup_handoff_cleanup')
                        return true
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf,
            },
        )

        expect(result.handoffPath).toBeUndefined()
        expect(copyToAndroidSaf).toHaveBeenCalledWith(
            expect.objectContaining({
                sourcePath: 'C:\\app\\handoffs\\risu-backup.bin',
                suggestedName: 'risu-backup.bin',
            }),
        )
        expect(calls[0]).toEqual([
            'native_file_job_start',
            {
                request: {
                    kind: 'export-legacy-local-backup',
                    expectedRevision: 21,
                },
            },
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('keeps unavailable legacy backup capability as a structured native error', async () => {
        const calls: string[] = []

        await expect(
            runNativeLegacyLocalBackupExport(
                {
                    revision: 5,
                    flushPendingData: async () => undefined,
                },
                {
                    type: 'desktopPath',
                    path: 'C:\\chosen\\backup.bin',
                },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        calls.push(command)
                        throw {
                            code: 'capability-unavailable',
                            message:
                                'native legacy backup requires v2 asset and cold authority',
                        }
                    },
                    wait: async () => undefined,
                    copyToAndroidSaf: async () => ({
                        bytes: 0,
                        warningCodes: [],
                    }),
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'capability-unavailable',
        })
        expect(calls).toEqual(['native_file_job_start'])
    })

    it('restores a compatible package through the existing destructive replacement fence', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const events: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-legacy-local-backup',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 18,
                    sourceBytes: 4096,
                    sourceSha256: '8'.repeat(64),
                    characterCount: 3,
                    presetCount: 2,
                    warningCodes: [],
                }),
                kind: 'restore-legacy-local-backup',
            },
        ]

        const result = await runNativeLegacyLocalBackupRestore(
            restoreRuntime(17, {
                acquire: () => {
                    events.push('fence-acquired')
                },
                refresh: () => {
                    events.push('refreshed')
                },
                release: () => {
                    events.push('fence-released')
                },
            }),
            {
                type: 'androidSpool',
                token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
            },
            {
                beforeActivation: () => {
                    events.push('fresh-status')
                },
                afterRefresh: () => {
                    events.push('plugins-reloaded')
                },
            },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'lossless-restore' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(18)

        expect(events).toEqual([
            'fresh-status',
            'fence-acquired',
            'refreshed',
            'fence-released',
            'plugins-reloaded',
        ])
        expect(calls).toEqual([
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'restore-legacy-local-backup',
                        source: {
                            type: 'androidSpool',
                            token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
                        },
                        expectedRevision: 17,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'lossless-restore' }],
            [
                'native_file_job_finalize',
                { jobId: 'lossless-restore', expectedRevision: 17 },
            ],
            ['native_file_job_status', { jobId: 'lossless-restore' }],
            ['native_file_job_forget', { jobId: 'lossless-restore' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('retains a committed backup restore when renderer refresh fails', async () => {
        const commands: string[] = []
        const statuses: NativeFileJobStatus[] = [
            {
                ...status('waitingForInput'),
                kind: 'restore-legacy-local-backup',
                phase: 'awaiting-activation',
            },
            {
                ...status('succeeded', {
                    revision: 19,
                    sourceBytes: 4096,
                    sourceSha256: '9'.repeat(64),
                    characterCount: 3,
                    presetCount: 2,
                    warningCodes: [],
                }),
                kind: 'restore-legacy-local-backup',
            },
        ]

        await expect(
            runNativeLegacyLocalBackupRestore(
                restoreRuntime(18, {
                    refresh: () => {
                        throw new Error('refresh failed')
                    },
                }),
                {
                    type: 'desktopPath',
                    path: 'C:\\chosen\\backup.bin',
                },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'lossless-restore' }
                        if (command === 'native_file_job_status')
                            return statuses.shift()
                        if (command === 'native_file_job_finalize')
                            return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobActivationCommittedError',
            committedRevision: 19,
            recoveryRequired: true,
        })
        expect(commands).not.toContain('native_file_job_forget')
    })

    it('exports a complete compatible package to a desktop destination without bytes in IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-legacy-local-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 8192,
                totalBytes: 8192,
                completedItems: 8,
                totalItems: 8,
            },
            result: {
                revision: 23,
                sourceBytes: 4096,
                sourceSha256: 'a'.repeat(64),
                characterCount: 3,
                presetCount: 2,
                warningCodes: [],
            },
        }

        const result = await runNativeLegacyLocalBackupExport(
            {
                revision: 22,
                flushPendingData: async (reason) => {
                    calls.push([`flush:${reason}`, undefined])
                },
            },
            { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'lossless-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async () => ({ bytes: 0, warningCodes: [] }),
            },
        )

        expect(result).toEqual(terminal.result)
        expect(calls).toEqual([
            ['flush:native-legacy-local-backup-export', undefined],
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-legacy-local-backup',
                        destination: 'C:\\chosen\\backup.bin',
                        expectedRevision: 22,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'lossless-export' }],
            ['native_file_job_forget', { jobId: 'lossless-export' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('hands a managed backup export to Android SAF and cleans the native source', async () => {
        const events: string[] = []
        const observedStatuses: NativeFileJobStatus[] = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174002.bin'
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-legacy-local-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 4096,
                completedItems: 8,
            },
            result: {
                revision: 22,
                sourceBytes: 4096,
                sourceSha256: 'b'.repeat(64),
                characterCount: 3,
                presetCount: 2,
                warningCodes: [],
                handoffPath,
            },
        }

        const result = await runNativeLegacyLocalBackupExport(
            { revision: 22, flushPendingData: async () => undefined },
            { type: 'androidSaf', suggestedName: 'backup.bin' },
            { onStatus: (status) => observedStatuses.push(status) },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    events.push(`${command}:${JSON.stringify(args ?? {})}`)
                    if (command === 'native_file_job_start')
                        return { jobId: 'lossless-export' }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_legacy_backup_handoff_cleanup')
                        return undefined
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
                copyToAndroidSaf: async (request) => {
                    events.push(
                        `saf:${request.sourcePath}:${request.suggestedName}`,
                    )
                    request.onProgress?.({
                        requestId: 'android-export',
                        operation: 'destination-copy',
                        copiedBytes: 2048,
                        totalBytes: 4096,
                        token: null,
                    })
                    return {
                        bytes: 4096,
                        warningCodes: ['android-saf-provider-not-atomic'],
                    }
                },
            },
        )

        expect(result.handoffPath).toBeUndefined()
        expect(result.warningCodes).toEqual(['android-saf-provider-not-atomic'])
        expect(observedStatuses.at(-1)).toMatchObject({
            state: 'running',
            phase: 'publishing-destination',
            progress: { completedBytes: 2048, totalBytes: 4096 },
        })
        expect(events).toEqual([
            'native_file_job_start:{"request":{"kind":"export-legacy-local-backup","expectedRevision":22}}',
            'native_file_job_status:{"jobId":"lossless-export"}',
            `saf:${handoffPath}:backup.bin`,
            `native_legacy_backup_handoff_cleanup:{"path":"${handoffPath.replaceAll('\\', '\\\\')}"}`,
            'native_file_job_forget:{"jobId":"lossless-export"}',
        ])
    })

    it('rejects a short Android SAF handoff and still cleans both native receipts', async () => {
        const commands: string[] = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174003.bin'
        const terminal: NativeFileJobStatus = {
            jobId: 'lossless-export',
            kind: 'export-legacy-local-backup',
            state: 'succeeded',
            phase: 'complete',
            progress: { completedBytes: 4096, completedItems: 1 },
            result: {
                revision: 22,
                sourceBytes: 4096,
                sourceSha256: 'b'.repeat(64),
                characterCount: 0,
                presetCount: 0,
                warningCodes: [],
                handoffPath,
            },
        }

        await expect(
            runNativeLegacyLocalBackupExport(
                { revision: 22, flushPendingData: async () => undefined },
                { type: 'androidSaf', suggestedName: 'backup.bin' },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'lossless-export' }
                        if (command === 'native_file_job_status')
                            return terminal
                        if (command === 'native_legacy_backup_handoff_cleanup')
                            return undefined
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    copyToAndroidSaf: async () => ({
                        bytes: 2048,
                        warningCodes: [],
                    }),
                },
            ),
        ).rejects.toMatchObject({
            code: 'length-mismatch',
            warningCodes: ['partial-destination-may-remain'],
        })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_legacy_backup_handoff_cleanup',
            'native_file_job_forget',
        ])
    })

    it.each([
        {
            label: 'legacy backup',
            run: runNativeLegacyLocalBackupExport,
            kind: 'export-legacy-local-backup' as const,
            cleanupCommand: 'native_legacy_backup_handoff_cleanup',
            suggestedName: 'backup.bin',
            handoffPath:
                'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174005.bin',
        },
        {
            label: 'legacy backup',
            run: runNativeLegacyLocalBackupExport,
            kind: 'export-legacy-local-backup' as const,
            cleanupCommand: 'native_legacy_backup_handoff_cleanup',
            suggestedName: 'risu-backup.bin',
            handoffPath:
                'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174005.bin',
        },
    ])(
        'retains a successful Android $label job when native source cleanup fails',
        async ({ run, kind, cleanupCommand, suggestedName, handoffPath }) => {
            const commands: string[] = []
            const terminal: NativeFileJobStatus = {
                jobId: 'portable-export',
                kind,
                state: 'succeeded',
                phase: 'complete',
                progress: { completedBytes: 4096, completedItems: 3 },
                result: {
                    revision: 22,
                    sourceBytes: 4096,
                    sourceSha256: 'b'.repeat(64),
                    characterCount: 3,
                    presetCount: 2,
                    warningCodes: [],
                    handoffPath,
                },
            }

            const result = await run(
                { revision: 22, flushPendingData: async () => undefined },
                { type: 'androidSaf', suggestedName },
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'portable-export' }
                        if (command === 'native_file_job_status')
                            return terminal
                        if (command === cleanupCommand)
                            throw new Error('handoff is still in use')
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    copyToAndroidSaf: async () => ({
                        bytes: 4096,
                        warningCodes: [],
                    }),
                },
            )

            expect(result.warningCodes).toEqual(['cleanup-failed'])
            expect(commands).toEqual([
                'native_file_job_start',
                'native_file_job_status',
                cleanupCommand,
            ])
        },
    )

    it('restores from a descriptor without sending file or database bytes through IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses = [
            status('running'),
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation' as const,
            },
            status('succeeded', {
                revision: 4,
                sourceBytes: 128,
                sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }),
        ]
        const refreshed: number[] = []
        const runtime = restoreRuntime(3, {
            capture: (reason) => {
                calls.push([`capture:${reason}`, undefined])
            },
            refresh: (revision) => {
                refreshed.push(revision)
            },
        })

        const result = await runNativeBlockRisuSaveRestore(
            runtime,
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') {
                        return {
                            jobId: 'job-1',
                            warningCodes: ['cleanup-failed'],
                        }
                    }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(4)
        expect(result.warningCodes).toEqual(['cleanup-failed'])
        expect(refreshed).toEqual([4])
        expect(calls).toEqual([
            ['capture:native-block-risu-save-restore', undefined],
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'restore-block-risu-save',
                        source: {
                            type: 'desktopPath',
                            path: 'C:\\chosen\\backup.risudat',
                        },
                        expectedRevision: 3,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['capture:native-block-risu-save-restore-activation', undefined],
            [
                'native_file_job_finalize',
                { jobId: 'job-1', expectedRevision: 3 },
            ],
            ['native_file_job_status', { jobId: 'job-1' }],
            ['native_file_job_forget', { jobId: 'job-1' }],
        ])
        expect(calls.some(([command]) => command.includes('read'))).toBe(false)
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('activates against an explicitly recaptured token after restore preparation', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const statuses = [
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation' as const,
            },
            status('succeeded', {
                revision: 6,
                sourceBytes: 128,
                sourceSha256: 'a'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            }),
        ]

        const result = await runNativeBlockRisuSaveRestore(
            // Reading the backup took long enough for the renderer to leave an
            // edit behind, which the explicit activation token capture flushes to revision 5.
            restoreRuntime(4, { fencedRevision: 5 }),
            { type: 'desktopPath', path: 'C:\chosen\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'job-1' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result.revision).toBe(6)
        expect(
            calls.find(([command]) => command === 'native_file_job_start')?.[1],
        ).toMatchObject({ request: { expectedRevision: 4 } })
        expect(
            calls.find(
                ([command]) => command === 'native_file_job_finalize',
            )?.[1],
        ).toEqual({ jobId: 'job-1', expectedRevision: 5 })
    })

    it('explicit abort requests native cancellation and waits for terminal cleanup', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        let statusCount = 0
        const promise = runNativeBlockRisuSaveRestore(
            restoreRuntime(7, {
                refresh: () => {
                    throw new Error('cancelled restore must not refresh')
                },
            }),
            {
                type: 'androidSpool',
                token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
            },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? status('running')
                            : status('cancelled')
                    }
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => {
                    controller.abort()
                },
            },
        )

        await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it.each(['fence', 'blocking'] as const)(
        'cancels before activation when aborted during %s acquisition',
        async (abortAt) => {
            const controller = new AbortController()
            const commands: string[] = []
            const release = vi.fn()
            const refresh = vi.fn()
            let finishAcquisition!: () => void
            let acquisitionStarted!: () => void
            const acquired = new Promise<void>((resolve) => {
                acquisitionStarted = resolve
            })
            const acquisition = new Promise<void>((resolve) => {
                finishAcquisition = resolve
            })
            const statuses: NativeFileJobStatus[] = [
                { ...status('waitingForInput'), phase: 'awaiting-activation' },
                status('cancelled'),
            ]
            const result = runNativeBlockRisuSaveRestore(
                restoreRuntime(2, {
                    acquire: async () => {
                        acquisitionStarted()
                        await acquisition
                    },
                    refresh,
                    release,
                }),
                { type: 'desktopPath', path: 'C:\\chosen\\backup.bin' },
                {
                    signal: controller.signal,
                    onBlockingChange: (blocking) => {
                        if (blocking && abortAt === 'blocking')
                            controller.abort()
                    },
                },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'job-1' }
                        if (command === 'native_file_job_status')
                            return statuses.shift()
                        if (command === 'native_file_job_cancel')
                            return 'requested'
                        if (command === 'native_file_job_finalize')
                            return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            )
            await acquired
            if (abortAt === 'fence') controller.abort()
            finishAcquisition()
            await expect(result).rejects.toMatchObject({ name: 'AbortError' })
            expect(commands).toEqual([
                'native_file_job_start',
                'native_file_job_status',
                'native_file_job_cancel',
                'native_file_job_status',
                'native_file_job_forget',
            ])
            expect(refresh).not.toHaveBeenCalled()
            expect(release).toHaveBeenCalledOnce()
        },
    )

    it('does not start a native job when cancellation arrives during the flush', async () => {
        const controller = new AbortController()
        const commands: string[] = []

        await expect(
            runNativeBlockRisuSaveRestore(
                restoreRuntime(2, { capture: () => controller.abort() }),
                { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([])
    })

    it('discards an unclaimed Android source when cancellation arrives during mutation capture', async () => {
        const controller = new AbortController()
        const token = '2c4d33fe-2e29-4625-bb1e-c8d1084f9557'
        const commands: string[] = []
        const discarded: string[] = []

        await expect(
            runNativeLegacyLocalBackupRestore(
                restoreRuntime(2, { capture: () => controller.abort() }),
                { type: 'androidSpool', token },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    discardAndroidSource: (sourceToken) => {
                        discarded.push(sourceToken)
                        return true
                    },
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(discarded).toEqual([token])
        expect(commands).toEqual([])
    })

    it('discards an unclaimed Android source when restore starts already cancelled', async () => {
        const controller = new AbortController()
        const token = '2c4d33fe-2e29-4625-bb1e-c8d1084f9557'
        const discarded: string[] = []
        controller.abort()

        await expect(
            runNativeLegacyLocalBackupRestore(
                restoreRuntime(2),
                { type: 'androidSpool', token },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    discardAndroidSource: (sourceToken) => {
                        discarded.push(sourceToken)
                        return true
                    },
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(discarded).toEqual([token])
    })

    it('reports cleanup failure when an unclaimed Android source cannot be discarded', async () => {
        const controller = new AbortController()
        controller.abort()

        await expect(
            runNativeLegacyLocalBackupRestore(
                restoreRuntime(2),
                {
                    type: 'androidSpool',
                    token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
                },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    discardAndroidSource: () => false,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'cleanup-failed',
        })
    })

    it('normalizes Android source discard exceptions as cleanup failure', async () => {
        const controller = new AbortController()
        controller.abort()

        await expect(
            runNativeLegacyLocalBackupRestore(
                restoreRuntime(2),
                {
                    type: 'androidSpool',
                    token: '2c4d33fe-2e29-4625-bb1e-c8d1084f9557',
                },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                    discardAndroidSource: () => {
                        throw new Error('bridge unavailable')
                    },
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'cleanup-failed',
        })
    })

    it('cancels staged data instead of activating when a live edit invalidates the token', async () => {
        const commands: string[] = []
        const statuses = [
            {
                ...status('waitingForInput'),
                phase: 'awaiting-activation' as const,
            },
            status('cancelled'),
        ]

        await expect(
            runNativeBlockRisuSaveRestore(
                restoreRuntime(3, {
                    acquire: () => {
                        throw new Error('mutation generation changed')
                    },
                }),
                { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                undefined,
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'job-1' }
                        if (command === 'native_file_job_status')
                            return statuses.shift()
                        if (command === 'native_file_job_cancel')
                            return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ code: 'revision-conflict' })

        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
        expect(commands).not.toContain('native_file_job_finalize')
    })

    it('releases the replacement fence before plugin reload and acknowledgement', async () => {
        const events: string[] = []
        const observedPhases: string[] = []
        let statusCount = 0
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'f'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        await runNativeBlockRisuSaveRestore(
            restoreRuntime(8, {
                acquire: () => {
                    events.push('fence-acquired')
                },
                refresh: () => {
                    events.push('working-set-refreshed')
                },
                release: () => events.push('fence-released'),
            }),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            {
                afterRefresh: () => {
                    events.push('plugins-reloaded')
                },
                onStatus: (status) => {
                    observedPhases.push(status.phase)
                    if (status.detail)
                        events.push(`stage:${status.detail.stage}`)
                },
            },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    if (command === 'native_file_job_start')
                        return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? {
                                  ...status('waitingForInput'),
                                  phase: 'awaiting-activation',
                              }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') {
                        events.push('native-finalized')
                        return 'requested'
                    }
                    if (command === 'native_file_job_forget') {
                        events.push('terminal-acknowledged')
                        return true
                    }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(events).toEqual([
            'fence-acquired',
            'native-finalized',
            'stage:refreshing-app',
            'working-set-refreshed',
            'fence-released',
            'stage:reloading-plugins',
            'plugins-reloaded',
            'terminal-acknowledged',
        ])
        expect(observedPhases).toContain('activating-database')
    })

    it('continues a committed restore once after read-only recovery without reactivation', async () => {
        const events: string[] = []
        let statusCount = 0
        const commands: string[] = []
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'e'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const runtime = restoreRuntime(8, {
            projection: 'refresh-required',
            release: () => events.push('fence-released'),
        })
        await expect(runNativeBlockRisuSaveRestore(
            runtime,
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            { afterRefresh: () => { events.push('plugins-reloaded') } },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? { ...status('waitingForInput'), phase: 'awaiting-activation' }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') {
                        events.push('terminal-acknowledged')
                        return true
                    }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toBeInstanceOf(NativeFileJobActivationCommittedError)

        expect(events).toEqual(['fence-released'])
        expect(commands.filter(command => command === 'native_file_job_finalize')).toHaveLength(1)
        await continueCommittedWorkingSetRefresh(9, runtime, 2)
        await continueCommittedWorkingSetRefresh(9, runtime, 2)
        expect(events).toEqual([
            'fence-released',
            'plugins-reloaded',
            'terminal-acknowledged',
        ])
        expect(commands.filter(command => command === 'native_file_job_finalize')).toHaveLength(1)
    })

    it('acknowledges a committed native job when plugin reload fails after fence release', async () => {
        let statusCount = 0
        const commands: string[] = []
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'd'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        await expect(runNativeBlockRisuSaveRestore(
            restoreRuntime(8),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            { afterRefresh: async () => { throw new Error('plugin reload failed') } },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? { ...status('waitingForInput'), phase: 'awaiting-activation' }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )).rejects.toBeInstanceOf(NativeFileJobActivationCommittedError)

        expect(commands.filter(command => command === 'native_file_job_finalize')).toHaveLength(1)
        expect(commands.filter(command => command === 'native_file_job_forget')).toHaveLength(1)
    })

    it.each(['refresh', 'native-status', 'ui-status'])(
        'retains a committed job and exposes recovery state when %s fails',
        async (failurePoint) => {
            const commands: string[] = []
            let statusCount = 0
            const committed = {
                revision: 9,
                sourceBytes: 128,
                sourceSha256: 'b'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
            }

            const promise = runNativeBlockRisuSaveRestore(
                restoreRuntime(8, {
                    refresh: () => {
                        if (failurePoint === 'refresh')
                            throw new Error('refresh unavailable')
                    },
                }),
                { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                {
                    onNativeStatus: (status) => {
                        if (
                            status.state === 'succeeded' &&
                            failurePoint === 'native-status'
                        )
                            throw new Error('committed observer unavailable')
                    },
                    onStatus: (status) => {
                        if (
                            status.state === 'succeeded' &&
                            failurePoint === 'ui-status'
                        )
                            throw new Error('committed UI unavailable')
                    },
                },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'job-1' }
                        if (command === 'native_file_job_status') {
                            statusCount++
                            return statusCount === 1
                                ? {
                                      ...status('waitingForInput'),
                                      phase: 'awaiting-activation',
                                  }
                                : status('succeeded', committed)
                        }
                        if (command === 'native_file_job_finalize')
                            return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            )

            await expect(promise).rejects.toEqual(
                expect.objectContaining({
                    name: 'NativeFileJobActivationCommittedError',
                    code: 'activation-committed-refresh-failed',
                    committedRevision: 9,
                    recoveryRequired: true,
                } satisfies Partial<NativeFileJobActivationCommittedError>),
            )
            expect(commands).toEqual([
                'native_file_job_start',
                'native_file_job_status',
                'native_file_job_finalize',
                'native_file_job_status',
            ])
        },
    )

    it('keeps committed success when terminal acknowledgement fails', async () => {
        const commands: string[] = []
        let statusCount = 0
        const committed = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'c'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        }

        const result = await runNativeBlockRisuSaveRestore(
            restoreRuntime(8),
            { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
            undefined,
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'job-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? {
                                  ...status('waitingForInput'),
                                  phase: 'awaiting-activation',
                              }
                            : status('succeeded', committed)
                    }
                    if (command === 'native_file_job_finalize')
                        return 'requested'
                    if (command === 'native_file_job_forget') {
                        throw {
                            code: 'store-error',
                            message: 'terminal acknowledgement failed',
                        }
                    }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual({
            ...committed,
            warningCodes: ['cleanup-failed'],
        })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_finalize',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('acknowledges terminal failure even when translating it to an exception', async () => {
        const commands: string[] = []
        const failed = status('failed')
        failed.error = { code: 'corrupt-input', message: 'invalid gzip data' }

        await expect(
            runNativeBlockRisuSaveRestore(
                restoreRuntime(3),
                { type: 'desktopPath', path: 'C:\\chosen\\backup.risudat' },
                undefined,
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'job-1' }
                        if (command === 'native_file_job_status') return failed
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ code: 'corrupt-input' })
        expect(commands.at(-1)).toBe('native_file_job_forget')
    })

    it('preserves structured native command errors at the facade boundary', async () => {
        await expect(
            runNativeBlockRisuSaveRestore(
                restoreRuntime(1),
                { type: 'desktopPath', path: 'C:\\missing\\backup.risudat' },
                undefined,
                {
                    isTauri: () => true,
                    invoke: async () => {
                        throw {
                            code: 'invalid-source',
                            message: 'desktop source is unavailable',
                        }
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'invalid-source',
            message: 'desktop source is unavailable',
        })
    })

    it('exports a pinned revision to a native destination without file chunks in IPC', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const observed: NativeFileJobStatus[] = []
        let revision = 11
        const runtime = {
            get revision() {
                return revision
            },
            flushPendingData: async (reason: string) => {
                calls.push([`flush:${reason}`, undefined])
                revision = 12
            },
        }
        const running: NativeFileJobStatus = {
            jobId: 'export-1',
            kind: 'export-block-risu-save',
            state: 'running',
            phase: 'writing-export',
            progress: {
                completedBytes: 64,
                completedItems: 2,
            },
        }
        const succeeded: NativeFileJobStatus = {
            jobId: 'export-1',
            kind: 'export-block-risu-save',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: 512,
                totalBytes: 512,
                completedItems: 4,
                totalItems: 4,
            },
            result: {
                revision: 12,
                sourceBytes: 256,
                sourceSha256: 'd'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
            },
        }
        const statuses = [running, succeeded]

        const result = await runNativeBlockRisuSaveExport(
            runtime,
            'C:\\chosen\\backup.risudat',
            { omitAccount: true, onStatus: (value) => observed.push(value) },
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'export-1' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(result).toEqual(succeeded.result)
        expect(observed).toEqual([running, succeeded])
        expect(calls).toEqual([
            ['flush:native-block-risu-save-export', undefined],
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'export-block-risu-save',
                        destination: 'C:\\chosen\\backup.risudat',
                        expectedRevision: 12,
                        omitAccount: true,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'export-1' }],
            ['native_file_job_status', { jobId: 'export-1' }],
            ['native_file_job_forget', { jobId: 'export-1' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
    })

    it('keeps the JavaScript facade bounded when native reports a 10 GiB export', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const collectGarbage = (
            globalThis as typeof globalThis & { gc?: () => void }
        ).gc
        collectGarbage?.()
        const heapBefore = process.memoryUsage().heapUsed
        const tenGiB = 10 * 1024 * 1024 * 1024
        const terminal: NativeFileJobStatus = {
            jobId: 'large-export',
            kind: 'export-block-risu-save',
            state: 'succeeded',
            phase: 'complete',
            progress: {
                completedBytes: tenGiB * 2,
                totalBytes: tenGiB * 2,
                completedItems: 50_007,
                totalItems: 50_007,
            },
            result: {
                revision: 15,
                sourceBytes: tenGiB,
                sourceSha256: 'e'.repeat(64),
                characterCount: 50_000,
                presetCount: 7,
                warningCodes: [],
            },
        }

        const result = await runNativeBlockRisuSaveExport(
            {
                revision: 15,
                flushPendingData: async () => undefined,
            },
            'C:\\chosen\\ten-gib.risudat',
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') {
                        return { jobId: 'large-export' }
                    }
                    if (command === 'native_file_job_status') return terminal
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        collectGarbage?.()
        const heapDelta = Math.max(
            0,
            process.memoryUsage().heapUsed - heapBefore,
        )
        console.info(
            `[native-file-job-heap] declared=10GiB heapDelta=${heapDelta}`,
        )
        expect(result.sourceBytes).toBe(tenGiB)
        expect(JSON.stringify(calls).length).toBeLessThan(512)
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')
        if (collectGarbage) expect(heapDelta).toBeLessThan(16 * 1024 * 1024)
    })

    it('cancels a native export without acknowledging it before terminal cleanup', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        let statusCount = 0

        const promise = runNativeBlockRisuSaveExport(
            {
                revision: 6,
                flushPendingData: async () => undefined,
            },
            'C:\\chosen\\backup.risudat',
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'export-1' }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        return statusCount === 1
                            ? {
                                  jobId: 'export-1',
                                  kind: 'export-block-risu-save',
                                  state: 'running',
                                  phase: 'writing-export',
                                  progress: {
                                      completedBytes: 1,
                                      completedItems: 0,
                                  },
                              }
                            : {
                                  jobId: 'export-1',
                                  kind: 'export-block-risu-save',
                                  state: 'cancelled',
                                  phase: 'complete',
                                  progress: {
                                      completedBytes: 1,
                                      completedItems: 0,
                                  },
                              }
                    }
                    if (command === 'native_file_job_cancel') return 'requested'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => controller.abort(),
            },
        )

        await expect(promise).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('starts an exact official publication request and leaves success unacknowledged', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const request = {
            expectedRevision: 17,
            lease: 'snapshot-publication-17',
            accountId: 'account-1',
            baseUrl: 'https://realm.example',
            replacements: {
                'asset://old': 'asset://new',
                'asset://portrait': 'https://cdn.example/portrait',
            },
            session: 'session-9',
            saveDate: '1777777777777',
            credential: { kind: 'risu-auth' as const, token: 'legacy-secret' },
        }
        const result = {
            revision: 17,
            sourceBytes: 512,
            sourceSha256: 'f'.repeat(64),
            characterCount: 2,
            presetCount: 1,
            warningCodes: [],
            publication: {
                kind: 'written' as const,
                accountId: 'account-1',
                session: 'session-9',
                saveDate: '1777777777777',
                status: 200,
                replacementKey: 'replacement-1',
                warning: null,
                reloadSession: false,
            },
        }

        const outcome = await runNativeOfficialPublicationAttempt(
            request,
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start') {
                        return { jobId: 'publication-1' }
                    }
                    if (command === 'native_file_job_status') {
                        return {
                            jobId: 'publication-1',
                            kind: 'official-publication-upload',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: {
                                completedBytes: 1024,
                                totalBytes: 1024,
                                completedItems: 2,
                                totalItems: 2,
                            },
                            result,
                        }
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(outcome?.kind).toBe('completed')
        if (!outcome || outcome.kind !== 'completed') {
            throw new Error('Expected a completed publication')
        }
        expect(outcome.receipt.jobId).toBe('publication-1')
        expect(outcome.receipt.result).toEqual(result)
        expect(calls).toEqual([
            [
                'native_file_job_start',
                {
                    request: {
                        kind: 'official-publication-upload',
                        ...request,
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'publication-1' }],
        ])
        expect(JSON.stringify(calls)).not.toContain('database')
        expect(JSON.stringify(calls)).not.toContain('path')
        expect(JSON.stringify(calls)).not.toContain('Uint8Array')

        await outcome.receipt.acknowledge()
        await outcome.receipt.acknowledge()
        expect(calls.slice(2)).toEqual([
            ['native_file_job_forget', { jobId: 'publication-1' }],
        ])
    })

    it('falls back only when publication capability is unavailable before a job ID', async () => {
        const request = {
            expectedRevision: 3,
            lease: 'snapshot-publication-3',
            accountId: 'account-1',
            baseUrl: 'https://realm.example',
            replacements: {},
            session: null,
            saveDate: '1777777777777',
            credential: { kind: 'risu-auth' as const, token: 'legacy-secret' },
        }

        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async () => {
                        throw {
                            code: 'capability-unavailable',
                            message: 'publication jobs are unavailable',
                        }
                    },
                    wait: async () => undefined,
                },
            ),
        ).resolves.toBeNull()

        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async () => {
                        throw {
                            code: 'invalid-request',
                            message: 'invalid base URL',
                        }
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'invalid-request',
        })

        const malformedStartCommands: string[] = []
        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        malformedStartCommands.push(command)
                        if (command === 'native_file_job_start') return {}
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'invalid-result',
        })
        expect(malformedStartCommands).toEqual(['native_file_job_start'])

        let started = false
        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        if (command === 'native_file_job_start') {
                            started = true
                            return { jobId: 'publication-1' }
                        }
                        throw {
                            code: 'capability-unavailable',
                            message: 'status temporarily unavailable',
                        }
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({
            name: 'NativeFileJobError',
            code: 'capability-unavailable',
        })
        expect(started).toBe(true)
    })

    it('does not start publication when already aborted', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        controller.abort()

        await expect(
            runNativeOfficialPublicationAttempt(
                {
                    expectedRevision: 3,
                    lease: 'snapshot-publication-3',
                    accountId: 'account-1',
                    baseUrl: 'https://realm.example',
                    replacements: {},
                    session: null,
                    saveDate: '1777777777777',
                    credential: { kind: 'risu-auth', token: 'legacy-secret' },
                },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(commands).toEqual([])
    })

    it('requests publication cancellation once and waits for terminal cleanup', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        const states: NativeFileJobStatus['state'][] = [
            'running',
            'cancelling',
            'cancelled',
        ]
        let waitCount = 0

        await expect(
            runNativeOfficialPublicationAttempt(
                {
                    expectedRevision: 3,
                    lease: 'snapshot-publication-3',
                    accountId: 'account-1',
                    baseUrl: 'https://realm.example',
                    replacements: {},
                    session: null,
                    saveDate: '1777777777777',
                    credential: { kind: 'risu-auth', token: 'legacy-secret' },
                },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start') {
                            return { jobId: 'publication-1' }
                        }
                        if (command === 'native_file_job_status') {
                            const state = states.shift() ?? 'cancelled'
                            return {
                                jobId: 'publication-1',
                                kind: 'official-publication-upload',
                                state,
                                phase:
                                    state === 'cancelled'
                                        ? 'complete'
                                        : 'uploading-database',
                                progress: {
                                    completedBytes: 64,
                                    completedItems: 0,
                                },
                            }
                        }
                        if (command === 'native_file_job_cancel') {
                            throw {
                                code: 'store-error',
                                message: 'cancel response was lost',
                            }
                        }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => {
                        waitCount++
                        if (waitCount === 1) controller.abort()
                    },
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })

        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('keeps a too-late publication success unacknowledged after abort', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        let statusCount = 0
        const result = {
            revision: 9,
            sourceBytes: 128,
            sourceSha256: 'd'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
            publication: {
                kind: 'not-modified' as const,
                accountId: 'account-1',
                session: 'session-1',
                saveDate: '1777777777777',
                status: 304,
                replacementKey: 'replacement-1',
            },
        }

        const outcome = await runNativeOfficialPublicationAttempt(
            {
                expectedRevision: 9,
                lease: 'snapshot-publication-9',
                accountId: 'account-1',
                baseUrl: 'https://realm.example',
                replacements: {},
                session: 'session-1',
                saveDate: '1777777777777',
                credential: { kind: 'risu-auth', token: 'legacy-secret' },
            },
            { signal: controller.signal },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') {
                        return { jobId: 'publication-1' }
                    }
                    if (command === 'native_file_job_status') {
                        statusCount++
                        if (statusCount === 1) {
                            return {
                                jobId: 'publication-1',
                                kind: 'official-publication-upload',
                                state: 'running',
                                phase: 'uploading-database',
                                progress: {
                                    completedBytes: 64,
                                    completedItems: 0,
                                },
                            }
                        }
                        return {
                            jobId: 'publication-1',
                            kind: 'official-publication-upload',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: {
                                completedBytes: 256,
                                completedItems: 1,
                            },
                            result,
                        }
                    }
                    if (command === 'native_file_job_cancel') return 'tooLate'
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => controller.abort(),
            },
        )

        expect(outcome?.kind).toBe('completed')
        if (!outcome || outcome.kind !== 'completed') {
            throw new Error('Expected a completed publication')
        }
        expect(outcome.receipt.result).toEqual(result)
        expect(commands).toEqual([
            'native_file_job_start',
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
        ])
        await outcome.receipt.acknowledge()
        expect(commands.at(-1)).toBe('native_file_job_forget')
    })

    it('acknowledges failed and cancelled publication terminals before rejecting', async () => {
        const request = {
            expectedRevision: 3,
            lease: 'snapshot-publication-3',
            accountId: 'account-1',
            baseUrl: 'https://realm.example',
            replacements: {},
            session: null,
            saveDate: '1777777777777',
            credential: { kind: 'risu-auth' as const, token: 'legacy-secret' },
        }
        const failedCommands: string[] = []

        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        failedCommands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'publication-1' }
                        if (command === 'native_file_job_status') {
                            return {
                                jobId: 'publication-1',
                                kind: 'official-publication-upload',
                                state: 'failed',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 0,
                                    completedItems: 0,
                                },
                                error: {
                                    code: 'publication-timeout',
                                    message: 'upload stalled',
                                },
                            }
                        }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ code: 'publication-timeout' })
        expect(failedCommands.at(-1)).toBe('native_file_job_forget')

        const cancelledCommands: string[] = []
        await expect(
            runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        cancelledCommands.push(command)
                        if (command === 'native_file_job_start')
                            return { jobId: 'publication-2' }
                        if (command === 'native_file_job_status') {
                            return {
                                jobId: 'publication-2',
                                kind: 'official-publication-upload',
                                state: 'cancelled',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 0,
                                    completedItems: 0,
                                },
                            }
                        }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })
        expect(cancelledCommands.at(-1)).toBe('native_file_job_forget')
    })

    it('retains successful publication jobs whose association metadata does not match', async () => {
        const request = {
            expectedRevision: 3,
            lease: 'snapshot-publication-3',
            accountId: 'account-1',
            baseUrl: 'https://realm.example',
            replacements: {},
            session: null,
            saveDate: '1777777777777',
            credential: { kind: 'risu-auth' as const, token: 'legacy-secret' },
        }
        const cases = [
            { revision: 4, accountId: 'account-1' },
            { revision: 3, accountId: 'account-2' },
        ]

        for (const mismatch of cases) {
            const commands: string[] = []
            const attempt = runNativeOfficialPublicationAttempt(
                request,
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_start') {
                            return { jobId: 'publication-1' }
                        }
                        if (command === 'native_file_job_status') {
                            return {
                                jobId: 'publication-1',
                                kind: 'official-publication-upload',
                                state: 'succeeded',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 128,
                                    completedItems: 1,
                                },
                                result: {
                                    revision: mismatch.revision,
                                    sourceBytes: 128,
                                    sourceSha256: 'a'.repeat(64),
                                    characterCount: 1,
                                    presetCount: 0,
                                    warningCodes: [],
                                    publication: {
                                        kind: 'not-modified',
                                        accountId: mismatch.accountId,
                                        session: 'session-1',
                                        saveDate: '1777777777777',
                                        status: 304,
                                        replacementKey: 'replacement-1',
                                    },
                                },
                            }
                        }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            )

            await expect(attempt).rejects.toMatchObject({
                code: 'invalid-result',
            })
            expect(commands).not.toContain('native_file_job_forget')
        }
    })

    it('returns auth outcomes as unacknowledged receipts for the account retry loop', async () => {
        const commands: string[] = []
        const publication = {
            kind: 'reauthentication-needed' as const,
            warning: 'please sign in',
            accountId: 'account-1',
            session: null,
            saveDate: '1777777777777',
            status: 403,
        } satisfies NativeOfficialPublicationAttemptResult
        const outcome = await runNativeOfficialPublicationAttempt(
            {
                expectedRevision: 3,
                lease: 'snapshot-publication-3',
                accountId: 'account-1',
                baseUrl: 'https://realm.example',
                replacements: {},
                session: null,
                saveDate: '1777777777777',
                credential: { kind: 'risu-auth', token: 'legacy-secret' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_start') {
                        return { jobId: 'publication-1' }
                    }
                    if (command === 'native_file_job_status') {
                        return {
                            jobId: 'publication-1',
                            kind: 'official-publication-upload',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: { completedBytes: 0, completedItems: 0 },
                            result: {
                                revision: 3,
                                sourceBytes: 128,
                                sourceSha256: 'a'.repeat(64),
                                characterCount: 1,
                                presetCount: 0,
                                warningCodes: [],
                                publication,
                            },
                        }
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(outcome?.kind).toBe('completed')
        if (!outcome || outcome.kind !== 'completed') {
            throw new Error('Expected a completed publication')
        }
        expect(outcome.receipt.result.publication).toEqual(publication)
        expect(commands).not.toContain('native_file_job_forget')
        await outcome.receipt.acknowledge()
        expect(commands.at(-1)).toBe('native_file_job_forget')
    })

    it('keeps an ordinary publication 403 in the same job for reauthentication', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const outcome = await runNativeOfficialPublicationAttempt(
            {
                expectedRevision: 17,
                lease: 'snapshot-publication-17',
                accountId: 'account-1',
                baseUrl: 'https://realm.example',
                replacements: {},
                session: null,
                saveDate: '1000',
                credential: { kind: 'risu-auth', token: 'old-token' },
            },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (command === 'native_file_job_start')
                        return { jobId: 'publication-1' }
                    if (command === 'native_file_job_status')
                        return {
                            jobId: 'publication-1',
                            kind: 'official-publication-upload',
                            state: 'waitingForInput',
                            phase: 'awaiting-publication-retry',
                            progress: {
                                completedBytes: 256,
                                completedItems: 1,
                            },
                            publicationAttempt: {
                                kind: 'reauthentication-needed',
                                warning: 'please sign in',
                                accountId: 'account-1',
                                session: 'session-42',
                                saveDate: '1000',
                                status: 403,
                            },
                        }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => {
                    throw new Error('Publication retry state was not surfaced')
                },
            },
        )

        expect(outcome).toEqual({
            kind: 'waiting-for-reauthentication',
            warning: 'please sign in',
            jobId: 'publication-1',
            accountId: 'account-1',
            session: 'session-42',
        })
        expect(calls.map(([command]) => command)).toEqual([
            'native_file_job_start',
            'native_file_job_status',
        ])
    })

    it('continues a publication with refreshed input without starting a second job', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const result = {
            revision: 17,
            sourceBytes: 512,
            sourceSha256: 'f'.repeat(64),
            characterCount: 2,
            presetCount: 1,
            warningCodes: [],
            publication: {
                kind: 'written' as const,
                accountId: 'account-1',
                session: 'session-42',
                saveDate: '1001',
                status: 200,
                replacementKey: 'replacement-1',
                warning: null,
                reloadSession: false,
            },
        }
        const outcome = await continueNativeOfficialPublication(
            'publication-1',
            {
                accountId: 'account-1',
                session: 'session-42',
                saveDate: '1001',
                credential: { kind: 'risu-auth', token: 'new-token' },
            },
            { revision: 17, accountId: 'account-1' },
            {},
            {
                isTauri: () => true,
                invoke: async (command, args) => {
                    calls.push([command, args])
                    if (
                        command === 'native_file_job_official_publication_retry'
                    )
                        return 'accepted'
                    if (command === 'native_file_job_status')
                        return {
                            jobId: 'publication-1',
                            kind: 'official-publication-upload',
                            state: 'succeeded',
                            phase: 'complete',
                            progress: {
                                completedBytes: 1024,
                                completedItems: 2,
                            },
                            result,
                        }
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(outcome.kind).toBe('completed')
        if (outcome.kind !== 'completed')
            throw new Error('Expected a completed publication')
        expect(outcome.receipt.result).toEqual(result)
        expect(calls).toEqual([
            [
                'native_file_job_official_publication_retry',
                {
                    request: {
                        jobId: 'publication-1',
                        accountId: 'account-1',
                        session: 'session-42',
                        saveDate: '1001',
                        credential: { kind: 'risu-auth', token: 'new-token' },
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'publication-1' }],
        ])
    })

    it('uses one job for consecutive 403 retries and sends only fresh retry input', async () => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const retry = {
            accountId: 'account-1',
            session: 'session-42',
            saveDate: '1001',
            credential: { kind: 'risu-auth' as const, token: 'new-token' },
            baseUrl: 'https://stale.example',
        }
        const waiting = {
            jobId: 'publication-1',
            kind: 'official-publication-upload' as const,
            state: 'waitingForInput' as const,
            phase: 'awaiting-publication-retry' as const,
            progress: { completedBytes: 512, completedItems: 1 },
            publicationAttempt: {
                kind: 'reauthentication-needed' as const,
                warning: null,
                accountId: 'account-1',
                session: 'session-43',
                saveDate: '1001',
                status: 403,
            },
        }
        const terminal = {
            jobId: 'publication-1',
            kind: 'official-publication-upload' as const,
            state: 'succeeded' as const,
            phase: 'complete' as const,
            progress: { completedBytes: 1024, completedItems: 2 },
            result: {
                revision: 17,
                sourceBytes: 512,
                sourceSha256: 'f'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
                publication: {
                    kind: 'written' as const,
                    accountId: 'account-1',
                    session: 'session-43',
                    saveDate: '1002',
                    status: 200,
                    replacementKey: 'database/database.bin',
                    warning: null,
                    reloadSession: false,
                },
            },
        }
        let statusPoll = 0
        const dependencies = {
            isTauri: () => true,
            invoke: async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_official_publication_retry')
                    return 'accepted'
                if (command === 'native_file_job_status') {
                    return statusPoll++ === 0 ? waiting : terminal
                }
                throw new Error(`Unexpected command: ${command}`)
            },
            wait: async () => undefined,
        }
        const firstOutcome = await continueNativeOfficialPublication(
            'publication-1',
            retry,
            { revision: 17, accountId: 'account-1' },
            {},
            dependencies,
        )

        expect(firstOutcome).toEqual({
            kind: 'waiting-for-reauthentication',
            warning: null,
            jobId: 'publication-1',
            accountId: 'account-1',
            session: 'session-43',
        })
        const secondOutcome = await continueNativeOfficialPublication(
            'publication-1',
            {
                accountId: 'account-1',
                session: 'session-43',
                saveDate: '1002',
                credential: { kind: 'risu-auth', token: 'newer-token' },
                expectedRevision: 0,
            } as NativeOfficialPublicationRetryRequest & {
                expectedRevision: number
            },
            { revision: 17, accountId: 'account-1' },
            {},
            dependencies,
        )

        expect(secondOutcome.kind).toBe('completed')
        expect(calls).toEqual([
            [
                'native_file_job_official_publication_retry',
                {
                    request: {
                        jobId: 'publication-1',
                        accountId: 'account-1',
                        session: 'session-42',
                        saveDate: '1001',
                        credential: { kind: 'risu-auth', token: 'new-token' },
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'publication-1' }],
            [
                'native_file_job_official_publication_retry',
                {
                    request: {
                        jobId: 'publication-1',
                        accountId: 'account-1',
                        session: 'session-43',
                        saveDate: '1002',
                        credential: { kind: 'risu-auth', token: 'newer-token' },
                    },
                },
            ],
            ['native_file_job_status', { jobId: 'publication-1' }],
        ])
    })

    it('drains an already-aborted continuation through cancellation without sending retry input', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        controller.abort()

        await expect(
            continueNativeOfficialPublication(
                'publication-1',
                {
                    accountId: 'account-1',
                    session: 'session-42',
                    saveDate: '1001',
                    credential: { kind: 'risu-auth', token: 'new-token' },
                },
                { revision: 17, accountId: 'account-1' },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_cancel')
                            return 'requested'
                        if (command === 'native_file_job_status')
                            return {
                                jobId: 'publication-1',
                                kind: 'official-publication-upload',
                                state: 'cancelled',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 512,
                                    completedItems: 1,
                                },
                            }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })

        expect(commands).toEqual([
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('preserves a successful continuation that completes too late to cancel', async () => {
        const controller = new AbortController()
        const commands: string[] = []
        controller.abort()
        const terminal = {
            jobId: 'publication-1',
            kind: 'official-publication-upload' as const,
            state: 'succeeded' as const,
            phase: 'complete' as const,
            progress: { completedBytes: 512, completedItems: 1 },
            result: {
                revision: 17,
                sourceBytes: 512,
                sourceSha256: 'f'.repeat(64),
                characterCount: 2,
                presetCount: 1,
                warningCodes: [],
                publication: {
                    kind: 'written' as const,
                    accountId: 'account-1',
                    session: 'session-42',
                    saveDate: '1001',
                    status: 200,
                    replacementKey: 'database/database.bin',
                    warning: null,
                    reloadSession: false,
                },
            },
        }

        await expect(
            continueNativeOfficialPublication(
                'publication-1',
                {
                    accountId: 'account-1',
                    session: 'session-42',
                    saveDate: '1001',
                    credential: { kind: 'risu-auth', token: 'new-token' },
                },
                { revision: 17, accountId: 'account-1' },
                { signal: controller.signal },
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_cancel')
                            return 'too-late'
                        if (command === 'native_file_job_status')
                            return terminal
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).rejects.toMatchObject({ name: 'AbortError' })

        expect(commands).toEqual([
            'native_file_job_cancel',
            'native_file_job_status',
        ])
    })

    it('resumes an active publication as an unacknowledged terminal receipt', async () => {
        const commands: string[] = []
        let poll = 0
        const terminal = {
            jobId: 'publication-recovered',
            kind: 'official-publication-upload' as const,
            state: 'succeeded' as const,
            phase: 'complete' as const,
            progress: { completedBytes: 128, completedItems: 2 },
            result: {
                revision: 9,
                sourceBytes: 128,
                sourceSha256: 'd'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                publication: {
                    kind: 'not-modified' as const,
                    accountId: 'account-1',
                    session: 'session-2',
                    saveDate: '1777777777777',
                    status: 304,
                    replacementKey: 'database/database.bin',
                },
            },
        }
        const receipt = await resumeNativeOfficialPublication(
            'publication-recovered',
            { pollIntervalMs: 0 },
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_status') {
                        if (poll++ === 0)
                            return {
                                ...terminal,
                                state: 'running',
                                phase: 'uploading-database',
                                result: undefined,
                            }
                        return terminal
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(receipt?.result).toEqual(terminal.result)
        expect(commands).toEqual([
            'native_file_job_status',
            'native_file_job_status',
        ])

        await receipt?.acknowledge()
        expect(commands.at(-1)).toBe('native_file_job_forget')
    })

    it('cancels and forgets a recovered publication waiting for reauthentication', async () => {
        const commands: string[] = []
        let poll = 0

        await expect(
            resumeNativeOfficialPublication(
                'publication-waiting',
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_status') {
                            if (poll++ === 0)
                                return {
                                    jobId: 'publication-waiting',
                                    kind: 'official-publication-upload',
                                    state: 'waitingForInput',
                                    phase: 'awaiting-publication-retry',
                                    progress: {
                                        completedBytes: 128,
                                        completedItems: 1,
                                    },
                                    publicationAttempt: {
                                        kind: 'reauthentication-needed',
                                        warning: null,
                                        accountId: 'account-1',
                                        session: 'session-1',
                                        saveDate: '1000',
                                        status: 403,
                                    },
                                }
                            return {
                                jobId: 'publication-waiting',
                                kind: 'official-publication-upload',
                                state: 'cancelled',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 128,
                                    completedItems: 1,
                                },
                            }
                        }
                        if (command === 'native_file_job_cancel')
                            return 'requested'
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).resolves.toBeNull()

        expect(commands).toEqual([
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })

    it('retains a recovered waiting job that succeeds too late to cancel', async () => {
        const commands: string[] = []
        let poll = 0
        const terminal = {
            jobId: 'publication-waiting',
            kind: 'official-publication-upload' as const,
            state: 'succeeded' as const,
            phase: 'complete' as const,
            progress: { completedBytes: 128, completedItems: 1 },
            result: {
                revision: 17,
                sourceBytes: 128,
                sourceSha256: 'c'.repeat(64),
                characterCount: 1,
                presetCount: 0,
                warningCodes: [],
                publication: {
                    kind: 'not-modified' as const,
                    accountId: 'account-1',
                    session: 'session-1',
                    saveDate: '1000',
                    status: 304,
                    replacementKey: 'database/database.bin',
                },
            },
        }
        const receipt = await resumeNativeOfficialPublication(
            'publication-waiting',
            {},
            {
                isTauri: () => true,
                invoke: async (command) => {
                    commands.push(command)
                    if (command === 'native_file_job_status') {
                        if (poll++ === 0)
                            return {
                                ...terminal,
                                state: 'waitingForInput' as const,
                                phase: 'awaiting-publication-retry' as const,
                                result: undefined,
                                publicationAttempt: {
                                    kind: 'reauthentication-needed' as const,
                                    warning: null,
                                    accountId: 'account-1',
                                    session: 'session-1',
                                    saveDate: '1000',
                                    status: 403,
                                },
                            }
                        return terminal
                    }
                    if (command === 'native_file_job_cancel') return 'too-late'
                    throw new Error(`Unexpected command: ${command}`)
                },
                wait: async () => undefined,
            },
        )

        expect(receipt?.result).toEqual(terminal.result)
        expect(commands).toEqual([
            'native_file_job_status',
            'native_file_job_cancel',
            'native_file_job_status',
        ])
    })

    it('forgets a recovered failed publication without exposing an association receipt', async () => {
        const commands: string[] = []

        await expect(
            resumeNativeOfficialPublication(
                'publication-failed',
                {},
                {
                    isTauri: () => true,
                    invoke: async (command) => {
                        commands.push(command)
                        if (command === 'native_file_job_status')
                            return {
                                jobId: 'publication-failed',
                                kind: 'official-publication-upload',
                                state: 'failed',
                                phase: 'complete',
                                progress: {
                                    completedBytes: 0,
                                    completedItems: 0,
                                },
                                error: { code: 'offline', message: 'offline' },
                            }
                        if (command === 'native_file_job_forget') return true
                        throw new Error(`Unexpected command: ${command}`)
                    },
                    wait: async () => undefined,
                },
            ),
        ).resolves.toBeNull()

        expect(commands).toEqual([
            'native_file_job_status',
            'native_file_job_forget',
        ])
    })
})

it('cancels staged restore and waits for terminal settlement when the fresh precondition fails', async () => {
    const calls: string[] = []
    const statuses = [
        { ...status('waitingForInput'), phase: 'awaiting-activation' },
        status('cancelled'),
    ]
    const acquire = vi.fn()
    await expect(
        runNativeLegacyLocalBackupRestore(
            restoreRuntime(17, { acquire }),
            { type: 'desktopPath', path: 'C:\\synthetic\\backup.bin' },
            {
                beforeActivation: () => {
                    throw new NativeFileJobError(
                        'resolve-pending-operation-first',
                        'pending',
                    )
                },
            },
            {
                isTauri: () => true,
                wait: async () => {},
                invoke: async (command) => {
                    calls.push(command)
                    if (command === 'native_file_job_start')
                        return { jobId: 'staged' }
                    if (command === 'native_file_job_status')
                        return statuses.shift()
                    return true
                },
            },
        ),
    ).rejects.toMatchObject({ code: 'resolve-pending-operation-first' })
    expect(acquire).not.toHaveBeenCalled()
    expect(calls).toEqual([
        'native_file_job_start',
        'native_file_job_status',
        'native_file_job_cancel',
        'native_file_job_status',
        'native_file_job_forget',
    ])
})

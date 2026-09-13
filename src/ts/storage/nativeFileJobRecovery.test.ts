import { describe, expect, it, vi } from 'vitest'

import { getAndroidSafExportSourceId } from './androidSafBridge'
import {
    acknowledgeRecoveredNativeRestores,
    reconcileNativeFileJobsBeforeBootstrap,
    shouldReconcileNativeFileJobs,
} from './nativeFileJobRecovery'
import type { NativeFileJobStatus } from './nativeFileJobs'

async function reconcileNativeRestoresBeforeBootstrap(
    dependencies: Parameters<typeof reconcileNativeFileJobsBeforeBootstrap>[0],
): Promise<string[]> {
    const result = await reconcileNativeFileJobsBeforeBootstrap(dependencies)
    return result.pendingRestoreAcknowledgements
}

function restoreStatus(
    jobId: string,
    state: NativeFileJobStatus['state'],
    phase: NativeFileJobStatus['phase'],
): NativeFileJobStatus {
    return {
        jobId,
        kind: 'restore-block-risu-save',
        state,
        phase,
        progress: { completedBytes: 0, completedItems: 0 },
        result: state === 'succeeded' ? {
            revision: 2,
            sourceBytes: 128,
            sourceSha256: 'a'.repeat(64),
            characterCount: 1,
            presetCount: 0,
            warningCodes: [],
        } : undefined,
    }
}

describe('native file job bootstrap reconciliation', () => {
    it('reconciles restore jobs on every Tauri target', () => {
        expect(shouldReconcileNativeFileJobs(true, false, false)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, true, true)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, true, false)).toBe(true)
        expect(shouldReconcileNativeFileJobs(false, false, true)).toBe(false)
    })

    it('waits for an active restore, finalizes staged data, and retains success for plugin reload', async () => {
        const calls: string[] = []
        const statuses = [
            restoreStatus('restore-1', 'waitingForInput', 'awaiting-activation'),
            restoreStatus('restore-1', 'succeeded', 'complete'),
        ]

        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') {
                    return [restoreStatus('restore-1', 'running', 'reading-source')]
                }
                if (command === 'native_file_job_status') return statuses.shift()
                if (command === 'native_file_job_finalize') return 'requested'
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual(['restore-1'])
        expect(calls).toEqual([
            'native_file_job_list',
            'native_file_job_status',
            'native_file_job_finalize',
            'native_file_job_status',
        ])
        expect(calls).not.toContain('native_file_job_forget')
    })

    it('acknowledges failed restores immediately', async () => {
        const forgotten: string[] = []
        const failed = restoreStatus('restore-failed', 'failed', 'complete')
        failed.error = { code: 'corrupt-input', message: 'bad file' }

        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command, args) => {
                if (command === 'native_file_job_list') return [
                    failed,
                ]
                if (command === 'native_file_job_forget') {
                    forgotten.push(String(args?.jobId))
                    return true
                }
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(forgotten).toEqual(['restore-failed'])
    })

    it.each([
        ['restore-official-account-snapshot', 'official-restore'],
        ['restore-legacy-local-backup', 'legacy-restore'],
    ] as const)('retains a successful %s for post-bootstrap acknowledgement', async (kind, jobId) => {
        const calls: string[] = []
        const completedRestore = {
            ...restoreStatus(jobId, 'succeeded', 'complete'),
            kind,
        }

        const result = await reconcileNativeFileJobsBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [completedRestore]
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(result).toEqual({
            pendingRestoreAcknowledgements: [jobId],
            pendingOfficialPublications: [],
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it.each([
        'prepare-content-import',
        'import-jpeg-asset',
    ] as const)('cancels and drains a nonterminal %s job without restore finalization', async (kind) => {
        const calls: string[] = []
        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('content-1', 'waitingForInput', 'awaiting-content-mapping'),
                    kind,
                }]
                if (command === 'native_file_job_cancel') return 'requested'
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('content-1', 'cancelled', 'complete'),
                    kind,
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(calls).toEqual([
            'native_file_job_list',
            'native_file_job_cancel',
            'native_file_job_status',
            'native_file_job_forget',
        ])
        expect(calls).not.toContain('native_file_job_finalize')
    })

    it('forgets terminal content jobs without adding restore acknowledgement', async () => {
        const calls: string[] = []
        const pending = await reconcileNativeRestoresBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('content-1', 'failed', 'complete'),
                    kind: 'prepare-content-import' as const,
                }]
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        })

        expect(pending).toEqual([])
        expect(calls).toEqual(['native_file_job_list', 'native_file_job_forget'])
    })

    it.each([
        ['export-block-risu-save', 'export-1'],
        ['export-legacy-local-backup', 'legacy-export-1'],
        ['export-compatible-local-backup', 'compatible-export-1'],
        ['export-character-charx', 'charx-export-1'],
        ['kei-backup-upload', 'kei-1'],
    ] as const)('returns without waiting for an active %s job and cleans it up in the background', async (kind, jobId) => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: string[] = []
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus(jobId, 'running', 'writing-export'),
                    kind,
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus(jobId, 'succeeded', 'complete'),
                    kind,
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual(['native_file_job_list'])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                'native_file_job_list',
                'native_file_job_status',
                'native_file_job_forget',
            ])
        })
    })

    it('cleans an abandoned Android lossless handoff before forgetting its terminal job', async () => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath =
            'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174004.bin'
        const dependencies = {
            invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_list')
                    return [
                        {
                            ...restoreStatus(
                                'lossless-export',
                                'running',
                                'writing-export',
                            ),
                            kind: 'export-legacy-local-backup' as const,
                        },
                    ]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus(
                        'lossless-export',
                        'succeeded',
                        'complete',
                    ),
                    kind: 'export-legacy-local-backup' as const,
                    result: {
                        ...restoreStatus(
                            'lossless-export',
                            'succeeded',
                            'complete',
                        ).result!,
                        handoffPath,
                    },
                }
                if (command === 'native_legacy_backup_handoff_cleanup')
                    return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual([['native_file_job_list', undefined]])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                ['native_file_job_list', undefined],
                ['native_file_job_status', { jobId: 'lossless-export' }],
                ['native_legacy_backup_handoff_cleanup', { path: handoffPath }],
                ['native_file_job_forget', { jobId: 'lossless-export' }],
            ])
        })
    })

    it('preserves a lossless handoff still owned by persisted Android SAF state', async () => {
        const calls: string[] = []
        const exportId = '123e4567-e89b-42d3-a456-426614174004'
        const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\risu-backup-${exportId}.bin`
        const androidSafExportId = vi.fn(() => exportId)
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list')
                    return [
                        {
                            ...restoreStatus(
                                'lossless-export',
                                'succeeded',
                                'complete',
                            ),
                            kind: 'export-legacy-local-backup' as const,
                            result: {
                                ...restoreStatus(
                                    'lossless-export',
                                    'succeeded',
                                    'complete',
                                ).result!,
                                handoffPath,
                            },
                        },
                    ]
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
            androidSafExportId,
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => {
            expect(androidSafExportId).toHaveBeenCalledOnce()
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it.each(['export-legacy-local-backup', 'export-compatible-local-backup'] as const)(
        'preserves a %s handoff still owned by persisted Android SAF state',
        async (kind) => {
            const calls: string[] = []
            const exportId = '123e4567-e89b-42d3-a456-426614174004'
            const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\risu-backup-${exportId}.bin`
            const androidSafExportId = vi.fn(() => exportId)
            const dependencies = {
                invoke: vi.fn(async (command: string) => {
                    calls.push(command)
                    if (command === 'native_file_job_list')
                        return [
                            {
                                ...restoreStatus('legacy-export', 'succeeded', 'complete'),
                                kind,
                                result: {
                                    ...restoreStatus('legacy-export', 'succeeded', 'complete')
                                        .result!,
                                    handoffPath,
                                },
                            },
                        ]
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                }),
                wait: vi.fn(async () => undefined),
                androidSafExportId,
            }

            await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
            await vi.waitFor(() => {
                expect(androidSafExportId).toHaveBeenCalledOnce()
            })
            expect(calls).toEqual(['native_file_job_list'])
        },
    )

    it('cleans an abandoned Android legacy backup handoff before forgetting its terminal job', async () => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risu-backup-123e4567-e89b-42d3-a456-426614174004.bin'
        const dependencies = {
            invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('legacy-export', 'running', 'writing-export'),
                    kind: 'export-legacy-local-backup' as const,
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('legacy-export', 'succeeded', 'complete'),
                    kind: 'export-legacy-local-backup' as const,
                    result: {
                        ...restoreStatus('legacy-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }
                if (command === 'native_legacy_backup_handoff_cleanup') return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual([['native_file_job_list', undefined]])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                ['native_file_job_list', undefined],
                ['native_file_job_status', { jobId: 'legacy-export' }],
                ['native_legacy_backup_handoff_cleanup', { path: handoffPath }],
                ['native_file_job_forget', { jobId: 'legacy-export' }],
            ])
        })
    })

    it('cleans an abandoned Android character CharX handoff before forgetting its terminal job', async () => {
        let resumePolling!: () => void
        const pollingGate = new Promise<void>((resolve) => resumePolling = resolve)
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174004.charx'
        const dependencies = {
            invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('charx-export', 'running', 'writing-export'),
                    kind: 'export-character-charx' as const,
                }]
                if (command === 'native_file_job_status') return {
                    ...restoreStatus('charx-export', 'succeeded', 'complete'),
                    kind: 'export-character-charx' as const,
                    result: {
                        ...restoreStatus('charx-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }
                if (command === 'native_character_charx_handoff_cleanup') return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => pollingGate),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        expect(calls).toEqual([['native_file_job_list', undefined]])

        resumePolling()
        await vi.waitFor(() => {
            expect(calls).toEqual([
                ['native_file_job_list', undefined],
                ['native_file_job_status', { jobId: 'charx-export' }],
                ['native_character_charx_handoff_cleanup', { path: handoffPath }],
                ['native_file_job_forget', { jobId: 'charx-export' }],
            ])
        })
    })

    it.each([
        ['export-character-charx', 'risu-charx-123e4567-e89b-42d3-a456-426614174004.jpeg', 'native_character_charx_handoff_cleanup'],
        ['export-character-card', 'risu-character-card-123e4567-e89b-42d3-a456-426614174004.json', 'native_character_card_handoff_cleanup'],
        ['export-character-card', 'risu-character-card-123e4567-e89b-42d3-a456-426614174004.png', 'native_character_card_handoff_cleanup'],
        ['export-risu-module', 'risu-module-123e4567-e89b-42d3-a456-426614174004.risum', 'native_risu_module_handoff_cleanup'],
    ] as const)('cleans an abandoned Android %s handoff before forgetting its terminal job', async (kind, fileName, cleanupCommand) => {
        const calls: Array<[string, Record<string, unknown> | undefined]> = []
        const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\${fileName}`
        const dependencies = {
            invoke: vi.fn(async (command: string, args?: Record<string, unknown>) => {
                calls.push([command, args])
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('character-export', 'succeeded', 'complete'),
                    kind,
                    result: {
                        ...restoreStatus('character-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                if (command === cleanupCommand) return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => {
            expect(calls).toEqual([
                ['native_file_job_list', undefined],
                [cleanupCommand, { path: handoffPath }],
                ['native_file_job_forget', { jobId: 'character-export' }],
            ])
        })
    })

    it.each([
        ['export-character-charx', 'risu-charx-123e4567-e89b-42d3-a456-426614174004.jpeg'],
        ['export-character-card', 'risu-character-card-123e4567-e89b-42d3-a456-426614174004.json'],
        ['export-character-card', 'risu-character-card-123e4567-e89b-42d3-a456-426614174004.png'],
        ['export-risu-module', 'risu-module-123e4567-e89b-42d3-a456-426614174004.risum'],
    ] as const)('preserves a %s handoff still owned by persisted Android SAF state', async (kind, fileName) => {
        const calls: string[] = []
        const exportId = '123e4567-e89b-42d3-a456-426614174004'
        const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\${fileName}`
        const androidSafExportId = vi.fn(() => exportId)
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('character-export', 'succeeded', 'complete'),
                    kind,
                    result: {
                        ...restoreStatus('character-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
            androidSafExportId,
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => expect(androidSafExportId).toHaveBeenCalledOnce())
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('preserves a character CharX handoff still owned by persisted Android SAF state', async () => {
        const calls: string[] = []
        const exportId = '123e4567-e89b-42d3-a456-426614174004'
        const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\risu-charx-${exportId}.charx`
        const androidSafExportId = vi.fn(() => exportId)
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('charx-export', 'succeeded', 'complete'),
                    kind: 'export-character-charx' as const,
                    result: {
                        ...restoreStatus('charx-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
            androidSafExportId,
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => {
            expect(androidSafExportId).toHaveBeenCalledOnce()
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('cleans and forgets a character CharX handoff after Android SAF becomes terminal', async () => {
        const calls: string[] = []
        const exportId = '123e4567-e89b-42d3-a456-426614174004'
        const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\risu-charx-${exportId}.charx`
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('charx-export', 'succeeded', 'complete'),
                    kind: 'export-character-charx' as const,
                    result: {
                        ...restoreStatus('charx-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                if (command === 'native_character_charx_handoff_cleanup') return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
            androidSafExportId: () => getAndroidSafExportSourceId({
                copyExport: vi.fn(),
                getExportSourceId: () => exportId,
                getExportStatus: () => JSON.stringify({ state: 'succeeded' }),
            }),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => {
            expect(calls).toEqual([
                'native_file_job_list',
                'native_character_charx_handoff_cleanup',
                'native_file_job_forget',
            ])
        })
    })

    it('cleans an unclaimed character CharX handoff when the Android SAF bridge is unavailable', async () => {
        const calls: string[] = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174004.charx'
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('charx-export', 'succeeded', 'complete'),
                    kind: 'export-character-charx' as const,
                    result: {
                        ...restoreStatus('charx-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                if (command === 'native_character_charx_handoff_cleanup') return undefined
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
            androidSafExportId: () => getAndroidSafExportSourceId(),
        }

        await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
        await vi.waitFor(() => {
            expect(calls).toEqual([
                'native_file_job_list',
                'native_character_charx_handoff_cleanup',
                'native_file_job_forget',
            ])
        })
    })

    it('retries character CharX handoff cleanup on the next bootstrap before forgetting the job', async () => {
        const calls: string[] = []
        const handoffPath = 'C:\\app\\native-file-jobs\\handoffs\\risu-charx-123e4567-e89b-42d3-a456-426614174004.charx'
        let cleanupAttempts = 0
        const log = vi.spyOn(console, 'error').mockImplementation(() => undefined)
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [{
                    ...restoreStatus('charx-export', 'succeeded', 'complete'),
                    kind: 'export-character-charx' as const,
                    result: {
                        ...restoreStatus('charx-export', 'succeeded', 'complete').result!,
                        handoffPath,
                    },
                }]
                if (command === 'native_character_charx_handoff_cleanup') {
                    cleanupAttempts += 1
                    if (cleanupAttempts === 1) throw new Error('handoff is still in use')
                    return undefined
                }
                if (command === 'native_file_job_forget') return true
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        }

        try {
            await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
            await vi.waitFor(() => expect(cleanupAttempts).toBe(1))
            expect(calls).not.toContain('native_file_job_forget')

            await expect(reconcileNativeRestoresBeforeBootstrap(dependencies)).resolves.toEqual([])
            await vi.waitFor(() => expect(calls).toContain('native_file_job_forget'))
            expect(cleanupAttempts).toBe(2)
        }
        finally {
            log.mockRestore()
        }
    })

    it.each([
        [
            'export-legacy-local-backup',
            'lossless-export',
            'risu-backup-123e4567-e89b-42d3-a456-426614174004.bin',
            'native_legacy_backup_handoff_cleanup',
        ],
        [
            'export-legacy-local-backup',
            'legacy-export',
            'risu-backup-123e4567-e89b-42d3-a456-426614174004.bin',
            'native_legacy_backup_handoff_cleanup',
        ],
        [
            'export-compatible-local-backup',
            'compatible-export',
            'risu-backup-123e4567-e89b-42d3-a456-426614174004.bin',
            'native_legacy_backup_handoff_cleanup',
        ],
    ] as const)(
        'retries %s handoff cleanup on the next bootstrap before forgetting the job',
        async (kind, jobId, fileName, cleanupCommand) => {
            const calls: string[] = []
            const handoffPath = `C:\\app\\native-file-jobs\\handoffs\\${fileName}`
            let cleanupAttempts = 0
            const log = vi
                .spyOn(console, 'error')
                .mockImplementation(() => undefined)
            const dependencies = {
                invoke: vi.fn(async (command: string) => {
                    calls.push(command)
                    if (command === 'native_file_job_list')
                        return [
                            {
                                ...restoreStatus(
                                    jobId,
                                    'succeeded',
                                    'complete',
                                ),
                                kind,
                                result: {
                                    ...restoreStatus(
                                        jobId,
                                        'succeeded',
                                        'complete',
                                    ).result!,
                                    handoffPath,
                                },
                            },
                        ]
                    if (command === cleanupCommand) {
                        cleanupAttempts += 1
                        if (cleanupAttempts === 1)
                            throw new Error('handoff is still in use')
                        return undefined
                    }
                    if (command === 'native_file_job_forget') return true
                    throw new Error(`Unexpected command: ${command}`)
                }),
                wait: vi.fn(async () => undefined),
            }

            try {
                await expect(
                    reconcileNativeRestoresBeforeBootstrap(dependencies),
                ).resolves.toEqual([])
                await vi.waitFor(() => expect(cleanupAttempts).toBe(1))
                expect(calls).not.toContain('native_file_job_forget')

                await expect(
                    reconcileNativeRestoresBeforeBootstrap(dependencies),
                ).resolves.toEqual([])
                await vi.waitFor(() =>
                    expect(calls).toContain('native_file_job_forget'),
                )
                expect(cleanupAttempts).toBe(2)
            } finally {
                log.mockRestore()
            }
        },
    )

    it('returns every publication job for late reconciliation without touching it early', async () => {
        const calls: string[] = []
        const publications: NativeFileJobStatus[] = [
            {
                jobId: 'publication-running',
                kind: 'official-publication-upload',
                state: 'running',
                phase: 'uploading-database',
                progress: { completedBytes: 64, completedItems: 0 },
            },
            {
                jobId: 'publication-succeeded',
                kind: 'official-publication-upload',
                state: 'succeeded',
                phase: 'complete',
                progress: { completedBytes: 128, completedItems: 1 },
            },
            {
                jobId: 'publication-failed',
                kind: 'official-publication-upload',
                state: 'failed',
                phase: 'complete',
                progress: { completedBytes: 64, completedItems: 0 },
            },
            {
                jobId: 'publication-cancelled',
                kind: 'official-publication-upload',
                state: 'cancelled',
                phase: 'complete',
                progress: { completedBytes: 0, completedItems: 0 },
            },
        ]

        const result = await reconcileNativeFileJobsBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return publications
                throw new Error(`Publication recovery touched ${command}`)
            }),
            wait: vi.fn(async () => {
                throw new Error('Publication recovery waited during bootstrap')
            }),
        })

        expect(result).toEqual({
            pendingRestoreAcknowledgements: [],
            pendingOfficialPublications: [
                'publication-running',
                'publication-succeeded',
                'publication-failed',
                'publication-cancelled',
            ],
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('scans publications on Tauri targets where restore jobs are not supported', async () => {
        const calls: string[] = []
        const result = await reconcileNativeFileJobsBeforeBootstrap({
            invoke: vi.fn(async (command) => {
                calls.push(command)
                if (command === 'native_file_job_list') return [
                    restoreStatus('restore-unsupported', 'running', 'reading-source'),
                    {
                        jobId: 'publication-android',
                        kind: 'official-publication-upload',
                        state: 'running',
                        phase: 'uploading-database',
                        progress: { completedBytes: 0, completedItems: 0 },
                    },
                ]
                throw new Error(`Unexpected command: ${command}`)
            }),
            wait: vi.fn(async () => undefined),
        }, { reconcileRestores: false })

        expect(result).toEqual({
            pendingRestoreAcknowledgements: [],
            pendingOfficialPublications: ['publication-android'],
        })
        expect(calls).toEqual(['native_file_job_list'])
    })

    it('forgets committed restores only after the caller reports successful plugin loading', async () => {
        const calls: string[] = []
        const dependencies = {
            invoke: vi.fn(async (command: string) => {
                calls.push(command)
                return true
            }),
            wait: vi.fn(async () => undefined),
        }

        await acknowledgeRecoveredNativeRestores(['restore-1'], dependencies)

        expect(calls).toEqual(['native_file_job_forget'])
        expect(dependencies.invoke).toHaveBeenCalledWith(
            'native_file_job_forget',
            { jobId: 'restore-1' },
        )
    })
})

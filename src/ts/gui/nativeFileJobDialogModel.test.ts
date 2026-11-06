import { language } from 'src/lang'
import { describe, expect, it } from 'vitest'

import { languageEnglish } from 'src/lang/en'
import type {
    NativeFileOperationOutcome,
    NativeFileOperationState,
} from '../storage/nativeFileJobManager'
import {
    emptyNativeImportCounts,
    resolveNativeFileJobStage,
    type NativeFileJobDetail,
    type NativeFileJobFailureOwner,
    type NativeFileJobStage,
    type NativeFileJobStatus,
    type NativeImportCounts,
} from '../storage/nativeFileJobs'
import {
    buildNativeFileJobDialogModel,
    failureReason,
    fillTemplate,
    formatBytes,
    formatElapsed,
} from './nativeFileJobDialogModel'

const copy = languageEnglish.risuNest.importDialog

it('orders account asset download after refresh and plugins and exposes separate outcomes while cancellable', () => {
    const current = status({ kind: 'restore-official-account-snapshot', activationRevision: 8,
        detail: detail('downloading-account-assets', { stageCompleted: 7, stageTotal: 10 }, { characters: 3 }),
        officialAssets: { phase: 'downloading', state: 'running', counts: { required: 10, alreadyPresent: 2, installed: 3, missing: 1, failed: 1, notAttempted: 3 } },
    })
    const history: NativeFileJobStage[] = ['downloading-account-backup', 'reading-database', 'activating', 'cleaning-up', 'refreshing-app', 'reloading-plugins', 'checking-account-assets', 'downloading-account-assets']
    // Once the database read meets cold items its total is unknown, and the bytes read still show.
    const reading = buildNativeFileJobDialogModel(running({ format: 'library-backup', observedStages: history.slice(0, 2), status: status({
        kind: 'restore-official-account-snapshot', detail: detail('reading-database', { stageCompleted: 3 * 1024 * 1024, stageTotal: undefined, stageUnit: 'bytes' }),
    }) }), null, 2000)
    expect([reading.overallPercent, reading.indeterminate, reading.overallText]).toEqual([null, true, '3.0 MiB'])
    expect(reading.stages.find(row => row.stage === 'reading-database')).toMatchObject({ state: 'active', detail: '3.0 MiB' })
    const model = buildNativeFileJobDialogModel(running({ format: 'library-backup', status: current, observedStages: history }), null, 2000)
    expect(model.cancelEnabled).toBe(true)
    expect(model.cancelLabel).toBe(copy.stopAccountAssets)
    expect(model.overallPercent).toBe(70)
    expect(model.stages.find(row => row.stage === 'downloading-account-assets')).toMatchObject({ label: copy.stageDownloadingAccountAssets, state: 'active' })
    expect(model.stages.findIndex(row => row.stage === 'reloading-plugins')).toBeLessThan(model.stages.findIndex(row => row.stage === 'downloading-account-assets'))
    expect(model.stages.find(row => row.stage === 'cleaning-up')?.label).toBe(copy.stageCleaningTemporary)
    expect(model.counters.filter(counter => counter.key.startsWith('account-')).map(counter => counter.value)).toEqual(['10', '2', '3', '1', '1', '3'])
    current.officialAssets!.transfer = { bytes: 1024, bytesPerSecond: 512, active: 1, limit: 1, waitingUntil: 47000 }
    const transferring = buildNativeFileJobDialogModel(running({ format: 'library-backup', status: current, observedStages: history }), null, 2000)
    expect(transferring.counters.filter(counter => ['account-bytes', 'account-rate', 'account-active', 'account-wait'].includes(counter.key)).map(counter => counter.value)).toEqual(['1.0 KiB', '512 B/s', '1/1', '00:45'])
    const failed = buildNativeFileJobDialogModel(null, outcome({ state: 'failed', status: { ...current, officialAssets: { ...current.officialAssets!, state: 'failed', reason: 'service' } }, observedStages: history }), 2000)
    expect(failed.terminal?.reason).toBe(copy.reasonAccountAssetService)
    const finishing = buildNativeFileJobDialogModel(running({ format: 'library-backup', status: current,
        observedStages: [...history, 'finishing-account-restore'] }), null, 2000)
    expect(finishing.cancelEnabled).toBe(false)
    const stopped = buildNativeFileJobDialogModel(null, outcome({ state: 'cancelled', status: { ...current, officialAssets: { ...current.officialAssets!, state: 'cancelled' } }, observedStages: history }), 2000)
    expect(stopped.terminal?.summary).toBe(copy.resultAccountAssetsStopped)
    const missing = buildNativeFileJobDialogModel(null, outcome({ state: 'succeeded', status: { ...current, officialAssets: {
        ...current.officialAssets!, state: 'succeeded', counts: { required: 2, alreadyPresent: 0, installed: 1, missing: 1, failed: 0, notAttempted: 0 },
    } } }), 2000)
    expect(missing.terminal?.summary).toBe(copy.resultAccountAssetsMissing)
    for (const [reason, expected] of [['receipt', copy.reasonAccountAssetReceipt], ['checking', copy.reasonAccountAssetChecking]] as const) {
        const rejected = buildNativeFileJobDialogModel(null, outcome({ state: 'failed', status: {
            ...current, officialAssets: { ...current.officialAssets!, state: 'failed', reason },
        } }), 2000)
        expect(rejected.terminal?.reason).toBe(expected)
    }
})

it('explains that a busy Android destination requires waiting for the current export', () => {
    expect(failureReason('destination-busy', 'export')).toBe(copy.reasonDestinationBusy)
})

it.each(['activating-database', 'cleaning-up'] as const)('does not present source EOF as completed restoration during %s', phase => {
    const stage = phase === 'cleaning-up' ? 'cleaning-up' : 'activating'
    const model = buildNativeFileJobDialogModel(running({status:status({phase,progress:{completedBytes:100,totalBytes:100,completedItems:1},detail:detail(stage)}),observedStages:[stage]}),null,1000)
    expect(model.overallPercent).toBeNull()
    expect(model.indeterminate).toBe(true)
    expect(model.cancelEnabled).toBe(false)
    expect(model.terminal).toBeNull()
})

it('shows dataset publication progress without a terminal outcome or invented counters', () => {
    const current = status({ kind: 'export-dataset', phase: 'publishing-destination', progress: { completedBytes: 5, totalBytes: 10, completedItems: 0 } })
    const model = buildNativeFileJobDialogModel(running({ kind: 'export', format: 'dataset', status: current, observedStages: ['preparing-export', 'writing-export', 'publishing-destination'] }), null, 2000)
    expect(model.title).toBe(languageEnglish.exportAsDataset)
    expect(model.terminal).toBeNull()
    expect(model.cancelEnabled).toBe(true)
    expect(model.counters).toEqual([])
    expect(model.stages.find(row => row.stage === 'publishing-destination')).toMatchObject({ state: 'active', detail: expect.stringContaining('5') })
})

it.each(['downloading-export', 'writing-export', 'verifying-export', 'finalizing-export'] as const)('uses only actual %s work even when aggregate bytes are complete', stage => {
    const current = status({
        kind: 'export-portable-backup', phase: stage === 'finalizing-export' ? 'finalizing-export' : 'writing-export',
        progress: { completedBytes: 300, totalBytes: 300, completedItems: 3 },
        detail: detail(stage, { stageCompleted: stage === 'finalizing-export' ? 0 : 7, stageTotal: undefined, stageUnit: 'bytes' }),
    })
    const model = buildNativeFileJobDialogModel(running({ kind: 'export', format: 'library-backup', status: current, observedStages: [stage] }), null, 2000)
    expect(model.overallPercent).toBeNull()
    expect(model.indeterminate).toBe(true)
    expect(model.overallText).toBe(stage === 'finalizing-export' ? '' : formatBytes(7))
    expect(model.terminal).toBeNull()
    expect(model.stages.find(row => row.stage === stage)?.state).toBe('active')
})

it('shows dataset writing item progress while byte size is unknown', () => {
    const current = status({ kind: 'export-dataset', phase: 'writing-export', progress: { completedBytes: 0, completedItems: 2, totalItems: 5 } })
    const model = buildNativeFileJobDialogModel(running({ kind: 'export', format: 'dataset', status: current, observedStages: ['writing-export'] }), null, 2000)
    expect(model.stages.find(row => row.stage === 'writing-export')?.detail).toBe(fillTemplate(copy.itemsOf, '2', '5'))
    expect(model.overallPercent).toBeNull()
    expect(model.indeterminate).toBe(true)
})

it('shows cancellable body copying after activation using the existing file copy label', () => {
    const current=status({kind:'restore-portable-backup',phase:'copying-missing-bodies',activationRevision:8,detail:detail('activating')})
    expect(resolveNativeFileJobStage(current)).toBe('copying-assets')
    const model=buildNativeFileJobDialogModel(running({format:'library-backup',status:current,observedStages:['activating','copying-assets']}),null,2000)
    expect(model.cancelEnabled).toBe(true)
    expect(model.stages.find(row=>row.stage==='copying-assets')?.label).toBe(copy.stageCopyingSource)
    expect(model.stages.findIndex(row=>row.stage==='activating')).toBeLessThan(model.stages.findIndex(row=>row.stage==='copying-assets'))
})

it('shows a cancellable wait without invented file progress before synchronization settles', () => {
    const model = buildNativeFileJobDialogModel(running({ format: 'content', waitingForSync: true }), null, 1000)
    expect(model.overallText).toBe(copy.waitingForSync)
    expect(model.overallPercent).toBeNull()
    expect(model.indeterminate).toBe(true)
    expect(model.cancelEnabled).toBe(true)
})

function status(patch: Partial<NativeFileJobStatus> = {}): NativeFileJobStatus {
    return {
        jobId: 'job',
        kind: 'restore-legacy-local-backup',
        state: 'running',
        phase: 'reading-source',
        progress: { completedBytes: 0, completedItems: 0 },
        ...patch,
    }
}

function detail(
    stage: NativeFileJobStage,
    patch: Partial<Omit<NativeFileJobDetail, 'stage' | 'counts'>> = {},
    counts: Partial<NativeImportCounts> = {},
): NativeFileJobDetail {
    return {
        stage,
        stageCompleted: 0,
        stageUnit: 'items',
        ...patch,
        counts: { ...emptyNativeImportCounts(), ...counts },
    }
}

function running(patch: Partial<NativeFileOperationState> = {}): NativeFileOperationState {
    return {
        kind: 'import',
        presentation: 'dialog',
        format: 'local-backup',
        startedAt: 1_000,
        observedStages: [],
        blocking: false,
        cancelRequested: false,
        partialWritesPossible: false,
        ...patch,
    }
}

function outcome(patch: Partial<NativeFileOperationOutcome> = {}): NativeFileOperationOutcome {
    return {
        kind: 'import',
        format: 'local-backup',
        startedAt: 1_000,
        finishedAt: 66_000,
        state: 'succeeded',
        observedStages: ['reading-archive', 'finalizing-staging', 'activating', 'refreshing-app', 'reloading-plugins'],
        warningCodes: [],
        partialWritesPossible: false,
        ...patch,
    }
}

function stageIds(model: ReturnType<typeof buildNativeFileJobDialogModel>): string[] {
    return model.stages.map((row) => `${row.stage}:${row.state}`)
}

describe('nativeFileJobDialogModel', () => {
    it.each([
        ['invalid-destination', 'reasonDestination'],
        ['destination-write-failed', 'reasonDestination'],
        ['storage-full', 'reasonStorageFull'],
        ['local-storage-full', 'reasonStorageFull'],
        ['insufficient-storage', 'reasonStorageFull'],
        ['compatibility-import-required', 'reasonCompatibilityImport'],
        ['io-error', 'reasonIo'],
        ['source-picker-failed', 'reasonIo'],
        ['source-copy-failed', 'reasonIo'],
        ['spool-write-failed', 'reasonIo'],
        ['source-open-failed', 'reasonIo'],
        ['source-read-failed', 'reasonIo'],
        ['source-reselect-required', 'reasonReselectSource'],
        ['import-interrupted', 'reasonImportInterrupted'],
        ['unsupported-source', 'reasonUnsupportedFormat'],
        ['publication-pending', 'reasonIncompletePublication'],
        ['length-mismatch', 'reasonIncompletePublication'],
        ['archive-output-failed', 'reasonIncompletePublication'],
        ['cleanup-failed', 'reasonCleanup'],
        ['capability-unavailable', 'reasonCapability'],
        ['rescue-format-not-restorable', 'reasonRescueFormat'],
        ['no-recovery-source', 'reasonNoRecoverySource'],
        ['source-busy', 'reasonBusy'],
        ['destination-required', 'reasonPlainJpeg'],
        ['unsupported-without-destination', 'reasonPlainJpeg'],
        ['discard-failed', 'reasonCleanup'],
        ['generation-active', 'reasonBusy'],
        ['library-operation-busy', 'reasonBusy'],
        ['server-sync-busy', 'reasonBusy'],
        ['resolve-pending-operation-first', 'reasonPendingOperation'],
        ['server-status-unavailable', 'reasonServerStatus'],
        ['remote-asset-unavailable', 'reasonRemoteAssetUnavailable'],
        ['asset-missing', 'reasonAssetMissing'],
        ['unsupported-projection', 'reasonCompatibleUnsupportedValue'],
    ] as const)('presents actionable file failure %s while keeping diagnostics secondary', (code, key) => {
        const model = buildNativeFileJobDialogModel(null, outcome({ state: 'failed', error: { code, message: 'synthetic detail', recoveryRequired: false } }), 0)
        expect(model.terminal?.reason).toBe(copy[key])
        expect(model.terminal?.details).toBe(`[${code}] synthetic detail`)
    })

    it('names the kind of an item too large to import', () => {
        for (const [kind, item] of Object.entries(copy.tooLargeItems)) {
            const model = buildNativeFileJobDialogModel(null, outcome({ state: 'failed', error: {
                code: 'payload-too-large', message: 'synthetic detail', recoveryRequired: false, itemKind: kind,
            } }), 0)
            expect(model.terminal?.reason).toBe(copy.reasonTooLarge.replace('{0}', item))
            const exported = buildNativeFileJobDialogModel(null, outcome({ kind: 'export', state: 'failed', error: {
                code: 'payload-too-large', message: 'synthetic detail', recoveryRequired: false, itemKind: kind,
            } }), 0)
            expect(exported.terminal?.reason).toBe(kind === 'compatible-database'
                ? copy.reasonCompatibleDatabaseTooLarge : copy.reasonExportTooLarge.replace('{0}', item))
        }
        expect(failureReason('payload-too-large', 'import', 'commit')).toBe(copy.reasonUnknown)
        expect(failureReason('payload-too-large')).toBe(copy.reasonUnknown)
    })

    it('names the item whose data a compatible export could not write', () => {
        const location = (owner?: NativeFileJobFailureOwner) => buildNativeFileJobDialogModel(null, outcome({ kind: 'export', state: 'failed', error: {
            code: 'unsupported-projection', message: 'synthetic detail', recoveryRequired: false, owner,
        } }), 0).terminal?.location
        expect(location({ kind: 'chat', name: 'Synthetic chat', character: 'Synthetic' }))
            .toBe(fillTemplate(copy.failureOwners.chat, 'Synthetic', 'Synthetic chat'))
        for (const kind of ['character', 'preset', 'module', 'persona'] as const) {
            expect(location({ kind, name: 'Synthetic' })).toBe(fillTemplate(copy.failureOwners[kind], 'Synthetic'))
        }
        expect(location()).toBe('')
    })

    it('asks to connect sync when a bound restore cannot reach its sync target', () => {
        const model = buildNativeFileJobDialogModel(null, outcome({ state: 'failed', error: { code: 'sync-unavailable', message: 'synthetic detail', recoveryRequired: false } }), 0)
        expect(model.terminal?.reason).toBe(languageEnglish.risuNest.backup.syncUnavailable)
        expect(model.terminal?.reason).not.toBe(copy.reasonRevisionConflict)
    })

    it.each(['revision-conflict', 'store-error'])('uses export wording for %s', code => {
        expect(failureReason(code, 'export')).not.toBe(failureReason(code, 'import'))
        expect(failureReason(code, 'export')).toMatch(/export/i)
    })

    it.each(['upstream-restore-losses', 'source-problems', 'source-preserved-repair-required', 'compatibility-losses',
        'risuai-inlays-excluded', 'converted-inlay-extension', 'converted-inlay-sidecars',
        'converted-inlay-provenance', 'inlay-ids-remapped', 'asset-paths-remapped',
        'opaque-plugin-inlay-references-unverified', 'opaque-plugin-asset-references-unverified',
        'inlay-codec-playback-unverified', 'asset-playback-unverified',
        'android-saf-unavailable', 'post-refresh-followup-failed', 'plugin-values-skipped:2', 'unknown-future-code'])
    ('never uses a warning identifier as primary text: %s', code => {
        const model = buildNativeFileJobDialogModel(null, outcome({ warningCodes: [code] }), 0)
        expect(model.warnings[0]).not.toContain(code)
        expect(model.terminal?.details).toContain(`[${code}]`)
        if (code !== 'unknown-future-code') expect(model.warnings[0]).not.toBe(copy.warningUnknown)
    })

    it('names how many plugin values a restore skipped', () => {
        const model = buildNativeFileJobDialogModel(null, outcome({ warningCodes: ['plugin-values-skipped:2'] }), 0)
        expect(model.warnings).toEqual([copy.warningPluginValuesSkipped.replace('{0}', '2')])
    })

    it('shows the non-atomic save warning only while an Android save runs', () => {
        const saving = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-portable-backup', phase: 'publishing-destination', warningCodes: ['android-saf-provider-not-atomic'] }),
                observedStages: ['preparing-export', 'writing-export', 'finalizing-export', 'publishing-destination'],
            }),
            null,
            0,
        )
        expect(saving.warnings).toEqual([copy.warningProviderNotAtomic])
        const saved = buildNativeFileJobDialogModel(null, outcome({
            kind: 'export', format: 'library-backup', warningCodes: ['android-saf-provider-not-atomic'],
        }), 0)
        expect(saved.terminal?.summary).toBe(copy.resultExportSucceeded)
        expect(saved.warnings).toEqual([])
        expect(saved.terminal?.details).toBe('')
        const interrupted = buildNativeFileJobDialogModel(null, outcome({
            kind: 'export', format: 'library-backup', state: 'cancelled', partialWritesPossible: true,
            warningCodes: ['android-saf-provider-not-atomic', 'partial-destination-may-remain'],
        }), 0)
        expect(interrupted.warnings).toEqual([languageEnglish.screenshotPartialDestinationMayRemain])
        expect(interrupted.terminal?.details).toBe('[partial-destination-may-remain]')
    })

    it('is closed when nothing is running and nothing finished', () => {
        expect(buildNativeFileJobDialogModel(null, null, 0).open).toBe(false)
        expect(
            buildNativeFileJobDialogModel(
                running({ presentation: 'inline' }),
                null,
                0,
            ).open,
        ).toBe(false)
    })

    it('opens with the expected pending stages before the first status arrives', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                source: { name: 'risu-backup.bin', bytes: 3 * 1024 * 1024 },
            }),
            null,
            31_000,
        )
        expect(model.open).toBe(true)
        expect(model.title).toBe(copy.titleLocalBackup)
        expect(model.subtitle).toBe('')
        expect(model.sourceName).toBe('risu-backup.bin')
        expect(model.sourceSize).toBe('3.0 MiB')
        expect(model.elapsed).toBe(fillTemplate(copy.elapsed, '00:30'))
        expect(model.indeterminate).toBe(true)
        expect(model.overallPercent).toBeNull()
        expect(stageIds(model)).toEqual([
            'reading-archive:pending',
            'preparing-attachments:pending',
            'reading-database:pending',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=–',
            'presets=–',
            'assets=–',
            'inlays=–',
            'coldStorage=–',
            'pocketMedia=–',
            'skipped=–',
        ])
        expect(model.cancelVisible).toBe(true)
        expect(model.cancelEnabled).toBe(true)
        expect(model.cancelLabel).toBe(copy.cancel)
        expect(model.closeVisible).toBe(false)
        expect(model.terminal).toBeNull()
    })

    it('falls back to job phases when a status carries no detail and drops skipped expected stages', () => {
        const reading = buildNativeFileJobDialogModel(
            running({
                status: status({
                    progress: {
                        completedBytes: 512 * 1024,
                        totalBytes: 1024 * 1024,
                        completedItems: 0,
                    },
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(stageIds(reading)[0]).toBe('reading-archive:active')
        expect(reading.stages[0].detail).toBe(
            fillTemplate(copy.overall, '512 KiB', '1.0 MiB'),
        )
        expect(reading.overallPercent).toBeNull()
        expect(reading.indeterminate).toBe(true)

        const staging = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'staging-database',
                    progress: {
                        completedBytes: 1024 * 1024,
                        totalBytes: 1024 * 1024,
                        completedItems: 2,
                    },
                }),
                observedStages: ['reading-archive', 'finalizing-staging'],
            }),
            null,
            1_000,
        )
        expect(stageIds(staging)).toEqual([
            'reading-archive:done',
            'finalizing-staging:active',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
    })

    it('renders detail stages with their own progress, counters, current item, and format subtitle', () => {
        const counts = {
            entriesRead: 12,
            assets: 5,
            inlays: 2,
            coldStorage: 1,
            pocketMedia: 3,
            pocketMetadata: 3,
            skipped: 1,
        }
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({
                    progress: {
                        completedBytes: 40,
                        totalBytes: 200,
                        completedItems: 12,
                    },
                    detail: detail(
                        'reading-archive',
                        {
                            stageCompleted: 40,
                            stageTotal: 100,
                            stageUnit: 'bytes',
                            currentItem: 'inlay/abc.webp',
                        },
                        counts,
                    ),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(model.subtitle).toBe(copy.formatPocketRisu)
        expect(model.stages[0]).toMatchObject({
            stage: 'reading-archive',
            state: 'active',
            detail: fillTemplate(copy.overall, '40 B', '100 B'),
        })
        expect(model.currentItem).toBe(
            fillTemplate(copy.currentItem, 'inlay/abc.webp'),
        )
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=–',
            'presets=–',
            'assets=5',
            'inlays=2',
            'coldStorage=1',
            'pocketMedia=3',
            'skipped=1',
        ])

        const preparing = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'preparing-attachments',
                        {
                            stageCompleted: 3,
                            stageTotal: 11,
                            currentItem: 'assets/a.png',
                        },
                        { ...counts, entriesTotal: 12, attachmentsPrepared: 3 },
                    ),
                }),
                observedStages: ['reading-archive', 'preparing-attachments'],
            }),
            null,
            1_000,
        )
        expect(stageIds(preparing).slice(0, 2)).toEqual([
            'reading-archive:done',
            'preparing-attachments:active',
        ])
        expect(preparing.stages[0].detail).toBe(
            fillTemplate(copy.itemsCount, '12'),
        )
        expect(preparing.stages[1].detail).toBe(
            fillTemplate(copy.itemsOf, '3', '11'),
        )
        expect(preparing.currentItem).toBe(
            fillTemplate(copy.currentItem, 'assets/a.png'),
        )

        const decoding = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'decoding-database',
                        { stageCompleted: 4096, stageTotal: 8192, stageUnit: 'bytes' },
                        {
                            ...counts,
                            entriesTotal: 12,
                            characters: 4,
                            charactersTotal: 9,
                            presets: 2,
                        },
                    ),
                }),
                observedStages: [
                    'reading-archive',
                    'preparing-attachments',
                    'reading-database',
                    'decoding-database',
                ],
            }),
            null,
            1_000,
        )
        expect(stageIds(decoding)).toEqual([
            'reading-archive:done',
            'preparing-attachments:done',
            'reading-database:done',
            'decoding-database:active',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(decoding.currentItem).toBe('')
        expect(decoding.counters[0].value).toBe(
            fillTemplate(copy.itemsOf, '4', '9'),
        )
        expect(decoding.counters[1].value).toBe('2')
    })

    it('labels a RisuAI backup only once every entry has been classified', () => {
        const partial = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'reading-archive',
                        {},
                        { entriesRead: 3, assets: 3 },
                    ),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            0,
        )
        expect(partial.subtitle).toBe('')
        const complete = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail(
                        'preparing-attachments',
                        {},
                        { entriesRead: 3, entriesTotal: 3, assets: 3 },
                    ),
                }),
                observedStages: ['reading-archive', 'preparing-attachments'],
            }),
            null,
            0,
        )
        expect(complete.subtitle).toBe(copy.formatRisuAi)
    })

    it('names the format after the reported job when the picker admitted it as a library backup', () => {
        const counts = { entriesRead: 3, pocketMedia: 1, pocketMetadata: 1 }
        const archive = buildNativeFileJobDialogModel(
            running({
                format: 'library-backup',
                status: status({
                    kind: 'restore-legacy-local-backup',
                    detail: detail('reading-archive', {}, counts),
                }),
                observedStages: ['reading-archive'],
            }),
            null,
            1_000,
        )
        expect(archive.title).toBe(copy.titleLocalBackup)
        expect(archive.subtitle).toBe(copy.formatPocketRisu)
        expect(archive.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
            'assets',
            'inlays',
            'coldStorage',
            'pocketMedia',
            'skipped',
        ])
        expect(stageIds(archive)).toContain('preparing-attachments:pending')

        const portable = buildNativeFileJobDialogModel(
            running({
                format: 'library-backup',
                status: status({ kind: 'restore-portable-backup' }),
                observedStages: ['reading-database'],
            }),
            null,
            1_000,
        )
        expect(portable.title).toBe(copy.titleBackup)
        expect(portable.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
        ])

        const unknown = buildNativeFileJobDialogModel(
            running({ format: 'library-backup' }),
            null,
            1_000,
        )
        expect(unknown.title).toBe(copy.titleBackup)

        for (const kind of ['restore-native-snapshot', 'snapshot-bodies'] as const) {
            const snapshot = buildNativeFileJobDialogModel(
                running({ format: 'library-backup', status: status({ kind }) }),
                null,
                1_000,
            )
            expect(snapshot.title).toBe(copy.titleSnapshot)
        }
    })

    it('uses the RisuSave template and inserts optional stages only when observed', () => {
        const base = running({
            format: 'risu-save',
            status: status({ kind: 'restore-block-risu-save' }),
        })
        const initial = buildNativeFileJobDialogModel(base, null, 0)
        expect(initial.title).toBe(copy.titleRisuSave)
        expect(stageIds(initial)).toEqual([
            'reading-database:pending',
            'finalizing-staging:pending',
            'activating:pending',
            'refreshing-app:pending',
            'reloading-plugins:pending',
        ])
        expect(initial.counters.map((counter) => counter.key)).toEqual([
            'characters',
            'presets',
        ])

        const decoding = buildNativeFileJobDialogModel(
            running({
                ...base,
                status: status({
                    kind: 'restore-block-risu-save',
                    detail: detail('decoding-database'),
                }),
                observedStages: ['reading-database', 'decoding-database'],
            }),
            null,
            0,
        )
        expect(stageIds(decoding).slice(0, 3)).toEqual([
            'reading-database:done',
            'decoding-database:active',
            'finalizing-staging:pending',
        ])
    })

    it('starts the stage list over after a compatibility re-selection', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({
                    detail: detail('reading-archive', { stageCompleted: 1 }),
                }),
                observedStages: [
                    'reading-archive',
                    'awaiting-reselect',
                    'reading-archive',
                ],
            }),
            null,
            0,
        )
        expect(stageIds(model).slice(0, 2)).toEqual([
            'awaiting-reselect:done',
            'reading-archive:active',
        ])
    })

    it('disables cancellation once data is being applied and reflects a pending cancel request', () => {
        const activating = buildNativeFileJobDialogModel(
            running({
                status: status({ phase: 'activating-database' }),
                observedStages: [
                    'reading-archive',
                    'finalizing-staging',
                    'awaiting-activation',
                    'activating',
                ],
            }),
            null,
            0,
        )
        expect(activating.cancelEnabled).toBe(false)
        expect(activating.cancelNote).toBe(copy.cancelUnavailable)
        expect(stageIds(activating)).toContain('activating:active')

        const awaiting = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'awaiting-activation',
                    state: 'waitingForInput',
                }),
                observedStages: [
                    'reading-archive',
                    'finalizing-staging',
                    'awaiting-activation',
                ],
            }),
            null,
            0,
        )
        expect(awaiting.cancelEnabled).toBe(true)

        const refreshing = buildNativeFileJobDialogModel(
            running({
                status: status({
                    phase: 'complete',
                    state: 'succeeded',
                    detail: detail('refreshing-app'),
                }),
                observedStages: [
                    'reading-archive',
                    'activating',
                    'refreshing-app',
                ],
            }),
            null,
            0,
        )
        expect(refreshing.cancelEnabled).toBe(false)

        const cancelling = buildNativeFileJobDialogModel(
            running({ cancelRequested: true }),
            null,
            0,
        )
        expect(cancelling.cancelEnabled).toBe(false)
        expect(cancelling.cancelLabel).toBe(copy.cancelling)
        expect(cancelling.cancelNote).toBe('')
    })

    it('summarizes a successful import with final counts, a complete row, and mapped warnings', () => {
        const model = buildNativeFileJobDialogModel(
            null,
            outcome({
                source: { name: 'risu-backup.bin', bytes: 2048 },
                status: status({
                    state: 'succeeded',
                    phase: 'complete',
                    progress: {
                        completedBytes: 4096,
                        totalBytes: 4096,
                        completedItems: 9,
                    },
                    detail: detail(
                        'activating',
                        {},
                        {
                            entriesRead: 9,
                            entriesTotal: 9,
                            assets: 4,
                            inlays: 3,
                            coldStorage: 2,
                            characters: 7,
                            presets: 2,
                        },
                    ),
                    result: {
                        revision: 3,
                        sourceBytes: 2048,
                        sourceFingerprintKind: 'whole-file-sha256' as const, sourceSha256: 'x',
                        characterCount: 7,
                        presetCount: 2,
                        warningCodes: ['cleanup-failed'],
                    },
                }),
                result: {
                    revision: 3,
                    sourceBytes: 2048,
                    sourceFingerprintKind: 'whole-file-sha256' as const, sourceSha256: 'x',
                    characterCount: 7,
                    presetCount: 2,
                    warningCodes: ['cleanup-failed'],
                },
                warningCodes: ['cleanup-failed', 'mystery-code'],
            }),
            0,
        )
        expect(model.open).toBe(true)
        expect(model.terminal).toMatchObject({
            state: 'succeeded',
            summary: copy.resultSucceeded,
            reason: '',
            details: '[cleanup-failed]\n[mystery-code]',
            restarting: false,
        })
        expect(model.subtitle).toBe(copy.formatRisuAi)
        expect(model.elapsed).toBe(fillTemplate(copy.elapsed, '01:05'))
        expect(model.overallPercent).toBe(100)
        expect(model.indeterminate).toBe(false)
        expect(stageIds(model)).toEqual([
            'reading-archive:done',
            'finalizing-staging:done',
            'activating:done',
            'refreshing-app:done',
            'reloading-plugins:done',
            'complete:done',
        ])
        expect(
            model.counters.map((counter) => `${counter.key}=${counter.value}`),
        ).toEqual([
            'characters=7',
            'presets=2',
            'assets=4',
            'inlays=3',
            'coldStorage=2',
            'pocketMedia=0',
            'skipped=0',
        ])
        expect(model.warnings).toEqual([
            copy.warningCleanupFailed,
            fillTemplate(copy.warningUnknown, 'mystery-code'),
        ])
        expect(model.cancelVisible).toBe(false)
        expect(model.closeVisible).toBe(true)
    })

    it('keeps the dialog open without a close button while the app restarts', () => {
        const model = buildNativeFileJobDialogModel(
            null,
            outcome({
                observedStages: [
                    'reading-archive',
                    'decoding-database',
                    'activating',
                    'restarting-app',
                ],
            }),
            0,
        )
        expect(model.terminal?.summary).toBe(copy.resultRestarting)
        expect(model.terminal?.restarting).toBe(true)
        expect(model.closeVisible).toBe(false)
        expect(stageIds(model).at(-1)).toBe('restarting-app:done')
    })

    it('explains cancellations and failures without leaking raw codes into the summary', () => {
        const cancelled = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'cancelled',
                observedStages: ['reading-archive'],
                status: status({
                    progress: {
                        completedBytes: 10,
                        totalBytes: 100,
                        completedItems: 1,
                    },
                }),
            }),
            0,
        )
        expect(cancelled.terminal).toMatchObject({
            state: 'cancelled',
            summary: copy.resultCancelled,
            details: '',
        })
        expect(cancelled.overallPercent).toBeNull()
        expect(stageIds(cancelled)).toEqual(['reading-archive:stopped'])

        const partial = buildNativeFileJobDialogModel(
            null,
            outcome({ state: 'cancelled', partialWritesPossible: true }),
            0,
        )
        expect(partial.terminal?.summary).toBe(copy.resultCancelledPartial)

        const failed = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'failed',
                observedStages: ['reading-archive', 'preparing-attachments'],
                error: {
                    code: 'unsupported-format',
                    message:
                        'PocketRisu legacy JSON Inlays require the compatibility importer',
                    recoveryRequired: false,
                },
            }),
            0,
        )
        expect(failed.terminal).toMatchObject({
            state: 'failed',
            summary: copy.resultFailed,
            reason: copy.reasonUnsupportedFormat,
            details:
                '[unsupported-format] PocketRisu legacy JSON Inlays require the compatibility importer',
        })
        expect(stageIds(failed)).toEqual([
            'reading-archive:done',
            'preparing-attachments:stopped',
        ])
        expect(failed.closeVisible).toBe(true)

        const committed = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'failed',
                error: {
                    code: 'activation-committed-refresh-failed',
                    message: 'refresh exploded',
                    recoveryRequired: true,
                },
            }),
            0,
        )
        expect(committed.terminal?.summary).toBe(copy.resultFailedAfterCommit)
        expect(committed.terminal?.reason).toBe('')
        expect(committed.terminal?.details).toBe(
            '[activation-committed-refresh-failed] refresh exploded',
        )

        const assetsMissing = buildNativeFileJobDialogModel(
            null,
            outcome({
                state: 'failed',
                error: { code: 'portable-bodies-incomplete', message: 'bodies missing', recoveryRequired: true },
            }),
            0,
        )
        expect(assetsMissing.terminal?.summary).toBe(copy.resultAssetsIncomplete)
    })

    it('maps failure codes to user copy and never shows the code itself', () => {
        const codes = [
            'unsupported-format',
            'invalid-source',
            'invalid-input',
            'truncated-input',
            'corrupt-input',
            'revision-conflict',
            'store-error',
            'missing-result',
            'missing-activation-fence',
            'import-error',
            'whatever',
        ]
        for (const code of codes) {
            const reason = failureReason(code)
            expect(reason.length).toBeGreaterThan(0)
            expect(reason).not.toContain(code)
        }
        expect(failureReason('damaged-image')).toBe(language.risuNest.imageFormats.damaged)
        expect(failureReason('whatever')).toBe(copy.reasonUnknown)
    })

    it.each([
        ['backup-encryption-invalid', 'reasonBackupEncryptionInvalid'],
        ['backup-key-request-failed', 'reasonBackupKeyRequest'],
        ['backup-key-http-error', 'reasonBackupKeyHttp'],
        ['backup-key-invalid', 'reasonBackupKeyInvalid'],
        ['backup-decryption-failed', 'reasonBackupDecryption'],
        ['backup-database-invalid', 'reasonBackupDatabase'],
    ] as const)('explains %s in the failure dialog', (code, reasonKey) => {
        const model = buildNativeFileJobDialogModel(null, outcome({
            state: 'failed', error: { code, message: 'Backup restore failed', recoveryRequired: false },
        }), 0)
        expect(model.terminal?.reason).toBe(copy[reasonKey])
    })

    it('never exposes stage identifiers in labels', () => {
        const stages: NativeFileJobStage[] = [
            'copying-source',
            'awaiting-reselect',
            'reading-archive',
            'preparing-attachments',
            'reading-database',
            'decoding-database',
            'finalizing-staging',
            'awaiting-activation',
            'activating',
            'refreshing-app',
            'reloading-plugins',
            'restarting-app',
        ]
        const model = buildNativeFileJobDialogModel(
            running({
                status: status({ detail: detail('restarting-app') }),
                observedStages: stages,
            }),
            null,
            0,
        )
        for (const row of model.stages) {
            expect(row.label).not.toContain(row.stage)
            expect(row.label.length).toBeGreaterThan(0)
        }
    })

    it('shows an export as its reported phases with export wording', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({
                    kind: 'export-portable-backup',
                    phase: 'writing-export',
                    progress: { completedBytes: 512, totalBytes: 2048, completedItems: 0 },
                    detail: detail('writing-export', { stageCompleted: 512, stageTotal: undefined, stageUnit: 'bytes' }),
                }),
                observedStages: ['preparing-export', 'writing-export'],
            }),
            null,
            5_000,
        )
        expect(model.open).toBe(true)
        expect(model.title).toBe(copy.titleExportBackup)
        expect(model.subtitle).toBe('')
        expect(stageIds(model)).toEqual([
            'preparing-export:done',
            'writing-export:active',
            'finalizing-export:pending',
        ])
        expect(model.overallPercent).toBeNull()
        expect(model.overallText).toBe(formatBytes(512))
        expect(model.indeterminate).toBe(true)
        expect(model.counters).toEqual([])
        expect(model.cancelVisible).toBe(true)
        expect(model.cancelEnabled).toBe(true)
        expect(model.cancelLabel).toBe(copy.cancelExport)
    })

    it('names the export after the file it writes before the first status arrives', () => {
        const risuSave = buildNativeFileJobDialogModel(
            running({ kind: 'export', format: 'risu-save' }),
            null,
            0,
        )
        expect(risuSave.title).toBe(copy.titleExportRisuSave)
        expect(stageIds(risuSave)).toEqual([
            'preparing-export:pending',
            'writing-export:pending',
            'finalizing-export:pending',
        ])
        const compatible = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-compatible-local-backup', phase: 'reading-source' }),
                observedStages: ['preparing-export'],
            }),
            null,
            0,
        )
        expect(compatible.title).toBe(copy.titleExportCompatible)
    })

    it('adds the save-to-location step only when a mobile export reports it', () => {
        const model = buildNativeFileJobDialogModel(
            running({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-portable-backup', phase: 'publishing-destination' }),
                observedStages: ['preparing-export', 'writing-export', 'finalizing-export', 'publishing-destination'],
            }),
            null,
            0,
        )
        expect(model.stages.map((row) => row.stage)).toEqual([
            'preparing-export',
            'writing-export',
            'publishing-destination',
            'finalizing-export',
        ])
        expect(model.stages.find((row) => row.stage === 'publishing-destination')?.state).toBe('active')
    })

    it('summarizes how an export ended with export wording', () => {
        const succeeded = buildNativeFileJobDialogModel(
            null,
            outcome({
                kind: 'export',
                format: 'library-backup',
                status: status({ kind: 'export-portable-backup', phase: 'complete', state: 'succeeded' }),
                observedStages: ['preparing-export', 'writing-export', 'finalizing-export'],
            }),
            0,
        )
        expect(succeeded.title).toBe(copy.titleExportBackup)
        expect(succeeded.terminal?.summary).toBe(copy.resultExportSucceeded)
        expect(succeeded.terminal?.restarting).toBe(false)
        expect(stageIds(succeeded)).toEqual([
            'preparing-export:done',
            'writing-export:done',
            'finalizing-export:done',
            'complete:done',
        ])
        expect(succeeded.closeVisible).toBe(true)
        expect(succeeded.counters).toEqual([])

        const cancelled = buildNativeFileJobDialogModel(
            null,
            outcome({ kind: 'export', format: 'risu-save', state: 'cancelled', observedStages: ['preparing-export'] }),
            0,
        )
        expect(cancelled.terminal?.summary).toBe(copy.resultExportCancelled)

        const failed = buildNativeFileJobDialogModel(
            null,
            outcome({
                kind: 'export',
                format: 'risu-save',
                state: 'failed',
                observedStages: ['preparing-export', 'writing-export'],
                error: { code: 'store-error', message: 'disk full', recoveryRequired: false },
            }),
            0,
        )
        expect(failed.terminal?.summary).toBe(copy.resultExportFailed)
        expect(failed.terminal?.reason).toBe(copy.reasonExportStoreError)
        expect(failed.terminal?.summary).not.toContain('import')
    })

    it('keeps the rescue archive on its archive presentation', () => {
        const model = buildNativeFileJobDialogModel(
            running({ kind: 'export', format: 'raw-recovery', observedStages: ['reading-archive'] }),
            null,
            0,
        )
        expect(model.title).toBe(languageEnglish.risuNest.recovery.exportTitle)
        expect(model.stages.map((row) => row.stage)).toEqual(['reading-archive'])
    })

    it('formats bytes and elapsed time', () => {
        expect(formatBytes(0)).toBe('0 B')
        expect(formatBytes(1023)).toBe('1023 B')
        expect(formatBytes(1536)).toBe('1.5 KiB')
        expect(formatBytes(150 * 1024 * 1024)).toBe('150 MiB')
        expect(formatBytes(1.25 * 1024 ** 3)).toBe('1.3 GiB')
        expect(formatBytes(-1)).toBe('')
        expect(formatElapsed(0)).toBe('00:00')
        expect(formatElapsed(65_000)).toBe('01:05')
        expect(formatElapsed(3_725_000)).toBe('1:02:05')
        expect(fillTemplate('{0} / {1}', 'a', 2)).toBe('a / 2')
    })
})

it('uses compact content progress and keeps cancellation available during metadata mapping', () => {
    const state = running({
        format: 'content',
        observedStages: ['preparing-attachments'],
        status: status({
            kind: 'prepare-content-import',
            detail: detail(
                'preparing-attachments',
                { stageCompleted: 1250, stageTotal: 2500 },
                { assets: 1250, attachmentsPrepared: 1250 },
            ),
        }),
    })
    const model = buildNativeFileJobDialogModel(state, null, 5000)
    expect(model.compact).toBe(true)
    expect(model.overallPercent).toBe(50)
    expect(model.counters.map((row) => row.key)).toEqual(['assets'])
    state.status!.state = 'succeeded'
    state.status!.detail = detail('finalizing-staging')
    state.observedStages.push('finalizing-staging')
    expect(buildNativeFileJobDialogModel(state, null, 6000).cancelEnabled).toBe(
        true,
    )
})

it.each([false, true])('presents background interruption while retaining partial-write semantics: %s', partialWritesPossible => {
    const model = buildNativeFileJobDialogModel(null, {
        kind: 'export', format: 'library-backup', startedAt: 1, finishedAt: 2,
        state: 'cancelled', observedStages: [], warningCodes: [], partialWritesPossible,
        interruption: 'background-expired',
    }, 3)
    expect(model.terminal?.summary).toBe(partialWritesPossible ? copy.resultBackgroundExpiredPartial : copy.resultBackgroundExpired)
})

it.each([
        ['queued', 'preparing-export'], ['reading-source', 'preparing-export'],
        ['writing-export', 'writing-export'], ['uploading-database', 'publishing-destination'],
        ['awaiting-publication-retry', 'publishing-destination'],
        ['finalizing-publication', 'finalizing-export'], ['complete', 'finalizing-export'],
    ] as const)('maps official publication phase %s to its visible %s stage', (phase, expected) => {
        expect(resolveNativeFileJobStage(status({ kind: 'official-publication-upload', phase }))).toBe(expected)
    })

    it.each([['succeeded', 'finalizing-export'], ['failed', null], ['cancelled', null]] as const)(
        'leaves the stopped stage current when an export job ends %s',
        (state, expected) => {
            for (const kind of ['export-compatible-local-backup', 'official-publication-upload'] as const) {
                expect(resolveNativeFileJobStage(status({ kind, phase: 'complete', state }))).toBe(expected)
            }
        },
    )

    it('shows official asset counts followed by database byte progress in the active upload stage', () => {
        const assetStatus = status({
            kind: 'official-publication-upload', phase: 'reading-source',
            detail: detail('publishing-destination', { stageCompleted: 2, stageTotal: 5, stageUnit: 'items' }),
        })
        const assets = buildNativeFileJobDialogModel(running({
            kind: 'export', format: 'library-backup', status: assetStatus,
            observedStages: [resolveNativeFileJobStage(assetStatus)!],
        }), null, 2_000)
        expect(assets.title).toBe(languageEnglish.risuNest.backup.officialPublish)
        expect(assets.stages.find(stage => stage.stage === 'publishing-destination')).toMatchObject({
            state: 'active', detail: fillTemplate(copy.itemsOf, '2', '5'),
        })
        const uploadStatus = status({
            kind: 'official-publication-upload', phase: 'uploading-database',
            progress: { completedBytes: 1_024, totalBytes: 4_096, completedItems: 0 },
            detail: detail('publishing-destination', { stageCompleted: 256, stageTotal: 2_048, stageUnit: 'bytes' }),
        })
        const upload = buildNativeFileJobDialogModel(running({
            kind: 'export', format: 'library-backup', status: uploadStatus,
            observedStages: ['publishing-destination', resolveNativeFileJobStage(uploadStatus)!],
        }), null, 3_000)
        expect(upload.overallPercent).toBe(13)
        expect(upload.indeterminate).toBe(false)
        expect(upload.stages.find(stage => stage.stage === 'publishing-destination')).toMatchObject({
            state: 'active', detail: fillTemplate(copy.overall, formatBytes(256), formatBytes(2_048)),
        })
        expect(upload.cancelEnabled).toBe(true)
    })

    it('names a native account restore in the dialog', () => {
        const model = buildNativeFileJobDialogModel(running({
            format: 'library-backup', status: status({ kind: 'restore-official-account-snapshot' }),
        }), null, 2_000)
        expect(model.title).toBe(languageEnglish.risuNest.backup.officialRestore)
    })

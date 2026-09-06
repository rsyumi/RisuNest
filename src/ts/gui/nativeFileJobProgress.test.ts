import { describe, expect, it } from 'vitest'

import appSource from '../../App.svelte?raw'
import { languageEnglish } from 'src/lang/en'
import type { NativeFileJobStatus } from '../storage/nativeFileJobs'
import {
    nativeFileJobPhaseLabel,
    nativeFileJobProgressText,
    nativeFileJobTitle,
} from './nativeFileJobProgress'

function status(patch: Partial<NativeFileJobStatus>): NativeFileJobStatus {
    return {
        jobId: 'job',
        kind: 'restore-block-risu-save',
        state: 'running',
        phase: 'queued',
        progress: { completedBytes: 0, completedItems: 0 },
        ...patch,
    } as NativeFileJobStatus
}

describe('nativeFileJobProgress', () => {
    it('maps every phase to localized copy instead of the internal code', () => {
        const phases: NativeFileJobStatus['phase'][] = [
            'queued', 'reading-source', 'awaiting-content-mapping', 'staging-database',
            'awaiting-activation', 'activating-database', 'writing-export', 'uploading-database',
            'awaiting-publication-retry', 'finalizing-publication', 'publishing-destination',
            'finalizing-export', 'complete',
        ]
        const localized = new Set([
            languageEnglish.risuNest.backup.progressPreparing,
            languageEnglish.risuNest.backup.progressTransferring,
            languageEnglish.risuNest.backup.progressFinalizing,
        ])

        for (const phase of phases) {
            const label = nativeFileJobPhaseLabel(status({ phase }))
            expect(localized.has(label)).toBe(true)
            expect(label).not.toContain(phase)
        }
        expect(nativeFileJobPhaseLabel(undefined)).toBe('')
    })

    it('adds a percentage when the job reports a total and megabytes otherwise', () => {
        expect(nativeFileJobProgressText(status({
            phase: 'reading-source',
            progress: { completedBytes: 512, totalBytes: 1024, completedItems: 0 },
        }))).toBe(`${languageEnglish.risuNest.backup.progressTransferring}: 50%`)
        expect(nativeFileJobProgressText(status({
            phase: 'writing-export',
            progress: { completedBytes: 2 * 1024 * 1024, completedItems: 0 },
        }))).toBe(`${languageEnglish.risuNest.backup.progressTransferring}: 2.0 MiB`)
        expect(nativeFileJobProgressText(status({ phase: 'queued' })))
            .toBe(languageEnglish.risuNest.backup.progressPreparing)
    })

    it('names local backup jobs as local backups rather than RisuSave', () => {
        expect(nativeFileJobTitle('import', status({ kind: 'restore-lossless-backup' })))
            .toBe(languageEnglish.loadBackupLocal)
        expect(nativeFileJobTitle('export', status({ kind: 'export-legacy-local-backup' })))
            .toBe(languageEnglish.saveBackupLocal)
        expect(nativeFileJobTitle('import', status({ kind: 'restore-block-risu-save' })))
            .toBe(languageEnglish.importRisuSave)
        expect(nativeFileJobTitle('export', undefined)).toBe(languageEnglish.exportRisuSave)
    })

    it('keeps the blocking overlay free of raw job phase codes', () => {
        expect(appSource).not.toContain('status?.phase ?? ')
        expect(appSource).toContain('nativeFileJobProgressText($nativeFileOperation.status)')
        expect(appSource).toContain('nativeFileJobTitle($nativeFileOperation.kind, $nativeFileOperation.status)')
    })
})

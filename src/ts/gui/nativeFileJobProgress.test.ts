import { describe, expect, it } from 'vitest'

import { readFileSync } from 'node:fs'
const appSource = readFileSync('src/App.svelte', 'utf8')
const dialogSource = readFileSync('src/lib/Others/NativeFileJobDialog.svelte', 'utf8')
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
            languageEnglish.risuNest.backup.progressReading,
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

    it('says an import is reading the file while an export is transferring it', () => {
        expect(nativeFileJobPhaseLabel(status({ kind: 'restore-legacy-local-backup', phase: 'reading-source' })))
            .toBe(languageEnglish.risuNest.backup.progressReading)
        expect(nativeFileJobPhaseLabel(status({ kind: 'export-legacy-local-backup', phase: 'writing-export' })))
            .toBe(languageEnglish.risuNest.backup.progressTransferring)
    })

    it('adds a percentage when the job reports a total and megabytes otherwise', () => {
        expect(nativeFileJobProgressText(status({
            phase: 'reading-source',
            progress: { completedBytes: 512, totalBytes: 1024, completedItems: 0 },
        }))).toBe(`${languageEnglish.risuNest.backup.progressReading}: 50%`)
        expect(nativeFileJobProgressText(status({
            phase: 'writing-export',
            progress: { completedBytes: 2 * 1024 * 1024, completedItems: 0 },
        }))).toBe(`${languageEnglish.risuNest.backup.progressTransferring}: 2.0 MiB`)
        expect(nativeFileJobProgressText(status({ phase: 'queued' })))
            .toBe(languageEnglish.risuNest.backup.progressPreparing)
    })

    it('names local backup jobs as local backups rather than RisuSave', () => {
        expect(
            nativeFileJobTitle(
                'import',
                status({ kind: 'restore-legacy-local-backup' }),
            ),
        ).toBe(languageEnglish.loadBackupLocal)
        expect(nativeFileJobTitle('export', status({ kind: 'export-legacy-local-backup' })))
            .toBe(languageEnglish.saveBackupLocal)
        expect(nativeFileJobTitle('import', status({ kind: 'restore-block-risu-save' })))
            .toBe(languageEnglish.importRisuSave)
        expect(nativeFileJobTitle('export', undefined)).toBe(languageEnglish.exportRisuSave)
    })

    it('mounts the shared dialog instead of an inline overlay and keeps raw job codes out of it', () => {
        expect(appSource).toContain('<NativeFileJobDialog />')
        expect(appSource).not.toContain('nativeFileJobProgressText(')
        expect(dialogSource).toContain('buildNativeFileJobDialogModel(')
        expect(dialogSource).not.toContain('.phase')
        expect(dialogSource).not.toContain('status?.')
    })
})

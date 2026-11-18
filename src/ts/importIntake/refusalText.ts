import { language } from 'src/lang'
import { failureReason } from '../gui/nativeFileJobDialogModel'
import { outcomeError } from '../storage/nativeFileJobManager'
import type { ImportIntakeRefusal } from './types'

/** The reason shown for a file the intake will not import. */
export function importIntakeRefusalText(refusal: ImportIntakeRefusal): string {
    switch (refusal.code) {
        case 'unsupported-format':
            return language.risuNest.importDialog.reasonUnsupportedFormat
        case 'plain-jpeg':
            return language.risuNest.importDialog.reasonPlainJpeg
        case 'png-without-card':
            return language.risuNest.importIntake.reasonPngWithoutCard
        case 'backup-with-other-files':
            return language.risuNest.importIntake.reasonBackupWithOtherFiles
        case 'plugin-api-version':
            return language.risuNest.plugins.unsupportedApiVersionInstall.replace('{version}', refusal.version)
        case 'read-failed': {
            const error = outcomeError('import', refusal.error)
            return failureReason(error.code, 'import', error.itemKind)
        }
    }
}

import { save } from './fileSelection'
import { language } from 'src/lang'
import { alertConfirm } from '../alert'
import { localFileStamp } from '../localFileStamp'

import { isTauri, isTauriAndroid, isTauriIOS } from '../platform'
import { runSharedNativeFileOperation } from './nativeFileJobManager'
import {
    NativeFileJobError,
    runNativeRawRecoveryExport,
    type NativeFileJobResult,
} from './nativeFileJobs'

export async function exportOriginalData(): Promise<NativeFileJobResult | null> {
    if (!isTauri) {
        throw new NativeFileJobError(
            'native-required',
            'Original data export requires the native app',
        )
    }
    return runSharedNativeFileOperation(
        'export',
        'raw-recovery-export',
        async ({ signal, onStatus }) => {
            if (signal.aborted)
                throw new DOMException('File operation cancelled', 'AbortError')
            const suggestedName = `risunest-${localFileStamp(Date.now())}.risunest-rescue.zip`
            const path = isTauriAndroid || isTauriIOS
                ? null
                : await save({
                      defaultPath: suggestedName,
                      filters: [{
                          name: 'RisuNest Rescue Archive',
                          extensions: ['risunest-rescue.zip'],
                      }],
                  })
            if (!isTauriAndroid && !isTauriIOS && !path) return null
            if (signal.aborted)
                throw new DOMException('File operation cancelled', 'AbortError')
            if (!await alertConfirm(language.risuNest.backup.originalDataAccountConfirm)) return null
            if (signal.aborted)
                throw new DOMException('File operation cancelled', 'AbortError')
            return runNativeRawRecoveryExport(
                isTauriIOS
                    ? { type: 'iosFiles', suggestedName }
                    : isTauriAndroid
                      ? { type: 'androidSaf', suggestedName }
                      : { type: 'desktopPath', path: path! },
                { signal, onStatus },
            )
        },
        { format: 'raw-recovery', presentation: 'dialog' },
    )
}

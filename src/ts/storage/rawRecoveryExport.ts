import { save } from '@tauri-apps/plugin-dialog'

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
            const suggestedName = `risunest-${new Date().toISOString().replace(/[:.]/g, '-')}.risunest-rescue.zip`
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

import { invoke } from '@tauri-apps/api/core'
import type { NativeFileJobSource } from '../storage/nativeFileJobs'
import type { ImportIntakeFormat } from './types'

/** What the native classifier found in one file. */
export interface NativeImportIntakeClassification {
    kind: 'character' | 'module' | 'lorebook' | 'preset' | 'backup' | 'script' | 'unsupported'
    format?: ImportIntakeFormat
    /** Unsupported files only. */
    refusal?: 'unsupported-format' | 'plain-jpeg' | 'png-without-card' | 'read-failed' | 'too-large'
    /** Present for `read-failed`. */
    message?: string
    bytes?: number
}

export interface NativeImportIntakeSource {
    source: NativeFileJobSource
    displayName: string
}

/** Classifies files by content where they are, in request order. A file that cannot be read is refused, not thrown. */
export function classifyNativeImportSources(
    sources: NativeImportIntakeSource[],
): Promise<NativeImportIntakeClassification[]> {
    return invoke<NativeImportIntakeClassification[]>('import_intake_classify', { request: { sources } })
}

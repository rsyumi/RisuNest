import type { NativeFileOperationImportLocation } from '../storage/nativeFileJobManager'

// Contract placeholder; the results worker replaces this module.

/** Goes to what an import added: the character, the module settings, the preset list or a character's lorebook. */
export function openImportResult(_location: NativeFileOperationImportLocation): void {
    throw new Error('Opening an import result is not wired yet')
}

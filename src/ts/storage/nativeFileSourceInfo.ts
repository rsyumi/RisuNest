import { stat } from '@tauri-apps/plugin-fs'

import type { NativeFileOperationSource } from './nativeFileJobManager'

/** Last path segment of a Windows or POSIX path, for showing which file an import reads. */
export function basenameOf(path: string): string {
    const segments = path.split(/[\\/]/).filter((segment) => segment.length > 0)
    return segments.at(-1) ?? path
}

/**
 * Describes a desktop file for the import dialog. The size is best effort: a
 * failed stat only drops the size, it never blocks the import.
 */
export async function describeDesktopSource(
    path: string,
    statFile: (path: string) => Promise<{ size: number }> = stat,
): Promise<NativeFileOperationSource> {
    const name = basenameOf(path)
    try {
        const { size } = await statFile(path)
        return Number.isFinite(size) && size >= 0 ? { name, bytes: size } : { name }
    }
    catch {
        return { name }
    }
}

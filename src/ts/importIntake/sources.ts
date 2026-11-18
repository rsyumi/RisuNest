import { invoke } from '@tauri-apps/api/core'
import { readFile, remove } from '@tauri-apps/plugin-fs'
import { language } from 'src/lang'
import { alertError } from '../alert'
import { failureReason } from '../gui/nativeFileJobDialogModel'
import { isTauriAndroid } from '../platform'
import { discardAndroidSafSource, type AndroidSpoolReady } from '../storage/androidSafBridge'
import type { NativeFileJobSource } from '../storage/nativeFileJobs'
import type { ImportIntakeSource } from './types'

/** Dropped files with these lowercased extensions are copied into the inbox and classified there. */
export const DROP_COPIED_EXTENSIONS: ReadonlySet<string> = new Set([
    'png', 'jpg', 'jpeg', 'charx', 'json', 'lorebook',
    'risum', 'risup', 'risupreset', 'risunest', 'risudat', 'bin',
])

/** Dropped plugin candidates, read as text and never copied. */
export const DROP_SCRIPT_EXTENSIONS: ReadonlySet<string> = new Set(['js', 'ts'])

export function lowercaseExtension(name: string): string {
    const separator = name.lastIndexOf('.')
    return separator < 0 ? '' : name.slice(separator + 1).toLowerCase()
}

// Android copies an opened file outside the spool allowlist to `<intent digest>-<index>-<name>`.
const ANDROID_OPENED_COPY_PREFIX = /^[0-9a-f]{64}-\d+-/

/** The name the user knows the file by. */
export function importSourceName(source: ImportIntakeSource): string {
    switch (source.type) {
        case 'file':
            return source.file.name
        case 'path': {
            const name = source.path.split(/[\\/]/).at(-1) || source.path
            return isTauriAndroid ? name.replace(ANDROID_OPENED_COPY_PREFIX, '') || name : name
        }
        case 'androidSpool':
            return source.spool.displayName
        case 'androidSpoolFailure':
            return source.failure.displayName
    }
}

/** Where native code reads the file, or null for a file that was never copied or could not be read. */
export function nativeImportSource(source: ImportIntakeSource): NativeFileJobSource | null {
    switch (source.type) {
        case 'file':
            return source.inboxPath ? { type: 'desktopPath', path: source.inboxPath } : null
        case 'path':
            return { type: 'desktopPath', path: source.path }
        case 'androidSpool':
            return { type: 'androidSpool', token: source.spool.token }
        case 'androidSpoolFailure':
            return null
    }
}

/**
 * What one delivery still holds: inbox copies, and Android spools no importer or restore has taken.
 * `release` removes them whichever way the delivery ended.
 */
export class ImportSourceLedger {
    private readonly copies: string[] = []
    private readonly spools = new Map<string, { spool: AndroidSpoolReady; refusedBackup: boolean }>()

    addCopy(path: string): void {
        this.copies.push(path)
    }

    addSpool(spool: AndroidSpoolReady): void {
        this.spools.set(spool.token, { spool, refusedBackup: false })
    }

    /** A backup spool refused because other files came with it; a failed release is reported. */
    markRefusedBackup(source: ImportIntakeSource): void {
        if (source.type !== 'androidSpool') return
        const entry = this.spools.get(source.spool.token)
        if (entry) entry.refusedBackup = true
    }

    /** Hands a spool to an importer or a restore, which releases or consumes it itself. */
    claim(source: ImportIntakeSource): void {
        if (source.type === 'androidSpool') this.spools.delete(source.spool.token)
    }

    async release(): Promise<void> {
        const copies = this.copies.splice(0)
        const spools = [...this.spools.values()]
        this.spools.clear()
        for (const path of copies) {
            await remove(path).catch(error => console.warn('Import source cleanup failed', error))
        }
        for (const { spool, refusedBackup } of spools) {
            const released = await discardAndroidSafSource(spool.token).catch(() => false)
            if (!released && refusedBackup) alertError(`${spool.displayName}: ${failureReason('discard-failed')}`)
        }
    }
}

/**
 * Reads a file's bytes for the TypeScript importers. An Android spool goes through the bounded
 * native reader and is released afterwards, as the content picker does.
 */
export async function readImportSourceBytes(
    source: ImportIntakeSource,
    ledger: ImportSourceLedger,
    signal: AbortSignal,
): Promise<Uint8Array> {
    signal.throwIfAborted()
    if (source.type === 'androidSpool') {
        ledger.claim(source)
        try {
            const response = await invoke<ArrayBuffer | number[]>('native_content_source_metadata', { token: source.spool.token })
            signal.throwIfAborted()
            return response instanceof ArrayBuffer ? new Uint8Array(response) : Uint8Array.from(response)
        } finally {
            await discardAndroidSafSource(source.spool.token).catch(() => false)
        }
    }
    const native = nativeImportSource(source)
    if (native?.type !== 'desktopPath') throw new Error(language.errors.noData)
    const data = await readFile(native.path)
    signal.throwIfAborted()
    return data
}

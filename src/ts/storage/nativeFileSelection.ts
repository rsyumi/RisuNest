import { invoke } from '@tauri-apps/api/core'
import '../androidNativeControl'
import { language } from 'src/lang'
import { open, SeekMode } from '@tauri-apps/plugin-fs'
import { recordNativeLogError } from '../nativeLog'
import { isTauriAndroid, isTauriIOS } from '../platform'
import { withLibraryFileSelection } from './libraryFileOperation'
import { nativeRoots } from './nativePaths'
import { ImageFormatError, imageFormatForName, imageFormatFromBytes } from '../media/imageFormats'

export interface NativeSelectedFile {
    path: string
    name: string
    bytes: number
    mimeType: string
}

export interface NativeFileSelectionBridge {
    pick(multiple: boolean): Promise<{ cancelled: boolean; files?: NativeSelectedFile[] }>
    discard(path: string): Promise<void>
}

export interface FileSelectionOptions {
    accept?: string
    multiple?: boolean
    signal?: AbortSignal
    image?: boolean
    onRejected?: (file: NativeSelectedFile, error: Error) => void
}

let selecting = false
const READ_CHUNK_BYTES = 256 * 1024
const READ_AHEAD_BYTES = 1024 * 1024

function bridge(): NativeFileSelectionBridge {
    if (isTauriAndroid) {
        if (!window.RisuNativeFileSelection) throw new Error('Android native file selection is unavailable')
        return window.RisuNativeFileSelection
    }
    if (isTauriIOS) return {
        pick: multiple => invoke('plugin:ios-native|pick_files', { multiple }),
        discard: path => invoke('plugin:ios-native|discard_file', { path }),
    }
    throw new Error('Native mobile file selection is unavailable')
}

function accepts(file: NativeSelectedFile, accept = '', image = false): boolean {
    const mime = file.mimeType.toLowerCase()
    if (image && !imageFormatForName(file.name)
        && (!mime || mime === 'application/octet-stream' || mime.startsWith('image/'))) return true
    const types = accept.toLowerCase().split(',').map(type => type.trim()).filter(Boolean)
    return !types.length || types.some(type => type === '*' || type === '*/*'
        || (type.startsWith('.') ? file.name.toLowerCase().endsWith(type)
            : type.endsWith('/*') ? file.mimeType.toLowerCase().startsWith(type.slice(0, -1))
                : file.mimeType.toLowerCase() === type))
}

// Staged copies are app-private and swept by both platforms later, so a failed cleanup is
// logged without failing a selection the user already completed.
function logCleanupFailure(error: unknown): void {
    void recordNativeLogError(`Selected file cleanup failed: ${String(error)}`).catch(() => {})
}

async function readSelected(file: NativeSelectedFile, signal?: AbortSignal, start = 0, end = file.bytes): Promise<Uint8Array<ArrayBuffer>> {
    signal?.throwIfAborted()
    const handle = await open(file.path, { read: true })
    try {
        if (start) await handle.seek(start, SeekMode.Start)
        const data = new Uint8Array(end - start)
        let position = 0
        while (position < data.length) {
            signal?.throwIfAborted()
            const count = await handle.read(data.subarray(position, Math.min(position + READ_CHUNK_BYTES, data.length)))
            if (!count) throw new Error('The selected file ended before all bytes were read')
            position += count
        }
        return data
    } finally {
        await handle.close().catch(logCleanupFailure)
    }
}

async function withNativeSelection<T>(options: FileSelectionOptions, consume: (files: NativeSelectedFile[], release: () => Promise<void>) => Promise<T>, cancelled: T): Promise<T> {
    return withLibraryFileSelection(async () => {
        if (options.signal?.aborted) return cancelled
        if (selecting) throw new Error('A file picker is already open')
        const native = bridge()
        selecting = true
        let files: NativeSelectedFile[] = []
        const release = async () => {
            const owned = files
            files = []
            for (const file of owned) await native.discard(file.path).catch(logCleanupFailure)
        }
        try {
            if (isTauriIOS) await nativeRoots()
            if (options.signal?.aborted) return cancelled
            const result = await native.pick(options.multiple ?? false)
            if (result.cancelled) return cancelled
            if (!Array.isArray(result.files) || !result.files.length) throw new Error('The file picker returned no document')
            files = result.files
            if (options.signal?.aborted) return cancelled
            const selected = options.multiple ? files : files.slice(0, 1)
            if (selected.some(file => !Number.isSafeInteger(file.bytes) || file.bytes < 0)) {
                throw new Error('The selected file has an invalid size')
            }
            const accepted = selected.filter(file => {
                if (accepts(file, options.accept, options.image)) return true
                const error = options.image ? new ImageFormatError('unsupported') : new Error(language.risuNest.importDialog.reasonUnsupportedFormat)
                if (!options.onRejected) throw error
                options.onRejected(file, error)
                return false
            })
            return await consume(accepted, release)
        } finally {
            try { await release() }
            finally { selecting = false }
        }
    })
}

export async function selectNativeFiles(options: FileSelectionOptions): Promise<File[]> {
    return (await selectNativeFileData(options)).map(file => new File([file.data], file.name, { type: file.mimeType }))
}

export function selectNativeFileData(options: FileSelectionOptions = {}) {
    return withNativeSelection(options, async files => {
        const selected: { name: string; data: Uint8Array<ArrayBuffer>; mimeType: string }[] = []
        for (const file of files) {
            try {
                if (options.image) {
                    const header = await readSelected(file, options.signal, 0, Math.min(file.bytes, 64 * 1024))
                    const firstBoxSize = header.length >= 8 ? new DataView(header.buffer, header.byteOffset, header.byteLength).getUint32(0) : 0
                    // AVIF may place its format box after another ISO BMFF box.
                    const mayHaveLaterFormat = header.length < file.bytes && (firstBoxSize === 1 || (firstBoxSize >= 8 && firstBoxSize <= file.bytes))
                    if (!imageFormatFromBytes(header) && !mayHaveLaterFormat) throw new ImageFormatError('unsupported')
                }
                selected.push({ name: file.name, data: await readSelected(file, options.signal), mimeType: file.mimeType })
            } catch (error) {
                options.signal?.throwIfAborted()
                if (!options.onRejected) throw error
                options.onRejected(file, error instanceof Error ? error : new Error(String(error)))
            }
        }
        return selected
    }, [])
}

export interface SelectedFileSlices {
    name: string
    size: number
    slice(start?: number, end?: number): { arrayBuffer(): Promise<ArrayBuffer> }
}

/** Keep the picked file's app copy until a native reader given its path settles, then remove it. */
export function withNativeSelectedPath<T>(options: Omit<FileSelectionOptions, 'multiple'>, consume: (file: NativeSelectedFile) => Promise<T>): Promise<T | null> {
    return withNativeSelection<T | null>({ ...options, multiple: false }, async ([file]) => file ? consume(file) : null, null)
}

/** Keep the source until the consumer settles, allowing early release before navigation. */
export function withNativeSelectedFile<T>(options: Omit<FileSelectionOptions, 'multiple'>, consume: (file: SelectedFileSlices, release: () => Promise<void>) => Promise<T>): Promise<T | null> {
    return withNativeSelection<T | null>({ ...options, multiple: false }, async ([file], discard) => {
        let active = true
        let cached: { start: number; data: Uint8Array<ArrayBuffer> } | undefined
        const offset = (value: number) => {
            const integer = Math.trunc(value) || 0
            return integer < 0 ? Math.max(file.bytes + integer, 0) : Math.min(integer, file.bytes)
        }
        try {
            return await consume({
                name: file.name,
                size: file.bytes,
                slice(start = 0, end = file.bytes) {
                    const from = offset(start)
                    const to = Math.max(from, offset(end))
                    return { async arrayBuffer() {
                        if (!active) throw new Error('The selected file has been released')
                        options.signal?.throwIfAborted()
                        if (from === to) return new ArrayBuffer(0)
                        if (to - from > READ_AHEAD_BYTES) return (await readSelected(file, options.signal, from, to)).buffer
                        if (!cached || from < cached.start || to > cached.start + cached.data.length) {
                            const data = await readSelected(file, options.signal, from, Math.min(file.bytes, from + READ_AHEAD_BYTES))
                            if (!active) throw new Error('The selected file has been released')
                            cached = { start: from, data }
                        }
                        return cached.data.slice(from - cached.start, to - cached.start).buffer
                    } }
                },
            }, async () => { active = false; cached = undefined; await discard() })
        } finally { active = false; cached = undefined }
    }, null)
}

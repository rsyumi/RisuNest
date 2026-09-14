import { invoke } from '@tauri-apps/api/core'
import { platform } from '@tauri-apps/plugin-os'
import type { AssetAlias, WorkingSetCommit } from './persistentDataStore'
import {
    getAndroidBinaryCommitBridge,
    type AndroidBinaryCommitBridge,
} from './androidBinaryCommitBridge'
import {
    ANDROID_LARGE_COMMIT_SIZE,
    MAX_ANDROID_COMMIT_BYTES,
    sendAndroidCommit,
} from './androidCommitTransport'

export interface CommitEnvelope {
    commit: WorkingSetCommit
    assetAliases: AssetAlias[]
}
export const LARGE_COMMIT_BYTES = 1024 * 1024
export const MAX_SHARED_COMMIT_BYTES = 64 * 1024 * 1024
const PROBE_VISITS = 4096

/** A bounded routing hint, not another full JSON serialization on the UI thread. */
export function isLargeCommit(value: unknown, threshold = LARGE_COMMIT_BYTES): boolean {
    let visits = 0
    let size = 0
    const pending = [value]
    while (pending.length) {
        if (++visits > PROBE_VISITS) return true
        const item = pending.pop()
        if (typeof item === 'string') size += item.length
        else if (item && typeof item === 'object') {
            if (Array.isArray(item) && item.length + visits > PROBE_VISITS) return true
            const keys = Object.keys(item)
            if (keys.length + visits > PROBE_VISITS) return true
            for (const key of keys) {
                size += key.length
                pending.push((item as Record<string, unknown>)[key])
            }
        } else size += 8
        if (size >= threshold) return true
    }
    return false
}

interface SharedEvent {
    additionalData?: { kind?: string; requestId?: string; id?: string }
    getBuffer(): ArrayBuffer
}
export interface SharedWebview {
    addEventListener(type: 'sharedbufferreceived', listener: (event: SharedEvent) => void): void
    removeEventListener(type: 'sharedbufferreceived', listener: (event: SharedEvent) => void): void
    releaseBuffer(buffer: ArrayBuffer): void
}
export interface CommitTransportDependencies {
    windows(): boolean
    android?(): boolean
    linux?(): boolean
    ios?(): boolean
    macos?(): boolean
    androidBinary?(): AndroidBinaryCommitBridge | null
    invoke<T>(
        command: string,
        args?: Record<string, unknown> | Uint8Array,
    ): Promise<T>
    encode(input: CommitEnvelope): Promise<Uint8Array>
    shared(): SharedWebview | undefined
}

export class NativeCommitTransport {
    private pending: Promise<unknown> = Promise.resolve()
    constructor(private readonly dependencies: CommitTransportDependencies) {}

    commit(input: CommitEnvelope): Promise<{ revision: number }> {
        const run = this.pending.then(() => this.send(input))
        this.pending = run.catch(() => undefined)
        return run
    }

    private async send(input: CommitEnvelope): Promise<{ revision: number }> {
        const deps = this.dependencies
        const android = deps.android?.() ?? false
        const rawPlatform =
            (deps.linux?.() ?? false) || (deps.macos?.() ?? false) || (deps.ios?.() ?? false)
        if (
            (!android && !rawPlatform && !deps.windows()) ||
            !isLargeCommit(
                input,
                android ? ANDROID_LARGE_COMMIT_SIZE : LARGE_COMMIT_BYTES,
            )
        )
            return deps.invoke('pds_commit', { ...input })
        let bytes: Uint8Array
        try {
            bytes = await deps.encode(input)
        } catch (error) {
            // Callable hooks cannot be structured-cloned. No native save has started yet.
            if (error instanceof DOMException && error.name === 'DataCloneError') {
                return deps.invoke('pds_commit', { ...input })
            }
            throw error
        }
        if (android) {
            // Keep the existing large-save contract beyond the bounded assembly budget.
            if (bytes.byteLength > MAX_ANDROID_COMMIT_BYTES)
                return deps.invoke('pds_commit', { ...input })
            return sendAndroidCommit(
                bytes,
                deps.invoke,
                deps.androidBinary ? deps.androidBinary() : getAndroidBinaryCommitBridge(),
            )
        }
        if (rawPlatform) return deps.invoke('pds_commit_raw', bytes)
        const webview = deps.shared()
        if (!webview || bytes.byteLength > MAX_SHARED_COMMIT_BYTES)
            return deps.invoke('pds_commit_raw', bytes)
        const requestId = crypto.randomUUID()
        let buffer: ArrayBuffer | undefined
        let nativeId: string | undefined
        let receivedId: string | undefined
        let receive!: () => void
        const received = new Promise<void>((resolve) => {
            receive = resolve
        })
        const listener = (event: SharedEvent) => {
            if (
                event.additionalData?.kind !== 'pds-commit' ||
                event.additionalData.requestId !== requestId
            )
                return
            if (buffer) return
            receivedId = event.additionalData.id
            buffer = event.getBuffer()
            receive()
        }
        webview.addEventListener('sharedbufferreceived', listener)
        let timer: ReturnType<typeof setTimeout> | undefined
        try {
            const opened = await deps.invoke<{ id: string; capacity: number } | null>(
                'pds_commit_shared_open',
                { requestId, totalBytes: bytes.byteLength },
            )
            if (!opened) return deps.invoke('pds_commit_raw', bytes)
            nativeId = opened.id
            await Promise.race([
                received,
                new Promise<never>((_, reject) => {
                    timer = setTimeout(
                        () => reject(new Error('Timed out receiving persistence shared buffer')),
                        10_000,
                    )
                }),
            ])
            if (
                receivedId !== nativeId ||
                !buffer ||
                !Number.isSafeInteger(opened.capacity) ||
                opened.capacity <= 0 ||
                opened.capacity !== buffer.byteLength
            ) {
                throw new Error('Invalid persistence shared buffer')
            }
            const shared = new Uint8Array(buffer)
            for (let offset = 0; offset < bytes.byteLength;) {
                const length = Math.min(shared.length, bytes.byteLength - offset)
                shared.set(bytes.subarray(offset, offset + length))
                const ack = await deps.invoke<number>('pds_commit_shared_chunk', {
                    id: nativeId,
                    offset,
                    length,
                })
                if (ack !== offset + length)
                    throw new Error('Invalid persistence chunk acknowledgement')
                offset = ack
            }
            // From this point an error is potentially a committed save. Never replay it.
            return await deps.invoke('pds_commit_shared_finish', { id: nativeId })
        } finally {
            if (timer !== undefined) clearTimeout(timer)
            webview.removeEventListener('sharedbufferreceived', listener)
            try {
                if (buffer) webview.releaseBuffer(buffer)
            } finally {
                if (nativeId)
                    await deps
                        .invoke('pds_commit_shared_cancel', { id: nativeId })
                        .catch(() => undefined)
            }
        }
    }
}

let encoder: Worker | undefined
let encoderIdle: ReturnType<typeof setTimeout> | undefined
export function encodeNativeCommit(input: CommitEnvelope): Promise<Uint8Array> {
    if (encoderIdle) clearTimeout(encoderIdle)
    encoder ??= new Worker(new URL('./nativeCommitEncoder.worker.ts', import.meta.url), {
        type: 'module',
    })
    const worker = encoder
    return new Promise((resolve, reject) => {
        const cleanup = () => {
            worker.onmessage = null
            worker.onerror = null
            encoderIdle = setTimeout(() => {
                worker.terminate()
                if (encoder === worker) encoder = undefined
            }, 30_000)
        }
        worker.onmessage = ({ data }: MessageEvent<{ bytes?: Uint8Array; error?: string }>) => {
            cleanup()
            if (data.bytes) resolve(data.bytes)
            else reject(new Error(data.error ?? 'Persistence encoding failed'))
        }
        worker.onerror = () => {
            cleanup()
            worker.terminate()
            encoder = undefined
            reject(new Error('Persistence encoder failed'))
        }
        try {
            worker.postMessage(input)
        } catch (error) {
            cleanup()
            reject(error)
        }
    })
}

export const nativeCommitTransport = new NativeCommitTransport({
    ios: () =>
        Boolean(
            (window as Window & { __TAURI_INTERNALS__?: unknown })
                .__TAURI_INTERNALS__,
        ) && platform() === 'ios',
    macos: () =>
        Boolean(
            (window as Window & { __TAURI_INTERNALS__?: unknown })
                .__TAURI_INTERNALS__,
        ) && platform() === 'macos',
    linux: () =>
        Boolean(
            (window as Window & { __TAURI_INTERNALS__?: unknown })
                .__TAURI_INTERNALS__,
        ) && platform() === 'linux',
    android: () =>
        Boolean(
            (window as Window & { __TAURI_INTERNALS__?: unknown })
                .__TAURI_INTERNALS__,
        ) && platform() === 'android',
    windows: () =>
        Boolean(
            (window as Window & { __TAURI_INTERNALS__?: unknown })
                .__TAURI_INTERNALS__,
        ) && platform() === 'windows',
    invoke,
    encode: encodeNativeCommit,
    shared: () =>
        (window as Window & { chrome?: { webview?: SharedWebview } }).chrome
            ?.webview,
})

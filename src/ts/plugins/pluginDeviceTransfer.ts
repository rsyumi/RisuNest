import { invoke } from '@tauri-apps/api/core'
import { isTauriAndroid } from '../platform'
import { ANDROID_LARGE_COMMIT_SIZE, sendAndroidCommit } from '../storage/androidCommitTransport'
import { nativeAssetByteSender } from '../storage/nativeAssetByteTransport'
import type { PluginDeviceMutation, PluginDeviceSpace, PluginDeviceObservation } from './pluginDeviceKeyspace'
import { structureParts, type StructuredLocalPluginValue } from './localPluginStorageValue'

export const PLUGIN_DEVICE_VALUE_BYTES = 512 * 1024 * 1024
export const PLUGIN_DEVICE_PART_BYTES = 8 * 1024 * 1024
const REQUEST_BYTES = 64 * 1024 * 1024
const encoder = new TextEncoder()

function characterSize(text: string, index: number): { units: number; bytes: number; encoded: number } {
    const code = text.charCodeAt(index)
    if (code < 0x80) {
        return { units: 1, bytes: 1, encoded: code === 34 || code === 92 || code === 8 || code === 9 || code === 10 || code === 12 || code === 13 ? 2 : code < 32 ? 6 : 1 }
    }
    if (code < 0x800) return { units: 1, bytes: 2, encoded: 2 }
    if (code >= 0xd800 && code <= 0xdbff) {
        const low = text.charCodeAt(index + 1)
        if (!(low >= 0xdc00 && low <= 0xdfff)) throw new Error('Plugin device value contains an unpaired surrogate')
        return { units: 2, bytes: 4, encoded: 4 }
    }
    if (code >= 0xdc00 && code <= 0xdfff) throw new Error('Plugin device value contains an unpaired surrogate')
    return { units: 1, bytes: 3, encoded: 3 }
}

export function pluginDeviceValueSize(value: string, limit = PLUGIN_DEVICE_VALUE_BYTES): number {
    let size = 2
    for (let index = 0; index < value.length;) {
        const character = characterSize(value, index)
        size += character.encoded
        if (size > limit) throw new Error('Plugin device value is too large')
        index += character.units
    }
    if (size > limit) throw new Error('Plugin device value is too large')
    return size
}

export function* pluginDeviceParts(value: string, limit = PLUGIN_DEVICE_PART_BYTES): Generator<{ part: string; byteLength: number }> {
    let start = 0, index = 0, encoded = 2, byteLength = 0
    while (index < value.length) {
        const character = characterSize(value, index)
        if (character.encoded + 2 > limit) throw new Error('Plugin device part is too small')
        if (encoded + character.encoded > limit) {
            yield { part: value.slice(start, index), byteLength }
            start = index; encoded = 2; byteLength = 0
        }
        encoded += character.encoded; byteLength += character.bytes; index += character.units
    }
    if (index > start) yield { part: value.slice(start, index), byteLength }
}

type NativeMutation = PluginDeviceMutation | { type: 'staged'; sessionId: string }
function requestSize(owner: string, mutations: readonly NativeMutation[], observations: readonly PluginDeviceObservation[]): number {
    const empty = mutations.map((mutation) => mutation.type === 'set' ? { ...mutation, value: '' } : mutation)
    return encoder.encode(JSON.stringify({ owner, mutations: empty, observations })).length
        + mutations.reduce((size, mutation) => size + (mutation.type === 'set' ? pluginDeviceValueSize(mutation.value) - 2 : 0), 0)
}

async function send<T>(command: string, request: Record<string, unknown>, bytes: number, finish: string): Promise<T> {
    if (isTauriAndroid && bytes > ANDROID_LARGE_COMMIT_SIZE) {
        return await sendAndroidCommit<T>(encoder.encode(JSON.stringify(request)), invoke, undefined, finish)
    }
    return await invoke<T>(command, request)
}

/** Runs at most `limit` streams at once, the rest in the order they asked. */
function streamLimit(limit: number) {
    const waiters: (() => void)[] = []
    let open = 0
    return async <T>(stream: () => Promise<T>): Promise<T> => {
        while (open >= limit) await new Promise<void>(resolve => waiters.push(resolve))
        open++
        try {
            return await stream()
        } finally {
            open--
            waiters.shift()?.()
        }
    }
}

/** Runs `run` after every earlier run of the same owner settled. */
function inOwnerOrder<T>(queues: Map<string, Promise<void>>, owner: string, run: () => Promise<T>): Promise<T> {
    const result = (queues.get(owner) ?? Promise.resolve()).then(run)
    const settled = result.then(() => undefined, () => undefined)
    queues.set(owner, settled)
    void settled.then(() => { if (queues.get(owner) === settled) queues.delete(owner) })
    return result
}

// The native side keeps at most two write sessions open per plugin and eight
// in all. Staging for one write per plugin and four in all leaves room for
// sessions that a reloaded page left open until they expire.
const ownerWrites = new Map<string, Promise<void>>()
const withWriteStream = streamLimit(4)

export async function writeNativePluginDeviceValues(owner: string, mutations: readonly PluginDeviceMutation[], observations: readonly PluginDeviceObservation[] = []): Promise<PluginDeviceObservation[]> {
    if (mutations.length === 0) return []
    if (mutations.length > 1024) throw new Error('Plugin device write batch is too large')
    const sizes = mutations.map((mutation) => mutation.type === 'set' ? pluginDeviceValueSize(mutation.value) : 0)
    const planned = mutations.map((mutation, index): NativeMutation => sizes[index] > PLUGIN_DEVICE_PART_BYTES
        ? { type: 'staged', sessionId: '00000000-0000-0000-0000-000000000000' } : mutation)
    if (requestSize(owner, planned, observations) > REQUEST_BYTES) throw new Error('Plugin device write batch is too large')
    const commit = async () => await send<PluginDeviceObservation[]>('pds_write_plugin_device_values', { owner, mutations: planned, observations }, requestSize(owner, planned, observations), 'pds_plugin_write_android_finish') ?? []
    if (!planned.some((mutation) => mutation.type === 'staged')) return await commit()
    return await inOwnerOrder(ownerWrites, owner, () => withWriteStream(async () => {
        const sessions: string[] = []
        try {
            for (let index = 0; index < mutations.length; index++) {
                const mutation = mutations[index]
                if (mutation.type !== 'set' || sizes[index] <= PLUGIN_DEVICE_PART_BYTES) continue
                const { sessionId } = await invoke<{ sessionId: string }>('pds_open_plugin_device_write', {
                    owner, space: mutation.space, key: mutation.key, encodedLength: sizes[index],
                })
                sessions.push(sessionId)
                let offset = 0
                for (const { part, byteLength } of pluginDeviceParts(mutation.value)) {
                    const request = { owner, sessionId, offset, part }
                    const size = pluginDeviceValueSize(part) - 2 + encoder.encode(JSON.stringify({ ...request, part: '' })).length
                    const next = await send<number>('pds_append_plugin_device_write', request, size, 'pds_plugin_part_android_finish')
                    if (next !== offset + byteLength) throw new Error('Invalid plugin device part acknowledgement')
                    offset = next
                }
                planned[index] = { type: 'staged', sessionId }
            }
            return await commit()
        } finally {
            await Promise.all(sessions.map((sessionId) => invoke('pds_abort_plugin_device_write', { owner, sessionId }).catch(() => undefined)))
        }
    }))
}

type ReadStart = string | { byteSize: number; version: string } | null
type ReadRange = { type: 'changed' } | { type: 'value'; value: string; nextOffset: number }

export async function readNativePluginDeviceValue(owner: string, space: PluginDeviceSpace, key: string): Promise<string | null> {
    for (let attempt = 0; attempt < 3; attempt++) {
        const opened = await invoke<ReadStart>('pds_read_plugin_device_value', { owner, space, key })
        if (opened == null) return null
        if (typeof opened === 'string') return opened
        const parts: string[] = []
        let offset = 0, changed = false
        while (offset < opened.byteSize) {
            const range = await invoke<ReadRange>('pds_read_plugin_device_range', { owner, space, key, version: opened.version, offset })
            if (range.type === 'changed') { changed = true; break }
            if (range.nextOffset <= offset || range.nextOffset > opened.byteSize) throw new Error('Invalid plugin device read range')
            parts.push(range.value)
            offset = range.nextOffset
        }
        if (!changed) return parts.join('')
    }
    throw new Error('Plugin device value changed during read')
}

// The native side keeps at most two part uploads open per plugin and eight in
// all. One per plugin and four in all leave room for uploads that a reloaded
// page left open until they expire.
const ownerStreams = new Map<string, Promise<void>>()
const withPartStream = streamLimit(4)

/**
 * Stores every part of a structure-form value as a device object and returns
 * their hashes in order. The row that names them is committed afterwards.
 */
export function storeNativePluginDeviceParts(owner: string, value: StructuredLocalPluginValue): Promise<string[]> {
    if (value.byteLength === 0) return Promise.resolve([])
    return inOwnerOrder(ownerStreams, owner, () => withPartStream(() => storeParts(owner, value)))
}

async function storeParts(owner: string, value: StructuredLocalPluginValue): Promise<string[]> {
    const hashes: string[] = []
    const sender = await nativeAssetByteSender(invoke)
    for await (const part of structureParts(value)) {
        const uploadId = await invoke<string>('pds_open_plugin_device_part', { owner, byteLength: part.byteLength })
        try {
            for (let offset = 0; offset < part.byteLength;) {
                const piece = part.subarray(offset, offset + sender.capacity)
                const next = await sender.append('pds_append_plugin_device_part', uploadId, offset, piece)
                if (next !== offset + piece.byteLength) throw new Error('Invalid plugin device part acknowledgement')
                offset = next
            }
            hashes.push(await invoke<string>('pds_finish_plugin_device_part', { owner, uploadId }))
        } catch (error) {
            await invoke('pds_abort_plugin_device_part', { owner, uploadId }).catch(() => undefined)
            throw error
        }
    }
    return hashes
}

const PART_READ_ATTEMPTS = 3
const PART_READ_RETRY_DELAY_MS = 100

// The native side reports a damaged or missing part as a validation error. A
// busy store answers with a store error, and a failed IPC call with no store
// error at all; only those two are worth another attempt.
function isTransientPartReadError(error: unknown): boolean {
    let value = error
    if (typeof value === 'string') {
        try { value = JSON.parse(value) } catch { return true }
    }
    const code = value && typeof value === 'object' ? (value as { code?: unknown }).code : undefined
    return code === undefined || code === 'store-error'
}

export async function readNativePluginDevicePart(hash: string, byteLength: number): Promise<Uint8Array> {
    const part = new Uint8Array(byteLength)
    for (let offset = 0; offset < byteLength;) {
        let response: unknown
        for (let attempt = 1; ; attempt++) {
            try {
                response = await invoke<unknown>('pds_read_plugin_device_part', { hash, offset })
                break
            } catch (error) {
                if (attempt === PART_READ_ATTEMPTS || !isTransientPartReadError(error)) throw error
                await new Promise(resolve => setTimeout(resolve, PART_READ_RETRY_DELAY_MS))
            }
        }
        const piece = response instanceof ArrayBuffer ? new Uint8Array(response)
            : response instanceof Uint8Array ? response
            : Array.isArray(response) ? Uint8Array.from(response as number[]) : null
        if (!piece || piece.byteLength === 0 || piece.byteLength > byteLength - offset) throw new Error('Invalid plugin device part read')
        part.set(piece, offset)
        offset += piece.byteLength
    }
    return part
}

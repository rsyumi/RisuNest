import {
    ANDROID_BINARY_CHUNK_BYTES,
    binaryCommitSender,
    getAndroidBinaryCommitBridge,
    type AndroidBinaryCommitBridge,
} from './androidBinaryCommitBridge'

export const ANDROID_COMMIT_CHUNK_BYTES = 32 * 1024
export const MAX_ANDROID_COMMIT_BYTES = 64 * 1024 * 1024
// Routing is a bounded UTF-16/field-count hint, not an exact byte measurement.
export const ANDROID_LARGE_COMMIT_SIZE = 128 * 1024

type Invoke = <T>(command: string, args?: Record<string, unknown>) => Promise<T>

/** Use native binary messages when registered, otherwise bounded UTF-8 strings.
 * Android's ordinary invoke serializes typed arrays as JSON number arrays.
 */
export async function sendAndroidCommit(
    bytes: Uint8Array,
    invoke: Invoke,
    bridge: AndroidBinaryCommitBridge | null | undefined = getAndroidBinaryCommitBridge(),
): Promise<{ revision: number }> {
    if (!bytes.length || bytes.length > MAX_ANDROID_COMMIT_BYTES)
        throw new Error('Invalid Android persistence payload size')
    const id = crypto.randomUUID()
    const decoder = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true })
    const sender = bridge ? binaryCommitSender(bridge, id) : undefined
    const expectedCapacity = sender ? ANDROID_BINARY_CHUNK_BYTES : ANDROID_COMMIT_CHUNK_BYTES
    try {
        const { capacity } = await invoke<{ capacity: number }>('pds_commit_android_open', {
            id,
            totalBytes: bytes.length,
            binary: Boolean(sender),
        })
        if (capacity !== expectedCapacity)
            throw new Error('Invalid Android persistence chunk capacity')
        for (let offset = 0; offset < bytes.length; ) {
            let end = Math.min(offset + capacity, bytes.length)
            // Never replace a code point split across calls with U+FFFD.
            while (!sender && end < bytes.length && (bytes[end] & 0xc0) === 0x80) end--
            if (end <= offset) throw new Error('Invalid Android persistence UTF-8 boundary')
            const part = bytes.subarray(offset, end)
            const ack = sender
                ? await sender.append(offset, part)
                : await invoke<number>('pds_commit_android_chunk', {
                      id,
                      offset,
                      chunk: decoder.decode(part),
                  })
            if (ack !== end) throw new Error('Invalid Android persistence chunk acknowledgement')
            offset = end
        }
        // An error after finish may have committed. Never fall back or replay here.
        return await invoke('pds_commit_android_finish', { id })
    } finally {
        sender?.close()
        // Client-owned ID also cleans up an open whose response was lost.
        await invoke('pds_commit_android_cancel', { id }).catch(() => undefined)
    }
}

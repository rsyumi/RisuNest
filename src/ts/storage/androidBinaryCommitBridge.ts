export const ANDROID_BINARY_CHUNK_BYTES = 256 * 1024
const HEADER_BYTES = 40

export interface AndroidBinaryCommitBridge {
    postMessage(packet: ArrayBuffer): void
    onmessage: ((event: { data: string }) => void) | null
}

/** Native registers this object only when both WebView features are supported. */
export function getAndroidBinaryCommitBridge(): AndroidBinaryCommitBridge | undefined {
    const bridge = (window as Window & { RisuNestCommit?: AndroidBinaryCommitBridge })
        .RisuNestCommit
    return typeof bridge?.postMessage === 'function' ? bridge : undefined
}

export function binaryCommitSender(bridge: AndroidBinaryCommitBridge, id: string) {
    let pending: { resolve(offset: number): void; reject(error: Error): void } | undefined
    let timer: ReturnType<typeof setTimeout> | undefined
    const listener = ({ data }: { data: string }) => {
        if (!pending) return
        try {
            const result = JSON.parse(data)
            if (result.id !== id) return
            if (result.error) throw new Error('Android binary persistence chunk rejected')
            if (!Number.isSafeInteger(result.offset))
                throw new Error('Invalid binary acknowledgement')
            pending.resolve(result.offset)
        } catch (error) {
            pending.reject(
                error instanceof Error ? error : new Error('Invalid binary acknowledgement'),
            )
        }
    }
    bridge.onmessage = listener
    return {
        async append(offset: number, bytes: Uint8Array): Promise<number> {
            if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/.test(id))
                throw new Error('Invalid binary persistence ID')
            const packet = new Uint8Array(HEADER_BYTES + bytes.length)
            for (let i = 0; i < id.length; i++) packet[i] = id.charCodeAt(i)
            new DataView(packet.buffer).setUint32(36, offset, true)
            packet.set(bytes, HEADER_BYTES)
            try {
                return await new Promise<number>((resolve, reject) => {
                    pending = { resolve, reject }
                    timer = setTimeout(
                        () => reject(new Error('Android binary persistence timed out')),
                        10_000,
                    )
                    bridge.postMessage(packet.buffer)
                })
            } finally {
                clearTimeout(timer)
                pending = undefined
            }
        },
        close() {
            clearTimeout(timer)
            pending = undefined
            if (bridge.onmessage === listener) bridge.onmessage = null
        },
    }
}

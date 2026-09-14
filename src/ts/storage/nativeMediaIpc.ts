export const NATIVE_MEDIA_IPC_CHUNK_BYTES = 64 * 1024
export const MAX_NATIVE_MEDIA_TRANSFER_BYTES = 512 * 1024 * 1024

type InvokeCommand = (command: string, args?: Record<string, unknown>) => Promise<unknown>

interface BoundedInputCommand {
    data: Uint8Array
    directCommand: string
    streamedFinishCommand: string
    args: Record<string, unknown>
}

function safeSize(value: unknown, context: string): number {
    if (!Number.isSafeInteger(value) || (value as number) < 0) {
        throw new TypeError(`${context} returned an invalid byte count`)
    }
    return value as number
}

function byteArray(value: unknown, context: string): Uint8Array {
    if (!Array.isArray(value)) throw new TypeError(`${context} returned invalid bytes`)
    if (value.length > NATIVE_MEDIA_IPC_CHUNK_BYTES) {
        throw new TypeError(`${context} exceeded one IPC chunk`)
    }
    const bytes = new Uint8Array(value.length)
    for (let index = 0; index < value.length; index += 1) {
        const byte = value[index]
        if (!Number.isInteger(byte) || byte < 0 || byte > 255) {
            throw new TypeError(`${context} returned invalid bytes`)
        }
        bytes[index] = byte
    }
    return bytes
}

async function preserveCleanupFailure<T>(
    operation: () => Promise<T>,
    cleanup: () => Promise<unknown>,
    message: string,
): Promise<T> {
    let completed = false
    let value: T | undefined
    let primaryError: unknown
    try {
        value = await operation()
        completed = true
    } catch (error) {
        primaryError = error
    }
    let cleanupError: unknown
    let cleanupFailed = false
    try {
        await cleanup()
    } catch (error) {
        cleanupFailed = true
        cleanupError = error
    }
    if (cleanupFailed) {
        console.error(message, cleanupError)
    }
    if (!completed) throw primaryError
    return value as T
}

export async function invokeWithBoundedNativeMediaInput<T>(
    invokeCommand: InvokeCommand,
    command: BoundedInputCommand,
): Promise<T> {
    if (command.data.byteLength > MAX_NATIVE_MEDIA_TRANSFER_BYTES) {
        throw new RangeError('Native media input exceeds the maximum transfer size')
    }
    const data = command.data.slice()
    if (data.byteLength <= NATIVE_MEDIA_IPC_CHUNK_BYTES) {
        return await invokeCommand(command.directCommand, {
            ...command.args,
            data: Array.from(data),
        }) as T
    }

    const uploadId = crypto.randomUUID()
    return preserveCleanupFailure(async () => {
        const opened = await invokeCommand('native_media_inlay_input_open', {
            uploadId,
            totalBytes: data.byteLength,
        }) as { capacity?: unknown }
        if (opened?.capacity !== NATIVE_MEDIA_IPC_CHUNK_BYTES) {
            throw new TypeError('Native media input upload returned an invalid chunk capacity')
        }
        for (let offset = 0; offset < data.byteLength;) {
            const end = Math.min(offset + NATIVE_MEDIA_IPC_CHUNK_BYTES, data.byteLength)
            const acknowledged = safeSize(await invokeCommand('native_media_inlay_input_chunk', {
                uploadId,
                offset,
                data: Array.from(data.subarray(offset, end)),
            }), 'Native media input upload')
            if (acknowledged !== end) {
                throw new Error('Native media input upload returned an invalid acknowledgement')
            }
            offset = end
        }
        return await invokeCommand(command.streamedFinishCommand, {
            ...command.args,
            uploadId,
        }) as T
    }, () => invokeCommand('native_media_inlay_input_cancel', { uploadId }),
    'Native media input transfer cleanup failed')
}

export async function consumeBoundedNativeMediaOutput<T>(
    invokeCommand: InvokeCommand,
    result: unknown,
    validateMetadata: (value: unknown) => T,
): Promise<{ data: Uint8Array, metadata: T }> {
    if (typeof result !== 'object' || result === null) {
        throw new TypeError('Native Inlay encoder returned an invalid response')
    }
    const envelope = result as {
        data?: unknown
        outputId?: unknown
        outputSize?: unknown
        metadata?: unknown
    }
    const cleanupId = typeof envelope.outputId === 'string' && envelope.outputId.length > 0
        ? envelope.outputId
        : null
    const consume = async () => {
        const outputSize = safeSize(envelope.outputSize, 'Native Inlay encoder')
        if (outputSize > MAX_NATIVE_MEDIA_TRANSFER_BYTES) {
            throw new RangeError('Native Inlay encoder output exceeds the maximum transfer size')
        }
        const metadata = validateMetadata(envelope.metadata)
        if (envelope.data !== null && envelope.data !== undefined) {
            if (envelope.outputId !== null && envelope.outputId !== undefined) {
                throw new TypeError('Native Inlay encoder returned ambiguous output')
            }
            const data = byteArray(envelope.data, 'Native Inlay encoder')
            if (data.byteLength > NATIVE_MEDIA_IPC_CHUNK_BYTES || data.byteLength !== outputSize) {
                throw new TypeError('Native Inlay encoder returned an invalid inline output size')
            }
            return { data, metadata }
        }
        if (!cleanupId || outputSize <= NATIVE_MEDIA_IPC_CHUNK_BYTES) {
            throw new TypeError('Native Inlay encoder returned an invalid streamed output')
        }
        const data = new Uint8Array(outputSize)
        for (let offset = 0; offset < outputSize;) {
            const end = Math.min(offset + NATIVE_MEDIA_IPC_CHUNK_BYTES, outputSize)
            const chunk = byteArray(await invokeCommand('native_media_inlay_output_read', {
                outputId: cleanupId,
                start: offset,
                endExclusive: end,
            }), 'Native media output read')
            if (chunk.byteLength !== end - offset) {
                throw new Error('Native media output read returned an invalid chunk length')
            }
            data.set(chunk, offset)
            offset = end
        }
        return { data, metadata }
    }
    if (!cleanupId) return consume()
    return preserveCleanupFailure(
        consume,
        () => invokeCommand('native_media_inlay_output_cancel', { outputId: cleanupId }),
        'Native media output transfer cleanup failed',
    )
}

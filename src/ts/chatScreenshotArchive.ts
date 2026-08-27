import { Zip, ZipPassThrough } from 'fflate'

export interface ScreenshotArchiveWriter {
    write(chunk: Uint8Array): Promise<void>
    close(): Promise<void>
    abort(): Promise<void | boolean>
}

export interface StreamingScreenshotArchive {
    addPage(pageNumber: number, page: Blob): Promise<void>
    close(signal?: AbortSignal): Promise<void | boolean>
    abort(): Promise<void>
}

function cancellationError() {
    return new DOMException('Screenshot capture was cancelled', 'AbortError')
}

async function waitForClose(promise: Promise<void>, signal?: AbortSignal) {
    if (!signal) return promise
    if (signal.aborted) throw cancellationError()
    await new Promise<void>((resolve, reject) => {
        const abort = () => reject(cancellationError())
        signal.addEventListener('abort', abort, { once: true })
        promise.then(resolve, reject).finally(() => signal.removeEventListener('abort', abort))
    })
}

export function createStreamingScreenshotArchive(
    writer: ScreenshotArchiveWriter,
): StreamingScreenshotArchive {
    let state: 'open' | 'closed' | 'aborted' = 'open'
    let writeChain = Promise.resolve()
    let streamError: unknown
    let resolveFinal!: () => void
    let rejectFinal!: (error: unknown) => void
    const finalChunk = new Promise<void>((resolve, reject) => {
        resolveFinal = resolve
        rejectFinal = reject
    })
    const zip = new Zip((error, chunk, final) => {
        if (error) {
            streamError = error
            rejectFinal(error)
            return
        }
        if (chunk.length > 0) {
            writeChain = writeChain.then(() => writer.write(chunk))
        }
        if (final) resolveFinal()
    })

    async function abortOnce(): Promise<boolean> {
        if (state === 'aborted') return true
        if (state === 'closed') return false
        zip.terminate()
        let aborted: void | boolean
        try {
            aborted = await writer.abort()
        }
        catch (error) {
            state = 'aborted'
            throw error
        }
        if (aborted === false) {
            state = 'closed'
            return false
        }
        state = 'aborted'
        return true
    }

    async function fail(error: unknown): Promise<boolean> {
        if (!await abortOnce()) return true
        throw error
    }

    return {
        async addPage(pageNumber, page) {
            if (state !== 'open') throw new Error('Screenshot archive is finalized')
            if (!Number.isSafeInteger(pageNumber) || pageNumber < 1) {
                throw new Error('Screenshot page number must be a positive integer')
            }
            try {
                const entry = new ZipPassThrough(`page-${pageNumber.toString().padStart(4, '0')}.png`)
                zip.add(entry)
                const reader = page.stream().getReader()
                try {
                    while (true) {
                        const { done, value } = await reader.read()
                        if (done) break
                        entry.push(value, false)
                        await writeChain
                        if (streamError) throw streamError
                    }
                    entry.push(new Uint8Array(0), true)
                    await writeChain
                } finally {
                    reader.releaseLock()
                }
                if (streamError) throw streamError
            } catch (error) {
                await fail(error)
            }
        },

        async close(signal) {
            if (state === 'aborted') throw new Error('Screenshot archive was aborted')
            if (state === 'closed') return
            try {
                if (signal?.aborted) throw cancellationError()
                zip.end()
                await waitForClose(finalChunk, signal)
                await waitForClose(writeChain, signal)
                if (streamError) throw streamError
                await waitForClose(writer.close(), signal)
                if (signal?.aborted) throw cancellationError()
                state = 'closed'
            } catch (error) {
                return fail(error)
            }
        },

        async abort() {
            await abortOnce()
        },
    }
}

import { Zip, ZipPassThrough } from 'fflate'

export const SCREENSHOT_ARCHIVE_ZIP32_SENTINEL = 0xffff_ffff
export const SCREENSHOT_ARCHIVE_MAX_ENTRIES = 0xffff - 1
export const SCREENSHOT_ARCHIVE_MAX_BYTES = SCREENSHOT_ARCHIVE_ZIP32_SENTINEL - 1

type ScreenshotArchiveLimits = Readonly<{
    maxEntries: number
    maxBytes: number
}>

const ZIP32_ARCHIVE_LIMITS: ScreenshotArchiveLimits = {
    maxEntries: SCREENSHOT_ARCHIVE_MAX_ENTRIES,
    maxBytes: SCREENSHOT_ARCHIVE_MAX_BYTES,
}

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
    limits: ScreenshotArchiveLimits = ZIP32_ARCHIVE_LIMITS,
): StreamingScreenshotArchive {
    let state: 'open' | 'closed' | 'aborted' = 'open'
    let writeChain = Promise.resolve()
    let entryCount = 0
    let outputBytes = 0
    let pageBytes = 0
    let streamError: unknown
    let resolveFinal!: () => void
    let rejectFinal!: (error: unknown) => void
    const finalChunk = new Promise<void>((resolve, reject) => {
        resolveFinal = resolve
        rejectFinal = reject
    })
    void finalChunk.catch(() => {})
    const zip = new Zip((error, chunk, final) => {
        if (error) {
            streamError = error
            rejectFinal(error)
            return
        }
        if (chunk.length > 0) {
            if (outputBytes + chunk.length > limits.maxBytes) {
                streamError = new Error('Screenshot archive exceeds the ZIP32 size limit')
                rejectFinal(streamError)
                return
            }
            outputBytes += chunk.length
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
        try {
            if (!await abortOnce()) return true
        } catch (abortError) {
            if (error instanceof DOMException && error.name === 'AbortError') throw abortError
        }
        throw error
    }

    return {
        async addPage(pageNumber, page) {
            if (state !== 'open') throw new Error('Screenshot archive is finalized')
            if (!Number.isSafeInteger(pageNumber) || pageNumber < 1) {
                throw new Error('Screenshot page number must be a positive integer')
            }
            if (pageNumber > limits.maxEntries) {
                await fail(new Error('Screenshot archive exceeds the ZIP32 entry limit'))
                return
            }
            if (entryCount >= limits.maxEntries) {
                await fail(new Error('Screenshot archive exceeds the ZIP32 entry limit'))
                return
            }
            if (page.size > limits.maxBytes) {
                await fail(new Error('Screenshot page exceeds the ZIP32 size limit'))
                return
            }
            if (pageBytes + page.size > limits.maxBytes) {
                await fail(new Error('Screenshot archive exceeds the ZIP32 size limit'))
                return
            }
            try {
                const entry = new ZipPassThrough(`page-${pageNumber.toString().padStart(4, '0')}.png`)
                zip.add(entry)
                entryCount += 1
                pageBytes += page.size
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

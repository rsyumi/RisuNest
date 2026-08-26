import { Zip, ZipPassThrough } from 'fflate'

export interface ScreenshotArchiveWriter {
    write(chunk: Uint8Array): Promise<void>
    close(): Promise<void>
    abort(): Promise<void>
}

export interface StreamingScreenshotArchive {
    addPage(pageNumber: number, page: Blob): Promise<void>
    close(): Promise<void>
    abort(): Promise<void>
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

    async function abortOnce() {
        if (state === 'aborted') return
        if (state === 'closed') return
        state = 'aborted'
        zip.terminate()
        await writer.abort()
    }

    async function fail(error: unknown): Promise<never> {
        await abortOnce()
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
                entry.push(new Uint8Array(await page.arrayBuffer()), true)
                await writeChain
                if (streamError) throw streamError
            } catch (error) {
                return fail(error)
            }
        },

        async close() {
            if (state === 'aborted') throw new Error('Screenshot archive was aborted')
            if (state === 'closed') return
            try {
                zip.end()
                await finalChunk
                await writeChain
                if (streamError) throw streamError
                await writer.close()
                state = 'closed'
            } catch (error) {
                return fail(error)
            }
        },

        abort: abortOnce,
    }
}

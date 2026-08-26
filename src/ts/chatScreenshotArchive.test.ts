import { strFromU8, unzipSync } from 'fflate'
import { describe, expect, it, vi } from 'vitest'
import { createStreamingScreenshotArchive } from './chatScreenshotArchive'

function blob(text: string) {
    return new Blob([text], { type: 'image/png' })
}

describe('streaming screenshot archive', () => {
    it('writes ordered zero-padded PNG entries and closes once', async () => {
        const chunks: Uint8Array[] = []
        let activeWrites = 0
        let maxActiveWrites = 0
        const writer = {
            write: vi.fn(async (chunk: Uint8Array) => {
                activeWrites++
                maxActiveWrites = Math.max(maxActiveWrites, activeWrites)
                await Promise.resolve()
                chunks.push(chunk.slice())
                activeWrites--
            }),
            close: vi.fn(async () => {}),
            abort: vi.fn(async () => {}),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await archive.addPage(2, blob('second'))
        await archive.addPage(1, blob('first'))
        await archive.close()

        const bytes = new Uint8Array(chunks.reduce((sum, chunk) => sum + chunk.length, 0))
        let offset = 0
        for (const chunk of chunks) {
            bytes.set(chunk, offset)
            offset += chunk.length
        }
        const entries = unzipSync(bytes)

        expect(Object.keys(entries).sort()).toEqual(['page-0001.png', 'page-0002.png'])
        expect(strFromU8(entries['page-0001.png'])).toBe('first')
        expect(strFromU8(entries['page-0002.png'])).toBe('second')
        expect(maxActiveWrites).toBe(1)
        expect(writer.close).toHaveBeenCalledOnce()
        expect(writer.abort).not.toHaveBeenCalled()
    })

    it('streams each PNG blob without materializing its full array buffer', async () => {
        const page = blob('streamed page')
        const arrayBuffer = vi.spyOn(page, 'arrayBuffer')
        const writer = {
            write: vi.fn(async () => {}),
            close: vi.fn(async () => {}),
            abort: vi.fn(async () => {}),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await archive.addPage(1, page)
        await archive.close()

        expect(arrayBuffer).not.toHaveBeenCalled()
        expect(writer.write).toHaveBeenCalled()
    })

    it('aborts the writer once and cannot close or add after abort', async () => {
        const writer = {
            write: vi.fn(async () => {}),
            close: vi.fn(async () => {}),
            abort: vi.fn(async () => {}),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await archive.addPage(1, blob('page'))
        await archive.abort()
        await archive.abort()

        expect(writer.abort).toHaveBeenCalledOnce()
        expect(writer.close).not.toHaveBeenCalled()
        await expect(archive.addPage(2, blob('late'))).rejects.toThrow('finalized')
        await expect(archive.close()).rejects.toThrow('aborted')
    })

    it('aborts on a write failure without closing partial output', async () => {
        const writer = {
            write: vi.fn(async () => {
                throw new Error('disk full')
            }),
            close: vi.fn(async () => {}),
            abort: vi.fn(async () => {}),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await expect(archive.addPage(1, blob('page'))).rejects.toThrow('disk full')
        expect(writer.abort).toHaveBeenCalledOnce()
        expect(writer.close).not.toHaveBeenCalled()
    })

    it('aborts instead of completing when cancellation arrives during close', async () => {
        const controller = new AbortController()
        const writer = {
            write: vi.fn(async () => {}),
            close: vi.fn(async () => controller.abort()),
            abort: vi.fn(async () => {}),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await archive.addPage(1, blob('page'))
        await expect(archive.close(controller.signal)).rejects.toMatchObject({ name: 'AbortError' })

        expect(writer.abort).toHaveBeenCalledOnce()
    })

    it('completes when native publication has crossed the cancellation boundary', async () => {
        const controller = new AbortController()
        const writer = {
            write: vi.fn(async () => {}),
            close: vi.fn(async () => controller.abort()),
            abort: vi.fn(async () => false),
        }
        const archive = createStreamingScreenshotArchive(writer)

        await archive.addPage(1, blob('page'))
        await expect(archive.close(controller.signal)).resolves.toBe(true)

        expect(writer.abort).toHaveBeenCalledOnce()
    })
})

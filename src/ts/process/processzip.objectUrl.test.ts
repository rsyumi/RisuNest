// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { CharXWriter } from './processzip'

vi.mock('../globalApi.svelte', () => ({
    AppendableBuffer: class {
        private chunks: Uint8Array[] = []

        append(data: Uint8Array) {
            this.chunks.push(data)
        }

        get buffer() {
            const size = this.chunks.reduce((total, chunk) => total + chunk.byteLength, 0)
            const result = new Uint8Array(size)
            let offset = 0
            for (const chunk of this.chunks) {
                result.set(chunk, offset)
                offset += chunk.byteLength
            }
            return result
        }

        clear() {
            this.chunks = []
        }
    },
    saveAsset: vi.fn(),
}))

vi.mock('../alert', () => ({
    alertStore: { set: vi.fn() },
}))

vi.mock('../util', () => ({
    asBuffer: (data: Uint8Array) => data.buffer,
    Semaphore: class {},
    sleep: vi.fn(),
}))

const originalCreateElement = document.createElement.bind(document)

afterEach(() => {
    vi.restoreAllMocks()
})

function installImageConversionDom(decode: () => Promise<void>) {
    const drawImage = vi.fn()
    vi.spyOn(document, 'createElement').mockImplementation((tagName: string, options?: ElementCreationOptions) => {
        if (tagName === 'canvas') {
            return {
                width: 0,
                height: 0,
                getContext: () => ({ drawImage }),
                toBlob: (callback: BlobCallback) => callback(new Blob(['jpeg'], { type: 'image/jpeg' })),
            } as unknown as HTMLCanvasElement
        }
        if (tagName === 'img') {
            return {
                width: 4,
                height: 2,
                src: '',
                decode,
            } as unknown as HTMLImageElement
        }
        return originalCreateElement(tagName, options)
    })
    return drawImage
}

describe('CharXWriter.writeJpeg object URL lifetime', () => {
    test('revokes the temporary URL once after conversion succeeds', async () => {
        const drawImage = installImageConversionDom(vi.fn(async () => undefined))
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:charx-success')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        const writer = new CharXWriter({ write: vi.fn(), close: vi.fn() } as never)

        await writer.writeJpeg(new Uint8Array([1, 2, 3]))

        expect(drawImage).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:charx-success')
    })

    test('revokes the temporary URL once when decoding fails', async () => {
        installImageConversionDom(vi.fn(async () => {
            throw new Error('decode failed')
        }))
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:charx-failure')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        const writer = new CharXWriter({ write: vi.fn(), close: vi.fn() } as never)

        await expect(writer.writeJpeg(new Uint8Array([1, 2, 3]))).rejects.toThrow('decode failed')

        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:charx-failure')
    })
})

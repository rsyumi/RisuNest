// @vitest-environment happy-dom

import { afterEach, describe, expect, test, vi } from 'vitest'
import { downloadBlobWithObjectUrl, withObjectUrl } from './objectUrl'

afterEach(() => {
    vi.useRealTimers()
    vi.restoreAllMocks()
})

describe('withObjectUrl', () => {
    test('keeps the URL alive until the asynchronous consumer succeeds, then revokes it once', async () => {
        const createObjectURL = vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:success')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        let releaseConsumer!: () => void
        const consumerPending = new Promise<void>((resolve) => {
            releaseConsumer = resolve
        })

        const operation = withObjectUrl(new Blob(['image']), async (url) => {
            expect(url).toBe('blob:success')
            expect(revokeObjectURL).not.toHaveBeenCalled()
            await consumerPending
            return 'converted'
        })

        await Promise.resolve()
        expect(revokeObjectURL).not.toHaveBeenCalled()
        releaseConsumer()

        await expect(operation).resolves.toBe('converted')
        expect(createObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:success')
    })

    test('revokes the URL once when the consumer fails', async () => {
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:failure')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')

        await expect(withObjectUrl(new Blob(['image']), async () => {
            throw new Error('decode failed')
        })).rejects.toThrow('decode failed')

        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:failure')
    })
})

describe('downloadBlobWithObjectUrl', () => {
    test('defers revocation until after the anchor click task', async () => {
        vi.useFakeTimers()
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:download')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(function () {
            expect(this.href).toBe('blob:download')
            expect(this.download).toBe('code.ts')
            expect(revokeObjectURL).not.toHaveBeenCalled()
        })

        downloadBlobWithObjectUrl(new Blob(['const value = 1']), 'code.ts')

        expect(click).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).not.toHaveBeenCalled()
        await vi.runAllTimersAsync()
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:download')
    })

    test('still schedules one revocation when clicking throws', async () => {
        vi.useFakeTimers()
        vi.spyOn(URL, 'createObjectURL').mockReturnValue('blob:click-failure')
        const revokeObjectURL = vi.spyOn(URL, 'revokeObjectURL')
        vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {
            throw new Error('click failed')
        })

        expect(() => downloadBlobWithObjectUrl(new Blob(['code']), 'code.txt')).toThrow('click failed')
        expect(revokeObjectURL).not.toHaveBeenCalled()
        await vi.runAllTimersAsync()
        expect(revokeObjectURL).toHaveBeenCalledTimes(1)
        expect(revokeObjectURL).toHaveBeenCalledWith('blob:click-failure')
    })
})

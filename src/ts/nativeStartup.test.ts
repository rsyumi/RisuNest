import { describe, expect, test, vi } from 'vitest'
import { checkNativeStartupStatus } from './nativeStartup'

describe('checkNativeStartupStatus', () => {
    test('does not invoke native startup status in the browser', async () => {
        const invoke = vi.fn()

        await checkNativeStartupStatus(false, invoke)

        expect(invoke).not.toHaveBeenCalled()
    })

    test('surfaces the native startup failure before bootstrap continues', async () => {
        const failure = new Error('synthetic native setup failure')
        const invoke = vi.fn().mockRejectedValue(failure)

        await expect(checkNativeStartupStatus(true, invoke)).rejects.toBe(failure)
        await expect(checkNativeStartupStatus(true, invoke)).rejects.toBe(failure)
        expect(invoke).toHaveBeenCalledWith('native_startup_status')
        expect(invoke).toHaveBeenCalledOnce()
    })
})

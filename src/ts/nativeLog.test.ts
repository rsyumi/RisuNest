import { describe, expect, it, vi } from 'vitest'
import { getNativeLogFilePath, getNativeLogTail, setNativeLogFileEnabled } from './nativeLog'

describe('native diagnostic log commands', () => {
    it('requests the latest 500 log entries', async () => {
        const invoke = vi.fn(async () => [])

        await getNativeLogTail(invoke)

        expect(invoke).toHaveBeenCalledWith('native_log_tail', { limit: 500 })
    })

    it('uses the typed commands for file logging state and its path', async () => {
        const invoke = vi.fn(async () => '/data/logs/risunest.log')

        await setNativeLogFileEnabled(false, invoke)
        await expect(getNativeLogFilePath(invoke)).resolves.toBe('/data/logs/risunest.log')

        expect(invoke).toHaveBeenCalledWith('native_log_set_file_enabled', { enabled: false })
        expect(invoke).toHaveBeenCalledWith('native_log_file_path')
    })
})

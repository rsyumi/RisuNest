import { describe, expect, it, vi } from 'vitest'
import { getNativeLogFilePath, getNativeLogTail, readNativeLogFilePage, setNativeLogFileEnabled } from './nativeLog'

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

    it('reads saved log pages from the newest end or from a cursor', async () => {
        const invoke = vi.fn(async () => ({ entries: [], older: null }))

        await readNativeLogFilePage(undefined, invoke)
        await readNativeLogFilePage({ file: 'previous', before: 4096 }, invoke)

        expect(invoke.mock.calls).toEqual([
            ['native_log_file_read', { file: 'current', before: null }],
            ['native_log_file_read', { file: 'previous', before: 4096 }],
        ])
    })
})

import { describe, expect, it, vi } from 'vitest'
import { androidSafReservationFixture } from '../../tests/support/androidSafReservation'
const picker = vi.hoisted(() => ({ reserve: undefined as unknown as typeof import('./storage/androidSafBridge').reserveAndroidSafDestination }))
vi.mock('./platform', async () => (await import('./platform.testSupport')).platformModule({ runtime: 'tauri', os: 'android' }))
vi.mock('./storage/androidSafBridge', async (importOriginal) => ({
    ...await importOriginal<typeof import('./storage/androidSafBridge')>(),
    reserveAndroidSafDestination: (...args: Parameters<typeof picker.reserve>) => picker.reserve(...args),
}))
import {
    getNativeLogFilePath, getNativeLogTail, NATIVE_LOG_FILE_EMPTY, readNativeLogFilePage, saveNativeLogFile, setNativeLogFileEnabled,
} from './nativeLog'

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

    it('reports a refused save as a failure when the picked Android destination may keep part of a file', async () => {
        const actual = await vi.importActual<typeof import('./storage/androidSafBridge')>('./storage/androidSafBridge')
        const fixture = androidSafReservationFixture(undefined, actual.reserveAndroidSafDestination)
        fixture.failRelease()
        picker.reserve = fixture.reserve
        const invoke = vi.fn(async (command: string) => {
            if (command === 'native_log_file_read') return { entries: [{ tsMs: 1, level: 'error', target: 'synthetic', message: 'synthetic' }], older: null }
            throw NATIVE_LOG_FILE_EMPTY
        })

        await expect(saveNativeLogFile(0, invoke)).rejects.toMatchObject({
            code: NATIVE_LOG_FILE_EMPTY,
            warningCodes: ['partial-destination-may-remain'],
        })
        expect(invoke).toHaveBeenCalledWith('native_log_file_export', { destination: undefined })
    })
})

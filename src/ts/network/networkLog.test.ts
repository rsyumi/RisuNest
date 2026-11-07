import { beforeEach, describe, expect, it, vi } from 'vitest'
import type { PlatformShape } from '../platform.testSupport'
import { androidSafReservationFixture } from '../../../tests/support/androidSafReservation'
const native = vi.hoisted(() => ({ invoke: vi.fn(), save: vi.fn(), android: vi.fn(), reserve: vi.fn(), ios: vi.fn(), platform: { runtime: 'tauri', os: 'windows' } as PlatformShape }))
vi.mock('@tauri-apps/api/core', () => ({ invoke: native.invoke }))
vi.mock('@tauri-apps/plugin-dialog', () => ({ save: native.save }))
vi.mock('../platform', async () => (await import('../platform.testSupport')).platformModule(() => native.platform))
vi.mock('../storage/androidSafBridge', async (importOriginal) => ({
    ...await importOriginal<typeof import('../storage/androidSafBridge')>(),
    copyNativeExportToAndroidSaf: native.android,
    reserveAndroidSafDestination: native.reserve,
}))
vi.mock('../storage/iosFiles', () => ({ exportIOSFile: native.ios }))
import { downloadNetworkLog, parseSsePreview, prettyLogJson } from './networkLog'

beforeEach(() => {
    vi.clearAllMocks()
    native.platform = { runtime: 'tauri', os: 'windows' }
    native.reserve.mockResolvedValue({requestId: 'synthetic-request', bind: async () => {}, release: async () => {}})
})
describe('network log viewer', () => {
    it.each(['cancelled', 'refused', 'failed', 'succeeded'])('preserves the %s outcome when Android reservation cleanup fails', async outcome => {
        native.platform = {runtime: 'tauri', os: 'android'}
        const actual = await vi.importActual<typeof import('../storage/androidSafBridge')>('../storage/androidSafBridge')
        const picker = androidSafReservationFixture(undefined, actual.reserveAndroidSafDestination)
        picker.failRelease()
        native.reserve.mockImplementation(picker.reserve)
        native.invoke.mockResolvedValue({path: 'synthetic-handoff', bytes: 123})
        // The command refuses with a string, as Tauri rejects.
        if (outcome === 'refused') native.invoke.mockRejectedValueOnce('network-log-export-failed')
        const failure = outcome === 'cancelled' ? new DOMException('cancelled', 'AbortError') : Object.assign(new Error('producer'), {code: 'producer-failed'})
        if (outcome === 'succeeded') native.android.mockResolvedValueOnce({bytes: 123})
        else if (outcome !== 'refused') native.android.mockRejectedValueOnce(failure)
        const result = downloadNetworkLog('synthetic')
        // Every outcome that may leave part of a file at the picked destination rejects with its warning.
        if (outcome === 'refused') {
            await expect(result).rejects.toMatchObject({code: 'network-log-export-failed', warningCodes: ['partial-destination-may-remain']})
            expect(native.invoke).not.toHaveBeenCalledWith('native_download_handoff_cleanup', expect.anything())
            return
        }
        if (outcome === 'succeeded') await expect(result).rejects.toMatchObject({code: 'destination-cleanup-failed'})
        else {
            await expect(result).rejects.toBe(failure)
            expect(failure).toMatchObject({warningCodes: ['partial-destination-may-remain']})
        }
        expect(native.invoke).toHaveBeenLastCalledWith('native_download_handoff_cleanup', {path: 'synthetic-handoff'})
    })
    it('does not write a log export when Android destination selection is cancelled', async () => {
        native.platform = {runtime: 'tauri', os: 'android'}
        native.reserve.mockRejectedValueOnce(new DOMException('cancelled', 'AbortError'))
        await expect(downloadNetworkLog('synthetic')).resolves.toBe(false)
        expect(native.invoke).not.toHaveBeenCalled()
    })
    it('bounds JSON formatting work and leaves malformed bodies in raw view', () => {
        expect(prettyLogJson('{"value":"[\\\"{}]"}')).not.toBeNull()
        expect(prettyLogJson('['.repeat(30) + '0' + ']'.repeat(30))).toBeNull()
        expect(prettyLogJson('"' + 'x'.repeat(65536) + '"')).toBeNull()
        expect(prettyLogJson('data: incomplete')).toBeNull()
    })
    it('preserves comments, mixed line endings, multiple data lines and unfinished tails', () => {
        const text = ': keepalive\r\nevent: delta\r\nid: 9\r\ndata: 가\r\ndata: 나\r\n\r\ndata: unfinished'
        const events = parseSsePreview(text)
        expect(events[0]).toMatchObject({ event: 'delta', id: '9', data: '가\n나', complete: true, partial: false })
        expect(events[1]).toMatchObject({ raw: 'data: unfinished', complete: false })
        expect(events.map(event => event.raw).join('')).toBe(text)
        expect(parseSsePreview('tail\n\ndata: next\n\n', 256)[0].partial).toBe(true)
    })
    it('bounds dense SSE event pages and keeps the exact byte offset', () => {
        const text = 'data: 안녕\n\n'.repeat(80)
        const events = parseSsePreview(text, 17)
        expect(events).toHaveLength(50)
        expect(events.at(-1)?.endOffset).toBe(17 + new TextEncoder().encode('data: 안녕\n\n'.repeat(50)).length)
    })
    it('uses the desktop save result and makes cancellation a no-op', async () => {
        native.save.mockResolvedValueOnce(null)
        expect(await downloadNetworkLog('synthetic')).toBe(false)
        expect(native.invoke).not.toHaveBeenCalled()
        native.save.mockResolvedValueOnce('selected.json')
        native.invoke.mockResolvedValueOnce({ bytes: 123 })
        expect(await downloadNetworkLog('synthetic')).toBe(true)
        expect(native.invoke).toHaveBeenCalledWith('network_log_command', { request: { op: 'export', id: 'synthetic', destination: 'selected.json' } })
    })
    for (const platform of ['android', 'ios'] as const) {
        it(`uses the ${platform} native file handoff and always cleans it`, async () => {
            native.platform = { runtime: 'tauri', os: platform }
            native.invoke.mockResolvedValueOnce({ path: 'synthetic-handoff', bytes: 123 }).mockResolvedValueOnce(undefined)
            native[platform].mockResolvedValueOnce({ bytes: 123 })
            expect(await downloadNetworkLog('synthetic')).toBe(true)
            expect(native[platform]).toHaveBeenCalledWith(expect.objectContaining({ sourcePath: 'synthetic-handoff', suggestedName: 'network-synthetic.json' }))
            expect(native.invoke).toHaveBeenLastCalledWith('native_download_handoff_cleanup', { path: 'synthetic-handoff' })

            native.invoke.mockResolvedValueOnce({ path: 'synthetic-handoff', bytes: 123 }).mockResolvedValueOnce(undefined)
            native[platform].mockResolvedValueOnce({ bytes: 100 })
            await expect(downloadNetworkLog('synthetic')).rejects.toThrow('native-export-incomplete')
            expect(native.invoke).toHaveBeenLastCalledWith('native_download_handoff_cleanup', { path: 'synthetic-handoff' })
        })
    }
    it('cleans a cancelled mobile export', async () => {
        native.platform = { runtime: 'tauri', os: 'android' }
        native.invoke.mockResolvedValueOnce({ path: 'synthetic-handoff', bytes: 123 }).mockResolvedValueOnce(undefined)
        native.android.mockRejectedValueOnce(Object.assign(new Error('cancelled'), { name: 'AbortError' }))
        expect(await downloadNetworkLog('synthetic')).toBe(false)
        expect(native.invoke).toHaveBeenLastCalledWith('native_download_handoff_cleanup', { path: 'synthetic-handoff' })
    })
})

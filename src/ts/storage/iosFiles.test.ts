import { beforeEach, describe, expect, it, vi } from 'vitest'
const { invoke } = vi.hoisted(() => ({ invoke: vi.fn() }))
vi.mock('@tauri-apps/api/core', () => ({ invoke }))
import { exportIOSFile, pickIOSFile, getIOSPublication } from './iosFiles'
beforeEach(() => {
    invoke.mockReset()
})
describe('iOS file publication', () => {
    it('reports picker cancellation without publishing success', async () => {
        invoke.mockResolvedValue({ cancelled: true })
        await expect(
            exportIOSFile({
                sourcePath: '/owned/file',
                suggestedName: 'backup.risunest',
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })
    })
    it('cleans the app-owned import copy when cancellation arrives during the picker', async () => {
        const controller = new AbortController()
        invoke.mockImplementation(async (command) => {
            if (command.endsWith('pick_file')) {
                controller.abort()
                return {
                    path: '/staged/file',
                    name: 'synthetic',
                    bytes: 1,
                    cancelled: false,
                }
            }
        })
        await expect(pickIOSFile(controller.signal)).rejects.toMatchObject({
            name: 'AbortError',
        })
        expect(invoke).toHaveBeenLastCalledWith(
            'plugin:ios-native|discard_file',
            { path: '/staged/file' },
        )
    })
    it('keeps a completed publication successful after a late abort', async () => {
        const controller = new AbortController()
        invoke.mockImplementation(async () => {
            controller.abort()
            return { cancelled: false, bytes: 42 }
        })
        await expect(
            exportIOSFile({
                sourcePath: '/owned/file',
                suggestedName: 'backup.risunest',
                signal: controller.signal,
            }),
        ).resolves.toEqual({ bytes: 42 })
    })
    it('retains recoverable receipts until the caller has stored its publication result', async () => {
        invoke.mockResolvedValue({ cancelled: false, bytes: 42 })
        await exportIOSFile({ sourcePath: '/owned/file', suggestedName: 'backup.risunest', requestId: 'caller-owned' })
        expect(invoke).toHaveBeenCalledOnce()
        invoke.mockClear()
        await exportIOSFile({ sourcePath: '/owned/file', suggestedName: 'backup.risunest' })
        expect(invoke).toHaveBeenLastCalledWith('plugin:ios-native|acknowledge_publication', { id: expect.any(String) })
    })
    it('never treats a pending publication receipt as success', async () => {
        invoke.mockResolvedValue({ state: 'pending' })
        await expect(getIOSPublication('receipt')).resolves.toBeNull()
        invoke.mockResolvedValue({ state: 'succeeded', bytes: 42 })
        await expect(getIOSPublication('receipt')).resolves.toEqual({
            bytes: 42,
        })
    })
})

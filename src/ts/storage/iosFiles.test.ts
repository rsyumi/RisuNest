import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { clearMocks, mockIPC } from '@tauri-apps/api/mocks'
import type { Channel } from '@tauri-apps/api/core'
const { invoke, roots, alertError, waitAlert } = vi.hoisted(() => ({ invoke: vi.fn(), roots: vi.fn(), alertError: vi.fn(), waitAlert: vi.fn() }))
vi.mock('@tauri-apps/api/core', async original => ({ ...(await original<typeof import('@tauri-apps/api/core')>()),
    invoke: ((command, ...args) => command === 'app_paths_roots' ? roots() : invoke(command, ...args)) as typeof import('@tauri-apps/api/core').invoke,
} satisfies typeof import('@tauri-apps/api/core')))
vi.mock('../alert', () => ({ alertError, waitAlert }))
vi.mock('../gui/nativeFileJobDialogModel', () => ({ failureReason: (code: string) => `reason:${code}` }))
import { exportIOSFile, pickIOSFile, pickIOSFiles, getIOSPublication, pickIOSBackupSource, materializeIOSBackupSource, reportInterruptedIOSBackupSources } from './iosFiles'
import { isLibraryFileOperationReserved } from './libraryFileOperation'
beforeEach(() => {
    mockIPC(() => undefined)
    invoke.mockReset()
    roots.mockReset().mockResolvedValue({ data: '/owned' })
    alertError.mockReset()
    waitAlert.mockReset().mockResolvedValue(undefined)
})
afterEach(() => clearMocks())
describe('iOS file publication', () => {
    it('returns native custody without an app-owned copy or a descriptor in JavaScript', async () => {
        invoke.mockImplementation(async command => command === 'native_portable_source_cleanup_orphans' ? {names:[],cleanupFailed:false}
            : {cancelled:false, token:'selected-token', name:'synthetic.risunest', bytes:4_294_967_296})
        await expect(pickIOSBackupSource()).resolves.toEqual({token:'selected-token', name:'synthetic.risunest', bytes:4_294_967_296})
        expect(invoke.mock.calls.map(call => call[0])).toEqual(['native_portable_source_cleanup_orphans','plugin:ios-native|pick_backup_source'])
        expect(alertError).not.toHaveBeenCalled()
    })
    it('says an earlier import did not finish before the picker opens', async () => {
        const order: string[] = []
        invoke.mockImplementation(async command => {
            order.push(command)
            return command === 'native_portable_source_cleanup_orphans' ? {names:['earlier.risunest'],cleanupFailed:false}
                : {cancelled:false, token:'selected-token', name:'synthetic.risunest', bytes:42}
        })
        alertError.mockImplementation(message => order.push(`alert:${message}`))
        waitAlert.mockImplementation(async () => { order.push('dismissed') })
        await pickIOSBackupSource()
        expect(order).toEqual(['native_portable_source_cleanup_orphans','alert:earlier.risunest: reason:import-interrupted','dismissed','plugin:ios-native|pick_backup_source'])
    })
    it('reports sources a reloaded page left unimported once', async () => {
        invoke.mockResolvedValueOnce({names:['one.risunest','two.risunest'],cleanupFailed:false}).mockResolvedValueOnce({names:[],cleanupFailed:false})
        await reportInterruptedIOSBackupSources()
        await reportInterruptedIOSBackupSources()
        expect(invoke.mock.calls).toEqual([['native_portable_source_cleanup_orphans'],['native_portable_source_cleanup_orphans']])
        expect(alertError.mock.calls).toEqual([['one.risunest: reason:import-interrupted'], ['two.risunest: reason:import-interrupted']])
    })
    it('keeps unavailable names anonymous and never displays a source path', async () => {
        invoke.mockResolvedValueOnce({names:[null],cleanupFailed:false})
        await reportInterruptedIOSBackupSources()
        expect(alertError).toHaveBeenCalledExactlyOnceWith('reason:import-interrupted')
        invoke.mockResolvedValueOnce({names:['/private/synthetic.risunest'],cleanupFailed:false})
        await expect(reportInterruptedIOSBackupSources()).rejects.toThrow('receipt is invalid')
        expect(alertError).toHaveBeenCalledOnce()
    })
    it('delivers cleaned receipts before refusing the next pick and retries only unfinished cleanup', async () => {
        invoke.mockResolvedValueOnce({names:['first.risunest'],cleanupFailed:true})
        await expect(pickIOSBackupSource()).rejects.toThrow('cleanup failed')
        expect(invoke).not.toHaveBeenCalledWith('plugin:ios-native|pick_backup_source')
        invoke.mockResolvedValueOnce({names:['second.risunest'],cleanupFailed:false})
            .mockResolvedValueOnce({cancelled:true})
        await expect(pickIOSBackupSource()).resolves.toBeNull()
        expect(alertError.mock.calls).toEqual([['first.risunest: reason:import-interrupted'], ['second.risunest: reason:import-interrupted']])
    })
    it('joins simultaneous startup and picker notices until dismissal', async () => {
        let dismiss!: () => void
        waitAlert.mockImplementationOnce(() => new Promise<void>(resolve => { dismiss = resolve }))
        invoke.mockResolvedValueOnce({names:['first.risunest'],cleanupFailed:false})
            .mockResolvedValueOnce({cancelled:true})
        const notice = reportInterruptedIOSBackupSources()
        const pick = pickIOSBackupSource()
        await vi.waitFor(() => expect(alertError).toHaveBeenCalledOnce())
        expect(invoke).toHaveBeenCalledOnce()
        dismiss()
        await Promise.all([notice, pick])
        expect(invoke).toHaveBeenLastCalledWith('plugin:ios-native|pick_backup_source')
    })
    it('propagates deferred native cleanup failure before reopening the picker and permits the next query', async () => {
        const failure = { code: 'cleanup-failed', message: 'Source cleanup could not be confirmed' }
        invoke.mockRejectedValueOnce(failure)
        await expect(pickIOSBackupSource()).rejects.toBe(failure)
        expect(invoke).not.toHaveBeenCalledWith('plugin:ios-native|pick_backup_source')
        invoke.mockResolvedValueOnce({ names: [], cleanupFailed: false }).mockResolvedValueOnce({ cancelled: true })
        await expect(pickIOSBackupSource()).resolves.toBeNull()
    })
    it('releases native and scoped custody after cancellation during selection', async () => {
        const controller = new AbortController()
        invoke.mockImplementation(async command => {
            if (command === 'plugin:ios-native|pick_backup_source') {
                controller.abort()
                return {cancelled:false, token:'selected-token', name:'synthetic.risunest', bytes:42}
            }
            if (command === 'native_portable_source_cleanup_orphans') return {names:[],cleanupFailed:false}
            return true
        })
        await expect(pickIOSBackupSource(controller.signal)).rejects.toMatchObject({name:'AbortError'})
        expect(invoke).toHaveBeenLastCalledWith('native_portable_source_discard', {source:{type:'iosScoped', token:'selected-token'}})
    })
    it('requests an upstream compatibility copy by token, never reopening a selected URL', async () => {
        invoke.mockResolvedValue({path:'/owned/synthetic.risudat',name:'synthetic.risudat',bytes:42})
        await expect(materializeIOSBackupSource('selected-token')).resolves.toEqual({path:'/owned/synthetic.risudat',name:'synthetic.risudat',bytes:42})
        expect(invoke).toHaveBeenCalledExactlyOnceWith('plugin:ios-native|materialize_backup_source',{token:'selected-token'})
    })
    it('reports large copy progress and ignores callbacks or cancellation after completion', async () => {
        const controller = new AbortController()
        const reports: unknown[] = []
        let channel!: Channel<{ copiedBytes: number; totalBytes: number }>
        invoke.mockImplementation(async (_command, args) => {
            channel = args.onProgress
            channel.onmessage({ copiedBytes: 4 * 1024 ** 3, totalBytes: 8 * 1024 ** 3 })
            return { path: '/owned/copied.risudat', name: 'copied.risudat', bytes: 8 * 1024 ** 3 }
        })
        await expect(materializeIOSBackupSource('source', { signal: controller.signal, onProgress: progress => reports.push(progress) }))
            .resolves.toMatchObject({ path: '/owned/copied.risudat' })
        controller.abort()
        channel.onmessage({ copiedBytes: 8 * 1024 ** 3, totalBytes: 8 * 1024 ** 3 })
        expect(reports).toEqual([{ copiedBytes: 4 * 1024 ** 3, totalBytes: 8 * 1024 ** 3 }])
        expect(invoke.mock.calls.map(call => call[0])).toEqual(['plugin:ios-native|materialize_backup_source'])
    })
    it('ignores copy progress that arrives after a finished copy nobody cancelled', async () => {
        const reports: unknown[] = []
        let channel!: Channel<{ copiedBytes: number; totalBytes: number }>
        invoke.mockImplementation(async (_command, args) => {
            channel = args.onProgress
            return { path: '/owned/copied.risudat', name: 'copied.risudat', bytes: 42 }
        })
        await materializeIOSBackupSource('source', { onProgress: progress => reports.push(progress) })
        channel.onmessage({ copiedBytes: 42, totalBytes: 42 })
        expect(reports).toEqual([])
    })
    // androidSafBridge.test.ts expects the same reasons for the same native codes.
    it.each([
        ['source-changed', 'source-reselect-required'],
        ['cancelled', 'source-reselect-required'],
        ['spool-create-failed', 'spool-write-failed'],
        ['source-busy', 'source-busy'],
        ['source-read-failed', 'source-read-failed'],
    ])('reports a native %s copy failure with the %s dialog reason', async (nativeCode, code) => {
        invoke.mockRejectedValue({ message: 'Backup source could not be copied', code: nativeCode })
        await expect(materializeIOSBackupSource('source')).rejects.toMatchObject({ code, message: expect.stringContaining(nativeCode) })
    })
    it('passes on a copy rejection that carries no failure code', async () => {
        invoke.mockRejectedValue('plugin unavailable')
        await expect(materializeIOSBackupSource('source')).rejects.toBe('plugin unavailable')
    })
    it.each([false, true])('cancels the selected copy and cleans any late result (failed %s)', async failed => {
        const controller = new AbortController()
        let finish!: (value: unknown) => void
        let fail!: (error: Error) => void
        const copy = new Promise((resolve, reject) => { finish = resolve; fail = reject })
        const discarded: string[] = []
        invoke.mockImplementation(async (command, args) => {
            if (command === 'plugin:ios-native|materialize_backup_source') return copy
            if (command === 'native_portable_source_discard') {
                expect(args.source).toEqual({ type: 'iosScoped', token: 'source' })
                if (failed) fail(new Error('Native copy cancelled'))
                else finish({ path: '/owned/late.risudat', name: 'late.risudat', bytes: 42 })
                return true
            }
            if (command === 'plugin:ios-native|discard_file') discarded.push(args.path)
        })
        const pending = materializeIOSBackupSource('source', { signal: controller.signal })
        const rejected = expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('plugin:ios-native|materialize_backup_source', { token: 'source' }))
        controller.abort()
        await rejected
        expect(discarded).toEqual(failed ? [] : ['/owned/late.risudat'])
    })
    it('does not begin materialization when already cancelled', async () => {
        const controller = new AbortController()
        controller.abort()
        await expect(materializeIOSBackupSource('source', { signal: controller.signal })).rejects.toMatchObject({ name: 'AbortError' })
        expect(invoke).not.toHaveBeenCalled()
    })
    it('reports picker cancellation without publishing success', async () => {
        invoke.mockResolvedValue({ cancelled: true })
        await expect(
            exportIOSFile({
                sourcePath: '/owned/file',
                suggestedName: 'backup.risunest',
            }),
        ).rejects.toMatchObject({ name: 'AbortError' })
    })
    it('keeps selection reserved through native return and cleanup', async () => {
        let finish!: (value: { cancelled: boolean }) => void
        invoke.mockImplementation(() => new Promise(resolve => { finish = resolve }))
        const selected = pickIOSFile()
        expect(isLibraryFileOperationReserved()).toBe(true)
        window.dispatchEvent(new Event('focus'))
        document.dispatchEvent(new Event('visibilitychange'))
        await vi.waitFor(() => expect(invoke).toHaveBeenCalledWith('plugin:ios-native|pick_file'))
        expect(isLibraryFileOperationReserved()).toBe(true)
        finish({ cancelled: true })
        expect(await selected).toBeNull()
        expect(isLibraryFileOperationReserved()).toBe(false)
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
    it('opens one picker for several files, returns the files it could not copy, and cleans every staged copy on cancellation', async () => {
        const files = [
            { path: '/staged/first', name: 'first.charx', bytes: 1 },
            { path: '/staged/second', name: 'second.png', bytes: 2 },
        ]
        invoke.mockImplementation(async (command) => command.endsWith('pick_files') ? { cancelled: false, files } : undefined)
        await expect(pickIOSFiles()).resolves.toEqual({ files, failures: [] })
        expect(invoke).toHaveBeenCalledExactlyOnceWith('plugin:ios-native|pick_files', { multiple: true, partial: true })

        const failures = [{ name: 'cloud.charx', message: 'synthetic download failure' }]
        invoke.mockImplementation(async () => ({ cancelled: false, files: [], failures }))
        await expect(pickIOSFiles()).resolves.toEqual({ files: [], failures })

        invoke.mockClear()
        const controller = new AbortController()
        invoke.mockImplementation(async (command) => {
            if (!command.endsWith('pick_files')) return undefined
            controller.abort()
            return { cancelled: false, files }
        })
        await expect(pickIOSFiles(controller.signal)).rejects.toMatchObject({ name: 'AbortError' })
        expect(invoke.mock.calls.slice(1)).toEqual([
            ['plugin:ios-native|discard_file', { path: '/staged/first' }],
            ['plugin:ios-native|discard_file', { path: '/staged/second' }],
        ])
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
type IOSFiles = typeof import('./iosFiles')
const stagedCalls: [string, string, (files: IOSFiles, signal?: AbortSignal) => Promise<unknown>, unknown][] = [
    ['pickIOSFile', 'pick_file', (files, signal) => files.pickIOSFile(signal), { cancelled: true }],
    ['pickIOSFiles', 'pick_files', (files, signal) => files.pickIOSFiles(signal), { cancelled: true }],
    ['exportIOSFile', 'export_file', (files, signal) => files.exportIOSFile({ sourcePath: '/owned/file', suggestedName: 'backup.risunest', requestId: 'caller-owned', signal }), { cancelled: false, bytes: 42 }],
    ['materializeIOSBackupSource', 'materialize_backup_source', (files, signal) => files.materializeIOSBackupSource('source', { signal }), { path: '/owned/copied.risudat', name: 'copied.risudat', bytes: 42 }],
    ['pickRewritableIOSFile', 'pick_rewritable_file', files => files.pickRewritableIOSFile('translated.po'), { cancelled: true }],
    ['getIOSPublication', 'publication', files => files.getIOSPublication('receipt'), { state: 'unknown' }],
    ['acknowledgeIOSPublication', 'acknowledge_publication', files => files.acknowledgeIOSPublication('receipt'), undefined],
]
describe('iOS data root handover', () => {
    // A fresh module graph has no cached root, as on a fresh launch, and the native side rejects staged work until it arrives.
    async function beforeDataRoot(reply: unknown) {
        vi.resetModules()
        let delivered = false
        let deliver!: () => void
        roots.mockImplementation(() => new Promise(resolve => { deliver = () => { delivered = true; resolve({ data: '/owned' }) } }))
        invoke.mockImplementation(async () => {
            if (!delivered) throw new Error('The application data root has not been received yet')
            return reply
        })
        return { files: await import('./iosFiles'), deliver: () => deliver() }
    }
    it.each(stagedCalls)('%s sends %s only after the native side holds the data root', async (_name, command, run, reply) => {
        const { files, deliver } = await beforeDataRoot(reply)
        const pending = run(files)
        await vi.waitFor(() => expect(roots).toHaveBeenCalledOnce())
        expect(invoke).not.toHaveBeenCalled()
        deliver()
        await pending
        expect(invoke.mock.calls.map(([name]) => name)).toEqual([`plugin:ios-native|${command}`])
    })
    it.each(stagedCalls.slice(0, 4))('%s sends nothing when cancelled while the data root is pending', async (_name, _command, run, reply) => {
        const { files, deliver } = await beforeDataRoot(reply)
        const controller = new AbortController()
        const pending = run(files, controller.signal)
        await vi.waitFor(() => expect(roots).toHaveBeenCalledOnce())
        controller.abort()
        deliver()
        await expect(pending).rejects.toMatchObject({ name: 'AbortError' })
        expect(invoke).not.toHaveBeenCalled()
    })
})

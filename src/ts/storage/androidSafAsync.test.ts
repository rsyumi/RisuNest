import { afterEach, describe, expect, it, vi } from 'vitest'
import {
    copyNativeExportToAndroidSaf,
    pickAndroidBackupSource,
    type AndroidSafJavascriptBridge,
} from './androidSafBridge'
import { portableAndroidPublicationDependencies, type PendingPortableExport } from './deviceBackup/job'

const requestId = '11111111-1111-4111-8111-111111111111'
const exportId = '22222222-2222-4222-8222-222222222222'

function fixture(bridge: AndroidSafJavascriptBridge) {
    const target = new EventTarget()
    const remove = vi.fn((name: string, listener: (event: Event) => void) => target.removeEventListener(name, listener))
    return {
        dependencies: {
            bridge,
            createRequestId: () => requestId,
            addEventListener: (name: string, listener: (event: Event) => void) => target.addEventListener(name, listener),
            removeEventListener: remove,
        },
        remove,
        emit: (name: string, detail: unknown) => target.dispatchEvent(new CustomEvent(name, { detail })),
    }
}

const request = { sourcePath: '/synthetic/owned/archive.bin', suggestedName: 'backup.bin' }
const terminal = { requestId, exportId, state: 'succeeded' as const, bytes: 3, warningCodes: [] }

afterEach(() => { delete window.RisuSafBridge })

describe('asynchronous Android file ownership', () => {
    it('awaits a refused acknowledgement and processes duplicate terminals only once', async () => {
        let finish!: (value: boolean) => void
        const acknowledgeExport = vi.fn(() => new Promise<boolean>((resolve) => { finish = resolve }))
        const { dependencies, emit } = fixture({ copyExport: vi.fn(), acknowledgeExport })
        const output = copyNativeExportToAndroidSaf(request, dependencies)
        const rejected = expect(output).rejects.toMatchObject({ code: 'acknowledgement-failed', requestId })
        emit('risu-android-saf-destination', terminal)
        emit('risu-android-saf-destination', terminal)
        await vi.waitFor(() => expect(acknowledgeExport).toHaveBeenCalledOnce())
        finish(false)
        await rejected
    })

    it('retains the copy listener after an uncertain cancellation reply', async () => {
        const controller = new AbortController()
        const cancelExport = vi.fn(async () => { throw new Error('reply lost') })
        const { dependencies, emit, remove } = fixture({ copyExport: vi.fn(), cancelExport })
        let settled = false
        const output = copyNativeExportToAndroidSaf({ ...request, signal: controller.signal }, dependencies)
        void output.then(() => { settled = true })
        controller.abort()
        await vi.waitFor(() => expect(cancelExport).toHaveBeenCalledOnce())
        expect(settled).toBe(false)
        expect(remove).not.toHaveBeenCalled()
        emit('risu-android-saf-destination', terminal)
        await expect(output).resolves.toMatchObject({ bytes: 3 })
        expect(remove).toHaveBeenCalled()
    })

    it('cleans the original cancelled source once after its terminal arrives', async () => {
        const controller = new AbortController()
        let finish!: (value: boolean) => void
        const discardSource = vi.fn(() => new Promise<boolean>((resolve) => { finish = resolve }))
        const { dependencies, emit, remove } = fixture({
            copyExport: vi.fn(), pickBackupSource: vi.fn(), discardSource,
            cancelSource: async () => { throw new Error('reply lost') },
        })
        const source = pickAndroidBackupSource({ signal: controller.signal }, dependencies)
        const rejected = expect(source).rejects.toMatchObject({ name: 'AbortError' })
        controller.abort()
        const batch = { requestId, ready: [{ token: exportId, displayName: 'backup.bin', bytes: 3 }], failures: [] }
        emit('risu-android-backup-source-picked', batch)
        emit('risu-android-backup-source-picked', batch)
        await vi.waitFor(() => expect(discardSource).toHaveBeenCalledOnce())
        expect(remove).not.toHaveBeenCalled()
        finish(true)
        await rejected
        expect(remove).toHaveBeenCalled()
    })

    it('does not lose completion delivered during an asynchronous recovery lookup', async () => {
        let finish!: (value: string | null) => void
        const copyExport = vi.fn()
        window.RisuSafBridge = {
            copyExport,
            getExportStatus: () => new Promise((resolve) => { finish = resolve }),
            getExportSourceId: async () => null,
        }
        const intent: PendingPortableExport = {
            schema: 'risunest.portable-export-intent/v1',
            jobId: 'synthetic-job', publication: 'android-saf', phase: 'publishing',
            requestId, suggestedName: 'backup.risunest',
        }
        const result = {
            revision: 1, sourceBytes: 3, sourceSha256: 'a'.repeat(64),
            characterCount: 1, presetCount: 1, warningCodes: [],
            handoffPath: `/synthetic/owned/${exportId}/backup.risunest`,
        }
        const output = portableAndroidPublicationDependencies().resumeAndroid(intent, result)
        window.dispatchEvent(new CustomEvent('risu-android-saf-destination', { detail: terminal }))
        finish(null)
        await expect(output).resolves.toEqual(terminal)
        expect(copyExport).not.toHaveBeenCalled()
    })
})

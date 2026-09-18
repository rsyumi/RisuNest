import { describe, expect, it, vi } from 'vitest'
import { IDBFactory } from 'fake-indexeddb'
import { ExternalStorageBridge, ExternalStorageUnsupportedError } from './bridge'

describe('ExternalStorageBridge', () => {
    it('captures the native revision and selection identity for an exit drain', async () => {
        const capture = {
            revision: '9',
            libraryEpoch: 'library-epoch',
            selection: {
                kind: 'external' as const,
                connectionId: 'connection',
                selectionEpoch: 'selection-epoch',
                paused: false,
                decisionRequired: false,
            },
        }
        const invoke = vi.fn(async () => capture)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await expect(bridge.captureExitTarget()).resolves.toBe(capture)
        expect(invoke).toHaveBeenCalledWith('external_storage_capture_exit_target', undefined)
    })

    it('reports unsupported web state without invoking native commands', async () => {
        const invoke = vi.fn()
        const bridge = new ExternalStorageBridge({ supported: () => false, invoke })
        await expect(bridge.getState()).resolves.toMatchObject({ supported: false, connections: [] })
        await expect(bridge.listProviders()).rejects.toBeInstanceOf(ExternalStorageUnsupportedError)
        expect(invoke).not.toHaveBeenCalled()
    })

    it('passes secrets only to the commit command after preparation', async () => {
        const invoke = vi.fn(async () => ({ connection: { id: 'connection' } }))
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })
        await bridge.commitConnection('prepared', { kind: 'webdav', password: 'secret' })
        expect(invoke).toHaveBeenCalledWith('external_storage_commit_connection', {
            request: {
                preparationId: 'prepared',
                secret: { kind: 'webdav', password: 'secret' },
            },
        })
    })

    it('sends a current-platform OAuth client only for authenticated recovery', async () => {
        const pending = {
            authorizationId: 'authorization',
            expiresAtMs: '1',
            state: 'browser-required',
        }
        const invoke = vi.fn(async () => pending)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await expect(bridge.beginAuthorization('prepared', 'platform-client')).resolves.toBe(pending)
        expect(invoke).toHaveBeenCalledWith('external_storage_begin_authorization', {
            request: {
                preparationId: 'prepared',
                currentPlatformClientId: 'platform-client',
            },
        })
    })

    it('sends the manual OAuth callback and client secret only to completion', async () => {
        const result = { connection: { id: 'connection' } }
        const invoke = vi.fn(async () => result)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await expect(bridge.completeAuthorization(
            'authorization',
            'https://callback.example/oauth?code=one&state=two',
            'transient-secret',
        )).resolves.toBe(result)
        expect(invoke).toHaveBeenCalledWith('external_storage_complete_authorization', {
            request: {
                authorizationId: 'authorization',
                redirectUrl: 'https://callback.example/oauth?code=one&state=two',
                clientSecret: 'transient-secret',
            },
        })
    })

    it('cancels a pending authorization by its opaque identifier', async () => {
        const invoke = vi.fn(async () => undefined)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await bridge.cancelAuthorization('authorization')

        expect(invoke).toHaveBeenCalledWith('external_storage_cancel_authorization', {
            authorizationId: 'authorization',
        })
    })

    it('returns an explicit non-consuming pending completion result', async () => {
        const pending = { authorizationPending: true as const, callbackRejected: true as const }
        const invoke = vi.fn(async () => pending)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await expect(bridge.completeAuthorization('authorization')).resolves.toBe(pending)
        expect(invoke).toHaveBeenCalledWith('external_storage_complete_authorization', {
            request: { authorizationId: 'authorization' },
        })
    })

    it('never sends recovery key bytes through the import command', async () => {
        const invoke = vi.fn(async () => ({ preparationId: 'prepared' }))
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })
        await bridge.prepareRecoveryImport('authenticated-envelope', 'word-word-word')
        expect(invoke).toHaveBeenCalledWith('external_storage_prepare_recovery_import', {
            request: { payload: 'authenticated-envelope', code: 'word-word-word' },
        })
    })

    it('forwards the opaque execution session identity', async () => {
        const invoke = vi.fn(async () => undefined)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })
        await bridge.setExecutionSession({ kind: 'exitDrain', id: 'session-uuid' })
        expect(invoke).toHaveBeenCalledWith('external_storage_set_execution_session', {
            request: { kind: 'exitDrain', id: 'session-uuid' },
        })
    })

    it('applies a staged receive using its exact expected revision', async () => {
        const applied = { snapshotId: 'snapshot-9', receivedRevision: '12' }
        const invoke = vi.fn(async () => applied)
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await expect(bridge.applyReceived('job-9', '8')).resolves.toBe(applied)
        expect(invoke).toHaveBeenCalledWith('external_storage_apply_received', {
            request: { jobId: 'job-9', expectedRevision: '8' },
        })
    })

    it('forwards conflict paging, source ownership, recheck, and deletion exactly', async () => {
        const invoke = vi.fn(async (command: string) => ({ command }))
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })
        const cursor = { createdAtMs: 7, id: 'conflict-7' }

        await bridge.listConflicts(cursor, 25)
        await bridge.openConflictSource('conflict-1', 'remote')
        await bridge.releaseConflictSource('external:source-token')
        await bridge.recheckConflict('conflict-1')
        await bridge.deleteConflict('conflict-1', true)

        expect(invoke.mock.calls).toEqual([
            ['external_storage_list_conflicts', { cursor, limit: 25 }],
            ['external_storage_open_conflict_source', { id: 'conflict-1', side: 'remote' }],
            ['external_storage_release_conflict_source', { token: 'external:source-token' }],
            ['external_storage_recheck_conflict', { id: 'conflict-1' }],
            ['external_storage_delete_conflict', { id: 'conflict-1', deleteRemotePoint: true }],
        ])
    })

    it('does not prepare a device capture when native already woke a rebound job', async () => {
        const invoke = vi.fn(async (command: string) => {
            if (command === 'external_storage_start_job') return {
                id: 'backup-1',
                connectionId: 'connection',
                kind: 'backup',
                state: 'running',
                phase: 'upload',
            }
            throw new Error(`Unexpected command: ${command}`)
        })
        const bridge = new ExternalStorageBridge({ supported: () => true, invoke })

        await bridge.startJob({ connectionId: 'connection', kind: 'backup' })

        expect(invoke).toHaveBeenCalledTimes(1)
        expect(invoke).not.toHaveBeenCalledWith('external_storage_prepare_device_capture', expect.anything())
    })

})

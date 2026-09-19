import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../../../platform'
import type {
    ExternalCapturePolicy,
    DecimalString,
    ExternalConnectionResult,
    ExternalAuthorizationPending,
    ExternalHistoryPage,
    ExternalHistoryDeletePreparation,
    ExternalJobSummary,
    ExternalReceivedApplicationResult,
    ExternalProviderDescriptor,
    ExternalProviderSecretInput,
    ExternalQuotaSummary,
    ExternalConnectionSettingsMaterial,
    ExternalRetentionPolicy,
    ExternalStorageState,
    ExternalSnapshotExportResult,
    LibrarySyncSelection,
    PendingExternalAuthorization,
    PreparedExternalConnection,
    PrepareExternalConnectionRequest,
    StartExternalJobRequest,
    ExternalConflictSummary,
    ExternalConflictCursor,
    ExternalConflictDeleteResult,
    ExternalConflictPage,
    ExternalConflictSource,
    ExternalExitCapture,
} from './types'

export class ExternalStorageUnsupportedError extends Error {
    readonly code = 'external-storage-unsupported'

    constructor() {
        super('External storage is available in the native app only.')
        this.name = 'ExternalStorageUnsupportedError'
    }
}

export interface ExternalStorageBridgeDependencies {
    supported(): boolean
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
}

const productionDependencies: ExternalStorageBridgeDependencies = {
    supported: () => isTauri,
    invoke: (command, args) => invoke(command, args),
}

export const unsupportedExternalStorageState: ExternalStorageState = {
    supported: false,
    selection: {
        kind: 'none',
        selectionEpoch: '0',
        paused: false,
        decisionRequired: false,
    },
    connections: [],
    jobs: [],
}

export class ExternalStorageBridge {
    constructor(
        private readonly dependencies: ExternalStorageBridgeDependencies = productionDependencies,
    ) {}

    get supported(): boolean {
        return this.dependencies.supported()
    }

    private native<T>(command: string, args?: Record<string, unknown>): Promise<T> {
        if (!this.supported) return Promise.reject(new ExternalStorageUnsupportedError())
        return this.dependencies.invoke(command, args) as Promise<T>
    }

    getState(): Promise<ExternalStorageState> {
        if (!this.supported) return Promise.resolve(unsupportedExternalStorageState)
        return this.native('external_storage_get_state')
    }

    captureExitTarget(): Promise<ExternalExitCapture> {
        return this.native('external_storage_capture_exit_target')
    }

    listProviders(): Promise<ExternalProviderDescriptor[]> {
        return this.native('external_storage_list_providers')
    }

    prepareConnection(request: PrepareExternalConnectionRequest): Promise<PreparedExternalConnection> {
        return this.native('external_storage_prepare_connection', { request })
    }

    commitConnection(
        preparationId: string,
        secret?: ExternalProviderSecretInput,
    ): Promise<ExternalConnectionResult> {
        return this.native('external_storage_commit_connection', {
            request: { preparationId, secret },
        })
    }

    beginAuthorization(
        preparationId: string,
        currentPlatformClientId?: string,
    ): Promise<PendingExternalAuthorization> {
        return this.native('external_storage_begin_authorization', {
            request: {
                preparationId,
                ...(currentPlatformClientId ? { currentPlatformClientId } : {}),
            },
        })
    }

    completeAuthorization(
        authorizationId: string,
        redirectUrl?: string,
        clientSecret?: string,
    ): Promise<ExternalConnectionResult | ExternalAuthorizationPending> {
        return this.native('external_storage_complete_authorization', {
            request: {
                authorizationId,
                ...(redirectUrl ? { redirectUrl } : {}),
                ...(clientSecret ? { clientSecret } : {}),
            },
        })
    }

    cancelAuthorization(authorizationId: string): Promise<void> {
        return this.native('external_storage_cancel_authorization', { authorizationId })
    }

    setCapturePolicy(connectionId: string, policy: ExternalCapturePolicy): Promise<void> {
        return this.native('external_storage_set_capture_policy', { connectionId, policy })
    }

    setRetentionPolicy(connectionId: string, policy: ExternalRetentionPolicy): Promise<void> {
        return this.native('external_storage_set_retention_policy', { connectionId, policy })
    }

    removeConnection(connectionId: string): Promise<void> {
        return this.native('external_storage_remove_connection', { connectionId })
    }

    setSyncTarget(
        connectionId: string | null,
        expectedSelectionEpoch: string,
    ): Promise<LibrarySyncSelection> {
        return this.native('external_storage_set_sync_target', {
            request: { connectionId, expectedSelectionEpoch },
        })
    }

    startJob(request: StartExternalJobRequest, jobId?: string): Promise<ExternalJobSummary> {
        return this.native<ExternalJobSummary>('external_storage_start_job', {
            request, ...(jobId === undefined ? {} : { jobId }),
        })
    }

    cancelJob(jobId: string): Promise<ExternalJobSummary> {
        return this.native('external_storage_cancel_job', { jobId })
    }

    getJob(jobId: string): Promise<ExternalJobSummary> {
        return this.native('external_storage_get_job', { jobId })
    }

    applyReceived(
        jobId: string,
        expectedRevision: DecimalString,
    ): Promise<ExternalReceivedApplicationResult> {
        return this.native('external_storage_apply_received', {
            request: { jobId, expectedRevision },
        })
    }

    setExecutionSession(request: {
        kind: 'foreground' | 'hidden' | 'exitDrain'
        id: string
    }): Promise<void> {
        return this.native('external_storage_set_execution_session', {
            request,
        })
    }

    listHistory(connectionId: string, cursor?: string): Promise<ExternalHistoryPage> {
        return this.native('external_storage_list_history', {
            request: { connectionId, ...(cursor ? { cursor } : {}) },
        })
    }

    prepareHistoryDelete(
        connectionId: string,
        pointId: string,
        pointObservation: string,
    ): Promise<ExternalHistoryDeletePreparation> {
        return this.native('external_storage_prepare_history_delete', {
            request: { connectionId, pointId, pointObservation },
        })
    }

    listConflicts(
        cursor?: ExternalConflictCursor,
        limit?: number,
    ): Promise<ExternalConflictPage> {
        return this.native('external_storage_list_conflicts', {
            ...(cursor ? { cursor } : {}),
            ...(limit === undefined ? {} : { limit }),
        })
    }

    openConflictSource(
        id: string,
        side: 'local' | 'remote',
    ): Promise<{ source: ExternalConflictSource }> {
        return this.native('external_storage_open_conflict_source', { id, side })
    }

    releaseConflictSource(token: string): Promise<void> {
        return this.native('external_storage_release_conflict_source', { token })
    }

    deleteConflict(
        id: string,
        deleteRemotePoint: boolean,
    ): Promise<ExternalConflictDeleteResult> {
        return this.native('external_storage_delete_conflict', {
            id,
            deleteRemotePoint,
        })
    }

    recheckConflict(id: string): Promise<ExternalConflictSummary> {
        return this.native('external_storage_recheck_conflict', { id })
    }

    getQuota(connectionId: string): Promise<ExternalQuotaSummary> {
        return this.native('external_storage_get_quota', { connectionId })
    }

    beginConnectionSettingsExport(connectionId: string): Promise<ExternalConnectionSettingsMaterial> {
        return this.native('external_storage_begin_connection_settings_export', { connectionId })
    }

    saveConnectionSettingsFile(transferId: string): Promise<void> {
        return this.native('external_storage_save_connection_settings_file', { transferId })
    }

    exportSnapshot(connectionId: string, snapshotId: string): Promise<ExternalSnapshotExportResult> {
        return this.native('external_storage_export_snapshot', {
            request: { connectionId, snapshotId },
        })
    }

    prepareConnectionSettingsImport(payload: string, recoveryKey: string): Promise<PreparedExternalConnection> {
        return this.native('external_storage_prepare_connection_settings_import', {
            request: { payload, recoveryKey },
        })
    }
}

let productionBridge: ExternalStorageBridge | undefined

export function getExternalStorageBridge(): ExternalStorageBridge {
    productionBridge ??= new ExternalStorageBridge()
    return productionBridge
}

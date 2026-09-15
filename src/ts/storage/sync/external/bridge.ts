import { invoke } from '@tauri-apps/api/core'
import { isTauri } from '../../../platform'
import {
    prepareExternalDeviceCapture,
    type ExternalDeviceCaptureConsumer,
    type ExternalDeviceCapturePreparation,
} from './deviceSections'
import { requestDeviceMaintenanceRestart as restartForDeviceMaintenance } from '../../deviceBackup/maintenance'
import type {
    DecimalString,
    ExternalConnectionResult,
    ExternalAuthorizationPending,
    ExternalHistoryPage,
    ExternalJobSummary,
    ExternalReceivedApplicationResult,
    ExternalProviderDescriptor,
    ExternalProviderSecretInput,
    ExternalQuotaSummary,
    ExternalRecoveryMaterial,
    ExternalStorageState,
    ExternalSnapshotExportResult,
    LibrarySyncSelection,
    PendingExternalAuthorization,
    PreparedExternalConnection,
    PrepareExternalConnectionRequest,
    StartExternalJobRequest,
    ExternalConflictSummary,
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
        secret: ExternalProviderSecretInput,
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

    async startJob(request: StartExternalJobRequest): Promise<ExternalJobSummary> {
        const job = await this.native<ExternalJobSummary>('external_storage_start_job', { request })
        if (request.kind !== 'backup' || job.phase !== 'device-capture') return job

        const storageState = await this.getState()
        const connection = storageState.connections.find(item => item.id === request.connectionId)
        if (!connection || (!connection.scope.deviceSettings && !connection.scope.devicePlugins))
            return job

        const scopeKey = `${Number(connection.scope.deviceSettings)}:${Number(connection.scope.devicePlugins)}`
        const consumers = storageState.jobs
            .filter(candidate => (
                candidate.kind === 'backup'
                && candidate.state === 'waiting'
                && candidate.phase === 'device-capture'
            ))
            .map(candidate => ({
                job: candidate,
                connection: storageState.connections.find(item => item.id === candidate.connectionId),
            }))
            .filter(({ connection: candidate }) =>
                candidate?.purpose === 'backup' &&
                `${Number(candidate.scope.deviceSettings)}:${Number(candidate.scope.devicePlugins)}` === scopeKey,
            )
            .map(({ job: candidate, connection: owner }) => ({
                jobId: candidate.id,
                purpose: owner!.purpose,
                scope: owner!.scope,
            }))

        if (!consumers.some(consumer => consumer.jobId === job.id)) {
            consumers.push({
                jobId: job.id,
                purpose: connection.purpose,
                scope: connection.scope,
            })
        }
        try {
            await this.prepareDeviceCapture(consumers)
            return job
        } catch (error) {
            await this.cancelJob(job.id).catch(() => undefined)
            throw error
        }
    }

    prepareDeviceCapture(
        consumers: readonly ExternalDeviceCaptureConsumer[],
    ): Promise<ExternalDeviceCapturePreparation | null> {
        return prepareExternalDeviceCapture(
            consumers,
            (command, args) => this.native(command, args),
        )
    }

    requestDeviceMaintenanceRestart(): Promise<never> {
        return restartForDeviceMaintenance(
            (command, args) => this.native(command, args),
        )
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

    listConflicts(connectionId: string): Promise<ExternalConflictSummary[]> {
        return this.native('external_storage_list_conflicts', { connectionId })
    }

    getQuota(connectionId: string): Promise<ExternalQuotaSummary> {
        return this.native('external_storage_get_quota', { connectionId })
    }

    beginRecoveryExport(connectionId: string): Promise<ExternalRecoveryMaterial> {
        return this.native('external_storage_begin_recovery_export', { connectionId })
    }

    saveRecoveryFile(recoveryId: string): Promise<void> {
        return this.native('external_storage_save_recovery_file', { recoveryId })
    }

    exportSnapshot(connectionId: string, snapshotId: string): Promise<ExternalSnapshotExportResult> {
        return this.native('external_storage_export_snapshot', {
            request: { connectionId, snapshotId },
        })
    }

    prepareRecoveryImport(payload: string, code: string): Promise<PreparedExternalConnection> {
        return this.native('external_storage_prepare_recovery_import', {
            request: { payload, code },
        })
    }
}

let productionBridge: ExternalStorageBridge | undefined

export function getExternalStorageBridge(): ExternalStorageBridge {
    productionBridge ??= new ExternalStorageBridge()
    return productionBridge
}

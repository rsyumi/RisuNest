import { invoke } from '@tauri-apps/api/core'

import type { NativeFileJobStatus } from './nativeFileJobs'

export interface NativeFileJobRecoveryDependencies {
    invoke(command: string, args?: Record<string, unknown>): Promise<unknown>
    wait(milliseconds: number): Promise<void>
}

export interface NativeFileJobRecoveryResult {
    pendingRestoreAcknowledgements: string[]
    pendingOfficialPublications: string[]
}

const productionDependencies: NativeFileJobRecoveryDependencies = {
    invoke: (command, args) => args === undefined ? invoke(command) : invoke(command, args),
    wait: (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds)),
}

export function shouldReconcileNativeFileJobs(
    isDesktop: boolean,
    isAndroid: boolean,
    _isAndroidSafEnabled: boolean,
): boolean {
    return isDesktop || isAndroid
}

function isTerminal(status: NativeFileJobStatus): boolean {
    return status.state === 'succeeded'
        || status.state === 'failed'
        || status.state === 'cancelled'
}

async function reconcileRestore(
    initial: NativeFileJobStatus,
    dependencies: NativeFileJobRecoveryDependencies,
): Promise<NativeFileJobStatus> {
    let status = initial
    let finalized = false
    while (!isTerminal(status)) {
        if (
            status.state === 'waitingForInput'
            && status.phase === 'awaiting-activation'
            && !finalized
        ) {
            await dependencies.invoke('native_file_job_finalize', { jobId: status.jobId })
            finalized = true
        }
        else {
            await dependencies.wait(100)
        }
        status = await dependencies.invoke('native_file_job_status', {
            jobId: status.jobId,
        }) as NativeFileJobStatus
    }
    return status
}

async function reconcileExportInBackground(
    initial: NativeFileJobStatus,
    dependencies: NativeFileJobRecoveryDependencies,
): Promise<void> {
    let status = initial
    while (!isTerminal(status)) {
        await dependencies.wait(100)
        status = await dependencies.invoke('native_file_job_status', {
            jobId: status.jobId,
        }) as NativeFileJobStatus
    }
    try {
        if (status.kind === 'export-lossless-backup' && status.result?.handoffPath) {
            await dependencies.invoke('native_lossless_handoff_cleanup', {
                path: status.result.handoffPath,
            })
        }
    }
    finally {
        await dependencies.invoke('native_file_job_forget', { jobId: status.jobId })
    }
}

async function discardContentJob(
    initial: NativeFileJobStatus,
    dependencies: NativeFileJobRecoveryDependencies,
): Promise<void> {
    let status = initial
    if (!isTerminal(status)) {
        await dependencies.invoke('native_file_job_cancel', { jobId: status.jobId })
        do {
            status = await dependencies.invoke('native_file_job_status', {
                jobId: status.jobId,
            }) as NativeFileJobStatus
            if (!isTerminal(status)) await dependencies.wait(100)
        } while (!isTerminal(status))
    }
    await dependencies.invoke('native_file_job_forget', { jobId: status.jobId })
}

function assertNever(value: never): never {
    throw new Error(`Unsupported native file job kind: ${String(value)}`)
}

export async function reconcileNativeFileJobsBeforeBootstrap(
    dependencies: NativeFileJobRecoveryDependencies = productionDependencies,
): Promise<NativeFileJobRecoveryResult> {
    const jobs = await dependencies.invoke('native_file_job_list') as NativeFileJobStatus[]
    const pendingRestoreAcknowledgements: string[] = []
    const pendingOfficialPublications: string[] = []
    for (const job of jobs) {
        const kind = job.kind
        switch (kind) {
            case 'restore-block-risu-save':
            case 'restore-lossless-backup': {
                const terminal = isTerminal(job) ? job : await reconcileRestore(job, dependencies)
                if (terminal.state === 'succeeded') {
                    pendingRestoreAcknowledgements.push(terminal.jobId)
                }
                else {
                    await dependencies.invoke('native_file_job_forget', {
                        jobId: terminal.jobId,
                    })
                }
                break
            }
            case 'export-block-risu-save':
            case 'export-lossless-backup':
            case 'kei-backup-upload':
                void reconcileExportInBackground(job, dependencies).catch((error) => {
                    console.error('Native export reconciliation failed', error)
                })
                break
            case 'prepare-content-import':
                await discardContentJob(job, dependencies)
                break
            case 'official-publication-upload':
                pendingOfficialPublications.push(job.jobId)
                break
            default:
                assertNever(kind)
        }
    }
    return {
        pendingRestoreAcknowledgements,
        pendingOfficialPublications,
    }
}

export async function reconcileNativeRestoresBeforeBootstrap(
    dependencies: NativeFileJobRecoveryDependencies = productionDependencies,
): Promise<string[]> {
    const result = await reconcileNativeFileJobsBeforeBootstrap(dependencies)
    return result.pendingRestoreAcknowledgements
}

export async function acknowledgeRecoveredNativeRestores(
    jobIds: string[],
    dependencies: NativeFileJobRecoveryDependencies = productionDependencies,
): Promise<void> {
    for (const jobId of jobIds) {
        await dependencies.invoke('native_file_job_forget', { jobId })
    }
}

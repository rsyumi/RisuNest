import type { DeviceNativeInvoke } from '../../deviceBackup/nativeSpool'
import {
    requestDeviceMaintenanceRestart,
    type DeviceMaintenanceBootstrap,
} from '../../deviceBackup/maintenance'
import {
    defaultDeviceExportChoices,
    selectedDeviceSections,
} from '../../deviceBackup/selection'
import {
    validateDeviceSectionId,
    type DeviceSectionId,
} from '../../deviceBackup/scopes'
import type {
    ExternalConnectionPurpose,
    ExternalStorageState,
    ExternalStorageScope,
} from './types'

export interface ExternalDeviceCaptureConsumer {
    jobId: string
    purpose: ExternalConnectionPurpose
    scope: ExternalStorageScope
}

export interface ExternalDeviceCaptureRequest {
    consumerIds: string[]
    deviceSections: DeviceSectionId[]
}

export type ExternalDeviceCapturePreparation =
    | { state: 'ready'; captureId: string }
    | { state: 'maintenance-required'; sessionId: string }

export interface ExternalDeviceRestoreSelection {
    library: boolean
    deviceSections: DeviceSectionId[]
}

function checkedId(value: string, label: string): string {
    if (!value || value.length > 1024 || value.includes('\0'))
        throw new Error(`Invalid ${label}`)
    return value
}

function checkedCaptureId(value: string): string {
    if (!/^[0-9a-f]{64}$/.test(value))
        throw new Error('Invalid device capture identifier')
    return value
}

function deviceScopeKey(scope: ExternalStorageScope): string {
    return `${Number(scope.deviceSettings)}:${Number(scope.devicePlugins)}`
}

function requireBackupDeviceScope(
    purpose: ExternalConnectionPurpose,
    scope: ExternalStorageScope,
): void {
    if (!scope.library || !scope.referencedAssets)
        throw new Error('External device backup requires library and referenced assets.')
    if (purpose !== 'backup' && (scope.deviceSettings || scope.devicePlugins))
        throw new Error('A synchronization repository cannot include device-only sections.')
}

/** Resolve only the device data areas named by the immutable repository scope. */
export async function resolveExternalDeviceSections(
    purpose: ExternalConnectionPurpose,
    scope: ExternalStorageScope,
    factory: IDBFactory = indexedDB,
): Promise<DeviceSectionId[]> {
    requireBackupDeviceScope(purpose, scope)
    const sections: DeviceSectionId[] = []
    if (scope.deviceSettings) sections.push('device-settings')
    if (scope.devicePlugins) {
        const choices = await defaultDeviceExportChoices(factory)
        sections.push(...selectedDeviceSections(choices))
    }
    const unique = new Set(sections)
    if (unique.size !== sections.length)
        throw new Error('External device capture contains duplicate sections.')
    return sections
}

/**
 * Build one request for overlapping jobs with the same immutable device scope.
 * Native capture ownership remains one session, while every job receives a
 * durable reference to the resulting shared capture.
 */
export async function buildExternalDeviceCaptureRequest(
    consumers: readonly ExternalDeviceCaptureConsumer[],
    factory: IDBFactory = indexedDB,
): Promise<ExternalDeviceCaptureRequest | null> {
    if (consumers.length === 0) return null
    const first = consumers[0]
    requireBackupDeviceScope(first.purpose, first.scope)
    const scopeKey = deviceScopeKey(first.scope)
    const consumerIds = consumers.map(({ jobId, purpose, scope }) => {
        requireBackupDeviceScope(purpose, scope)
        if (deviceScopeKey(scope) !== scopeKey)
            throw new Error('Overlapping backups have different device scopes.')
        return checkedId(jobId, 'external backup job identifier')
    })
    consumerIds.sort()
    if (new Set(consumerIds).size !== consumerIds.length)
        throw new Error('Duplicate external backup job identifier.')
    const deviceSections = await resolveExternalDeviceSections(
        first.purpose,
        first.scope,
        factory,
    )
    return deviceSections.length === 0 ? null : { consumerIds, deviceSections }
}

/**
 * Start the native shared capture and keep this document alive until the
 * existing maintenance bootstrap replaces it. A ready cached capture requires
 * no restart. Network work starts only after native sealing released the guard.
 */
export async function prepareExternalDeviceCapture(
    consumers: readonly ExternalDeviceCaptureConsumer[],
    invoke: DeviceNativeInvoke,
    options: { factory?: IDBFactory; reload?: () => void } = {},
): Promise<ExternalDeviceCapturePreparation | null> {
    const request = await buildExternalDeviceCaptureRequest(
        consumers,
        options.factory,
    )
    if (!request) return null
    const preparation = await invoke<ExternalDeviceCapturePreparation>(
        'external_storage_prepare_device_capture',
        { request },
    )
    if (preparation.state === 'ready') {
        checkedCaptureId(preparation.captureId)
        return preparation
    }
    if (preparation.state !== 'maintenance-required')
        throw new Error('Native device capture preparation is invalid.')
    checkedId(preparation.sessionId, 'device maintenance session identifier')
    const bootstrap = await invoke<DeviceMaintenanceBootstrap>(
        'native_device_backup_bootstrap',
    )
    if (
        bootstrap.mode !== 'maintenance' ||
        bootstrap.session?.sessionId !== preparation.sessionId ||
        !request.consumerIds.includes(bootstrap.session.jobId) ||
        bootstrap.session.selectedSections.length !== request.deviceSections.length ||
        bootstrap.session.selectedSections.some(
            (section, index) => section !== request.deviceSections[index],
        )
    )
        throw new Error('External device maintenance ownership does not match its backup jobs.')
    return requestDeviceMaintenanceRestart(invoke, options.reload)
}

/** Reconstruct capture groups after a normal bootstrap or renderer restart. */
export function pendingExternalDeviceCaptureGroups(
    state: ExternalStorageState,
): ExternalDeviceCaptureConsumer[][] {
    const groups = new Map<string, ExternalDeviceCaptureConsumer[]>()
    for (const job of state.jobs) {
        if (job.kind !== 'backup' || job.state !== 'waiting' || job.phase !== 'device-capture')
            continue
        const connection = state.connections.find(item => item.id === job.connectionId)
        if (
            !connection ||
            connection.purpose !== 'backup' ||
            (!connection.scope.deviceSettings && !connection.scope.devicePlugins)
        )
            throw new Error('Waiting device capture has no matching backup repository scope.')
        const key = deviceScopeKey(connection.scope)
        const group = groups.get(key) ?? []
        group.push({
            jobId: job.id,
            purpose: connection.purpose,
            scope: connection.scope,
        })
        groups.set(key, group)
    }
    return [...groups.values()]
}

/** Map a verified backup snapshot and the user's restore choices to maintenance sections. */
export function selectExternalDeviceRestoreSections(
    purpose: ExternalConnectionPurpose,
    scope: ExternalStorageScope,
    available: readonly string[],
    restoreAreas: readonly ('library' | 'referencedAssets' | 'deviceSettings' | 'devicePlugins')[],
): ExternalDeviceRestoreSelection {
    requireBackupDeviceScope(purpose, scope)
    const areas = new Set(restoreAreas)
    if (areas.size !== restoreAreas.length)
        throw new Error('Duplicate external restore area.')
    const selected: DeviceSectionId[] = []
    const seen = new Set<string>()
    for (const value of available) {
        validateDeviceSectionId(value)
        if (seen.has(value)) throw new Error('Duplicate device section in external snapshot.')
        if (value === 'device-settings' ? !scope.deviceSettings : !scope.devicePlugins)
            throw new Error('The external snapshot device sections differ from its repository scope.')
        seen.add(value)
        if (
            (value === 'device-settings' && areas.has('deviceSettings')) ||
            (value !== 'device-settings' && areas.has('devicePlugins'))
        )
            selected.push(value)
    }
    if (scope.deviceSettings && !seen.has('device-settings'))
        throw new Error('The external snapshot is missing device settings.')
    if (scope.devicePlugins && (!seen.has('local-storage') || !seen.has('localforage')))
        throw new Error('The external snapshot has an incomplete device plugin scope.')
    if (areas.has('deviceSettings') && !scope.deviceSettings)
        throw new Error('The repository scope does not include device settings.')
    if (areas.has('devicePlugins') && !scope.devicePlugins)
        throw new Error('The repository scope does not include device plugin data.')
    return {
        library: areas.has('library') || areas.has('referencedAssets'),
        deviceSections: selected,
    }
}

import type { AccountReadResult } from '../accountStorage'
import type { Database } from '../database.svelte'
import {
    RevisionConflictError,
    type DataRevision,
} from '../persistentDataStore'
import type { OfficialRevisionPublisher, PinnedPublication } from '../saveCoordinator'
import type { OfficialPullResult } from './officialAccountSnapshot'
import type { OfficialAccountAssetReader } from '../accountAssetAccess'
import type { PluginCompatibilityProfile } from '../../plugins/pluginCompatibility'

interface OfficialBootstrapAdapter extends OfficialRevisionPublisher {
    pull(signal?: AbortSignal): Promise<OfficialPullResult>
}

interface AccountMarkers {
    getItem(key: string): string | null
    setItem(key: string, value: string): void
}

export interface OfficialAccountBootstrapDependencies {
    isTauri: boolean
    local: {
        database: Database
        revision: DataRevision
        profile: PluginCompatibilityProfile
    }
    resolveWorkingSet(revision: DataRevision): Promise<{
        database: Database
        revision: DataRevision
        profile: PluginCompatibilityProfile
    }>
    adapter: OfficialBootstrapAdapter
    readRemoteDatabase(): Promise<AccountReadResult>
    markers: AccountMarkers
    accountMode: { isAccount: boolean }
    configurePublisher(publisher: OfficialRevisionPublisher | null): void
    assetReader: OfficialAccountAssetReader
    configureAssetReader(reader: OfficialAccountAssetReader | null): void
    chooseExistingRemote(): Promise<'pull' | 'push'>
    confirmInitialPush(): Promise<boolean>
    initializeProfile(profile: PluginCompatibilityProfile): void
    installDatabase(database: Database): void
    initializeWorkingSet(database: Database): Promise<void>
    onRemoteError(error: unknown): void
    /** Reports a boot pull skipped because local revisions were never published. */
    onPullSkipped?(input: { conflict: boolean }): void
}

export interface OfficialAccountBootstrapResult {
    database: Database
    revision: DataRevision
    officialEnabled: boolean
}

async function publishPinnedRevision(
    publisher: OfficialRevisionPublisher,
    revision: DataRevision,
): Promise<void> {
    let publication: PinnedPublication | null = null
    let failed = false
    let originalError: unknown
    try {
        publication = await publisher.pin(revision)
        await publication.publish()
    } catch (error) {
        failed = true
        originalError = error
    }
    try {
        await publication?.dispose()
    } catch (error) {
        if (!failed) {
            failed = true
            originalError = error
        }
    }
    if (failed) throw originalError
}

export async function publishOfficialRevisionIfChanged(
    changed: boolean,
    publisher: OfficialRevisionPublisher,
    revision: DataRevision,
): Promise<void> {
    if (!changed) return
    await publishPinnedRevision(publisher, revision)
}

function accountSyncRequested(dependencies: OfficialAccountBootstrapDependencies): boolean {
    if (dependencies.markers.getItem('dosync') === 'avoid') return false
    if (dependencies.markers.getItem('accountst') === 'able') return true
    return dependencies.markers.getItem('dosync') === 'sync'
        || Boolean(dependencies.local.database.account?.useSync)
}

function enableNewAccountMarkers(dependencies: OfficialAccountBootstrapDependencies): void {
    dependencies.markers.setItem('accountst', 'able')
    dependencies.markers.setItem('dosync', 'sync')
    dependencies.markers.setItem(
        'fallbackRisuToken',
        JSON.stringify(dependencies.local.database.account),
    )
}

export async function initializeOfficialAccountBootstrap(
    dependencies: OfficialAccountBootstrapDependencies,
): Promise<OfficialAccountBootstrapResult> {
    dependencies.configurePublisher(null)
    dependencies.configureAssetReader(null)
    dependencies.accountMode.isAccount = false
    const wasEnabled = !dependencies.isTauri && dependencies.markers.getItem('accountst') === 'able'
    let revision = dependencies.local.revision
    let officialEnabled = false

    if (!dependencies.isTauri && accountSyncRequested(dependencies)) {
        try {
            let action: 'pull' | 'push'
            if (wasEnabled) {
                action = 'pull'
            } else {
                const remote = await dependencies.readRemoteDatabase()
                action = remote.kind === 'missing'
                    ? 'push'
                    : await dependencies.chooseExistingRemote()
            }

            if (action === 'pull') {
                const pulled = await dependencies.adapter.pull()
                if (pulled.kind === 'activated') revision = pulled.revision
                if (pulled.kind === 'missing') action = 'push'
                if (pulled.kind === 'kept-local') {
                    dependencies.onPullSkipped?.({ conflict: pulled.conflict })
                }
            }

            if (action === 'push') {
                if (!await dependencies.confirmInitialPush()) {
                    dependencies.markers.setItem('dosync', 'avoid')
                } else {
                    await publishPinnedRevision(dependencies.adapter, revision)
                    officialEnabled = true
                }
            } else {
                officialEnabled = true
            }

            if (officialEnabled) {
                if (!wasEnabled) enableNewAccountMarkers(dependencies)
                dependencies.accountMode.isAccount = true
                dependencies.configurePublisher(dependencies.adapter)
                dependencies.configureAssetReader(dependencies.assetReader)
            }
        } catch (error) {
            if (error instanceof RevisionConflictError) throw error
            dependencies.onRemoteError(error)
        }
    }

    const resolved = revision === dependencies.local.revision
        ? dependencies.local
        : await dependencies.resolveWorkingSet(revision)
    if (resolved.revision !== revision) {
        throw new RevisionConflictError(revision, resolved.revision)
    }
    dependencies.initializeProfile(resolved.profile)
    dependencies.installDatabase(resolved.database)
    await dependencies.initializeWorkingSet(resolved.database)
    return { database: resolved.database, revision, officialEnabled }
}

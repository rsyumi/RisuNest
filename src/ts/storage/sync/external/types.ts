export type DecimalString = `${number}`

export type ExternalProviderId =
    | 'webdav'
    | 's3'
    | 'google_drive'
    | 'onedrive'
    | 'mybox'
    | 'github_releases'
    | 'gitlab_packages'

export type ExternalConnectionPurpose = 'backup' | 'sync'
export type ExternalPublicationStrategy = 'cas' | 'sequential' | 'backup-only'
export type ExternalOpenMode = 'create' | 'existing'

export interface ExternalOAuthProfile {
    projectId: string
    platformClientIds: Record<string, string>
}

export interface ExternalConnectionConfig {
    provider: ExternalProviderId
    profile?: string
    endpoint: string
    accountId: string
    location: Record<string, string>
    oauthProfile?: ExternalOAuthProfile
}

/**
 * What a backup connection captures by default. A synchronization connection
 * has none: what it exchanges is chosen per device under local data. Changing
 * it applies to work started afterwards and never rewrites an existing point.
 */


/**
 * How much of what this device backed up a connection keeps. An automatic
 * backup point is removed only once it is past both limits; kept, conflict and
 * recovery-candidate points stay whatever the limits are.
 */
export interface ExternalRetentionPolicy {
    keepCount: number
    keepDays: number
}

export interface ExternalCapabilities {
    immutableCreate: boolean
    directCompleteRead: boolean
    atomicCreateHead: boolean
    conditionalHeadUpdate: boolean
    stableHeadReplace: boolean
    headReadAfterWrite: boolean
    headRetryControl: boolean
    leaseOperations: boolean
    deleteObjects: boolean
    conditionalGet: boolean
    resumableUpload: boolean
    range: boolean
    snapshotDiscovery: boolean
    maxStoredBytes: number | null
    sdkOverheadBytes: number
    uploadAlignment: number
}

export interface ExternalEndpointConfirmation {
    providerId: ExternalProviderId
    authority: string
    accountHint?: string
    repositoryHint: string
    warnings: string[]
    remoteVerified: boolean
}

export interface PrepareExternalConnectionRequest {
    config: ExternalConnectionConfig
    mode: ExternalOpenMode
    purpose: ExternalConnectionPurpose
    recoveryKey?: string
    acknowledgements: string[]
}

export interface PreparedExternalConnection {
    preparationId: string
    expiresAtMs: DecimalString
    endpoint: ExternalEndpointConfirmation
    capabilities: ExternalCapabilities
    requiresOAuth: boolean
    requiresRecoveryKey: boolean
    requiresPlatformOAuthClient: boolean
    oauthProjectHint?: string
    requiresFolderSelection: boolean
}

export interface ExternalFolderSelection {
    name: string
    accountHint?: string
}

export interface ExternalFolderEntry {
    name: string
    /** Opaque native handle for this selection session. */
    handle: string
}

export interface ExternalFolderPage {
    path: ExternalFolderEntry[]
    folders: ExternalFolderEntry[]
    nextCursor?: string
    selectable: boolean
}

export type ExternalProviderSecretInput =
    | { kind: 'webdav'; password: string }
    | { kind: 's3'; accessKeyId: string; secretAccessKey: string }
    | { kind: 'mybox'; pat: string; expiresAtMs: DecimalString }
    | { kind: 'github'; token: string }
    | { kind: 'gitlab'; token: string }

export interface ExternalConnectionError {
    /** The connection whose sign-in or key clears this error, when an operation hit it reading through a connection
     * other than its own. */
    connectionId?: string
    detail?: string
    httpStatus?: number
    code: string
    message: string
    retryable: boolean
    retryAtMs?: DecimalString
    reason?:
        | 'publication-unknown'
        | 'lineage-changed'
        | 'local-changed'
        | 'remote-changed'
        | 'auth-required'
        | 'key-locked'
    action:
        | 'retry'
        | 'reauthenticate'
        | 'unlock-key'
        | 'check-endpoint'
        | 'free-space'
        | 'wait'
        | 'none'
}

export interface ExternalConnectionSummary {
    id: string
    providerId: ExternalProviderId
    purpose: ExternalConnectionPurpose
    strategy: ExternalPublicationStrategy
    mode: ExternalOpenMode
    displayName: string
    endpoint: ExternalEndpointConfirmation
    /** The policy in force, which is the default until the user changes it. */
    retentionPolicy: ExternalRetentionPolicy
    transferConcurrency: number
    capabilities: ExternalCapabilities
    status: 'ready' | 'paused' | 'reauth-required' | 'key-locked' | 'error'
    automaticBackupPaused: boolean
    lastVerifiedAtMs?: DecimalString
    lastSyncAtMs?: DecimalString
    lastBackupAtMs?: DecimalString
    lastError?: ExternalConnectionError
}

export interface LibrarySyncSelection {
    kind: 'none' | 'server' | 'external'
    connectionId?: string
    selectionEpoch: string
    paused: boolean
}

export interface ExternalExitCapture {
    revision: DecimalString
    libraryEpoch: string
    selection: LibrarySyncSelection
}

export type ExternalJobKind =
    | 'backup'
    | 'restore'
    | 'pin-history'
    | 'delete-history'
    | 'cleanup'
    | 'check-repository'

/** The areas beside the library that a backup can carry. */
export type ExternalRestoreSection = 'hypa' | 'local-plugins' | 'local-settings'

export type ExternalRestoreArea =
    | 'library'
    | 'referencedAssets'
    | ExternalRestoreSection

export interface StartExternalJobRequest {
    connectionId: string
    kind: ExternalJobKind
    snapshotId?: string
    pointId?: string
    pointObservation?: string
    confirmOtherDevice?: boolean
    confirmLastRetained?: boolean
    restoreAreas?: ExternalRestoreArea[]
    targetRevision?: DecimalString
    session?: 'foreground'
    sessionId?: string
    reason?: 'automatic' | 'manual'
}

export interface ExternalJobSummary {
    id: string
    applicationStarted?: boolean
    restoreStopped?: boolean
    /** The app cancelled the job after it kept failing; the user did not. */
    stoppedByApp?: boolean
    reason?: 'automatic' | 'manual'
    targetRevision?: DecimalString
    restoreRequest?: {
        snapshotId: string
        targetRevision: DecimalString
        restoreAreas: ExternalRestoreArea[]
    }
    pinRequest?: { snapshotId: string }
    deleteRequest?: {
        pointId: string
        pointObservation: string
        confirmOtherDevice: boolean
        confirmLastRetained: boolean
    }
    checkRequest?: {
        snapshotId?: string
    }
    connectionId: string
    kind: ExternalJobKind
    state:
        | 'queued'
        | 'running'
        | 'waiting'
        | 'succeeded'
        | 'failed'
        | 'cancelled'
        | 'uncertain'
        | 'conflict'
    phase: string
    /** Which work the four counters below measure. */
    counters?: 'prepared' | 'transferred'
    /** Confirmed objects in this running backup attempt, independent of preparation. */
    transfer?: import('./progress').ExternalTransferSnapshot
    completedBytes: DecimalString
    totalBytes?: DecimalString
    completedItems: DecimalString
    totalItems?: DecimalString
    message?: string
    error?: ExternalConnectionError
    startedAtMs: DecimalString
    updatedAtMs: DecimalString
    result?: {
        snapshotId?: string
        publishedRevision?: DecimalString
        receivedRevision?: DecimalString
        receiveReady?: boolean
        expectedRevision?: DecimalString
        deletedObjects?: DecimalString
        deletedBytes?: DecimalString
        verifiedObjects?: DecimalString
        verifiedBytes?: DecimalString
        damagedObjects?: DecimalString
        stopReason?: string
        reason?: string
    }
}

export interface ExternalReceivedApplicationResult {
    snapshotId: string
    receivedRevision: DecimalString
}

export interface ExternalStorageState {
    supported: boolean
    selection: LibrarySyncSelection
    connections: ExternalConnectionSummary[]
    jobs: ExternalJobSummary[]
    retainedPublications?: Array<{ id: string; connectionId: string; repositoryId: string; revision: DecimalString }>
}

export interface ExternalHistoryItem {
    id: string
    snapshotId: string
    pointId?: string
    pointObservation?: string
    deletable?: boolean
    kind: 'snapshot' | 'backup-point' | 'conflict' | 'recovery-candidate'
    createdAtMs: DecimalString
    logicalRevision: DecimalString
    pinned: boolean
    complete: boolean
    verified: boolean
    /** The areas this entry carries beside the library. */
    includedSections: ExternalRestoreSection[]
    /** Whether these values are the ones this device wrote. */
    sameDevice: boolean
}

export interface ExternalHistoryDeletePreparation {
    sameDevice: boolean
    lastRetained: boolean
}

export interface ExternalHistoryPage {
    items: ExternalHistoryItem[]
    nextCursor?: string
}

export interface ExternalQuotaBucket {
    id: string
    used: DecimalString
    limit: DecimalString
    resetAtMs?: DecimalString
    localEstimate: true
    unit: 'requests'
}

export interface ExternalQuotaSummary {
    connectionId: string
    buckets: ExternalQuotaBucket[]
    storage: {
        providerPhysicalBytes: DecimalString | null
        providerPhysicalKnown: boolean
        locallyUploadedBytesLowerBound: DecimalString
        locallyUploadedObjectCountLowerBound: DecimalString
        locallyUploadedCoverage: 'observed-remote-files'
        observedAtMs: DecimalString | null
        latestReachable?: {
            snapshotId: string
            knownDirectBytes: DecimalString
            knownDirectObjectCount: DecimalString
            complete: boolean
            coverage: 'snapshot-and-catalog-roots'
        }
    }
}

export interface PendingExternalAuthorization {
    authorizationId: string
    authorizationUrl?: string
    expiresAtMs: DecimalString
    state: 'browser-required' | 'native-pending' | 'complete'
}

export interface ExternalRecoveryKeyMaterial {
    key: string
}

export interface ExternalConnectionSettingsMaterial {
    transferId: string
    expiresAtMs: DecimalString
    payload: string
}

export interface ExternalSnapshotExportResult {
    cancelled: boolean
    destination?: string
    sha256?: string
}

export interface ExternalConnectionResult {
    connection: ExternalConnectionSummary
    recovery?: ExternalRecoveryKeyMaterial
}

export interface ExternalAuthorizationPending {
    authorizationPending: true
    callbackRejected?: true
}

export type ExternalAuthorizationOutcome =
    | ExternalConnectionResult
    | ExternalAuthorizationPending
    | { folderSelected: true; folder: ExternalFolderSelection }
    | { folderSelectionRequired: true; selectionId: string; accountHint?: string }
    | { folderSelectionCancelled: true }

export interface ExternalProviderDescriptor {
    id: ExternalProviderId
    displayName: string
    oauth: boolean
    authorizationAvailable: boolean
    strategies: ExternalPublicationStrategy[]
    profiles: string[]
}

export interface ExternalSnapshotExportProgress {
    completedBytes: DecimalString
    totalBytes?: DecimalString
    completedItems: DecimalString
    totalItems?: DecimalString
}

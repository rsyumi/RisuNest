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
export interface ExternalCapturePolicy {
    hypa: boolean
    localPlugins: boolean
    localSettings: boolean
}

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
    capturePolicy?: ExternalCapturePolicy
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
}

export type ExternalProviderSecretInput =
    | { kind: 'webdav'; password: string }
    | { kind: 's3'; accessKeyId: string; secretAccessKey: string }
    | { kind: 'mybox'; pat: string; expiresAtMs: DecimalString }
    | { kind: 'github'; token: string }
    | { kind: 'gitlab'; token: string }

export interface ExternalConnectionError {
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
        | 'resolve-conflict'
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
    capturePolicy?: ExternalCapturePolicy
    /** The policy in force, which is the default until the user changes it. */
    retentionPolicy: ExternalRetentionPolicy
    capabilities: ExternalCapabilities
    status: 'ready' | 'paused' | 'reauth-required' | 'key-locked' | 'error'
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
    decisionRequired: boolean
}

export interface ExternalExitCapture {
    revision: DecimalString
    libraryEpoch: string
    selection: LibrarySyncSelection
}

export type ExternalJobKind =
    | 'backup'
    | 'sync'
    | 'restore'
    | 'pin-history'
    | 'resolve-conflict'

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
    conflictId?: string
    choice?: 'local' | 'remote'
    restoreAreas?: ExternalRestoreArea[]
    targetRevision?: DecimalString
    session?: 'foreground' | 'exitDrain'
    sessionId?: string
    reason?: 'automatic' | 'manual' | 'exitDrain'
}

export interface ExternalJobSummary {
    id: string
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
        conflictId?: string
        publishedRevision?: DecimalString
        receivedRevision?: DecimalString
        receiveReady?: boolean
        expectedRevision?: DecimalString
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
}

export interface ExternalHistoryItem {
    id: string
    kind: 'snapshot' | 'backup-point' | 'conflict' | 'recovery-candidate'
    createdAtMs: DecimalString
    logicalRevision: DecimalString
    storedBytes?: DecimalString
    pinned: boolean
    complete: boolean
    verified: boolean
    /** The areas this entry carries beside the library. */
    includedSections: ExternalRestoreSection[]
    /** Whether these values are the ones this device wrote. */
    sameDevice: boolean
    deviceName?: string
    warning?: string
}

export interface ExternalHistoryPage {
    items: ExternalHistoryItem[]
    nextCursor?: string
}

export interface ExternalConflictSummary {
    id: string
    connectionId: string
    detectedAtMs: DecimalString
    localRevision: DecimalString
    remoteRevision: DecimalString
    localAvailable: boolean
    remoteAvailable: boolean
    remotePointConfirmed: boolean
    resolved: boolean
}

export interface ExternalConflictCursor {
    createdAtMs: number
    id: string
}

export interface ExternalConflictPage {
    conflicts: ExternalConflictSummary[]
    nextCursor?: ExternalConflictCursor
}

export interface ExternalConflictSource {
    type: 'conflictReference'
    token: string
}

export interface ExternalConflictDeleteResult {
    localDeleted: true
    remotePoint: 'deleted' | 'not-found' | 'left-remote'
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
        locallyUploadedCoverage: 'cached-upload-receipts'
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

export interface ExternalRecoveryMaterial {
    recoveryId: string
    expiresAtMs: DecimalString
    code: string
    qrPayload?: string
}

export interface ExternalSnapshotExportResult {
    cancelled: boolean
    destination?: string
    sha256?: string
}

export interface ExternalConnectionResult {
    connection: ExternalConnectionSummary
    recovery?: ExternalRecoveryMaterial
}

export interface ExternalAuthorizationPending {
    authorizationPending: true
    callbackRejected?: true
}

export interface ExternalProviderDescriptor {
    id: ExternalProviderId
    displayName: string
    oauth: boolean
    authorizationAvailable: boolean
    strategies: ExternalPublicationStrategy[]
    profiles: string[]
}

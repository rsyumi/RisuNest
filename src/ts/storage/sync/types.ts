export const officialAccountSnapshotCapability = {
    kind: 'official-account-snapshot',
    database: 'full-risu-save',
    assets: 'read-write-replacement-key',
    coldStorage: 'separate-keys',
    remoteRevision: 'opaque-session-cache',
} as const

export const driveSnapshotCapability = {
    kind: 'drive-snapshot',
    operations: ['create', 'list', 'restore'] as const,
    automaticMerge: false,
} as const

export const manifestDeltaCapability = {
    kind: 'manifest-delta',
    generationCas: true,
    changedRecords: true,
    contentHashes: true,
    tombstones: true,
} as const

export type SyncCapability =
    | typeof officialAccountSnapshotCapability
    | typeof driveSnapshotCapability
    | typeof manifestDeltaCapability

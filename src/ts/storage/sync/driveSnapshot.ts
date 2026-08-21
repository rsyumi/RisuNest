import { driveSnapshotCapability } from './types'

export interface DriveSnapshotInfo {
    id: string
    createdAt: number
    label: string
}

export interface DriveSnapshotOperations {
    createSnapshot(): Promise<void>
    listSnapshots(): Promise<readonly DriveSnapshotInfo[]>
    restoreSnapshot(id: string): Promise<void>
}

export interface DriveSnapshotAdapter extends DriveSnapshotOperations {
    readonly capability: typeof driveSnapshotCapability
}

export function createDriveSnapshotAdapter(
    operations: DriveSnapshotOperations,
): DriveSnapshotAdapter {
    return {
        capability: driveSnapshotCapability,
        createSnapshot: () => operations.createSnapshot(),
        listSnapshots: () => operations.listSnapshots(),
        restoreSnapshot: (id) => operations.restoreSnapshot(id),
    }
}

type DeviceSyncUriListener = (uri: string) => void

const deviceSyncListeners = new Set<DeviceSyncUriListener>()
let pendingDeviceSyncUri: string | null = null

export function publishDeviceSyncUri(uri: string): void {
    if (deviceSyncListeners.size === 0) {
        pendingDeviceSyncUri = uri
        return
    }
    for (const listener of deviceSyncListeners) listener(uri)
}

export function receiveDeviceSyncUri(
    uri: string,
    openSettings: (menuIndex: number) => void,
): void {
    publishDeviceSyncUri(uri)
    openSettings(18)
}

export function consumePendingDeviceSyncUri(): string | null {
    const uri = pendingDeviceSyncUri
    pendingDeviceSyncUri = null
    return uri
}

export function subscribeDeviceSyncUri(listener: DeviceSyncUriListener): () => void {
    deviceSyncListeners.add(listener)
    return () => deviceSyncListeners.delete(listener)
}

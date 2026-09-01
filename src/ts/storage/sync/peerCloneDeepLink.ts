type PeerCloneUriListener = (uri: string) => void

const listeners = new Set<PeerCloneUriListener>()
let pendingUri: string | null = null
const deviceSyncListeners = new Set<PeerCloneUriListener>()
let pendingDeviceSyncUri: string | null = null

export function publishPeerCloneUri(uri: string): void {
    if (isDeviceSyncUri(uri)) {
        publishDeviceSyncUri(uri)
        return
    }
    if (listeners.size === 0) {
        pendingUri = uri
        return
    }
    for (const listener of listeners) listener(uri)
}

export function consumePendingPeerCloneUri(): string | null {
    const uri = pendingUri
    pendingUri = null
    return uri
}

export function subscribePeerCloneUri(listener: PeerCloneUriListener): () => void {
    listeners.add(listener)
    return () => listeners.delete(listener)
}

function isDeviceSyncUri(uri: string): boolean {
    try {
        const value = new URL(uri)
        return value.protocol === 'risuailocal:' && value.hostname === 'peer-clone' && value.pathname === '/v2'
    } catch {
        return false
    }
}

export function publishDeviceSyncUri(uri: string): void {
    if (deviceSyncListeners.size === 0) {
        pendingDeviceSyncUri = uri
        return
    }
    for (const listener of deviceSyncListeners) listener(uri)
}

export function consumePendingDeviceSyncUri(): string | null {
    const uri = pendingDeviceSyncUri
    pendingDeviceSyncUri = null
    return uri
}

export function subscribeDeviceSyncUri(listener: PeerCloneUriListener): () => void {
    deviceSyncListeners.add(listener)
    return () => deviceSyncListeners.delete(listener)
}

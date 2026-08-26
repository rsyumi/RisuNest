type PeerCloneUriListener = (uri: string) => void

const listeners = new Set<PeerCloneUriListener>()
let pendingUri: string | null = null

export function publishPeerCloneUri(uri: string): void {
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

export interface RisuLocalUrlHandlers {
    onRealm(id: string): void
    onPeerClone(uri: string): void
    onDeviceSync?(uri: string): void
}

export function dispatchRisuLocalUrl(value: string, handlers: RisuLocalUrlHandlers): boolean {
    let url: URL
    try {
        url = new URL(value)
    } catch {
        return false
    }
    if (url.protocol !== 'risuailocal:') return false
    const segments = url.pathname.split('/').filter(Boolean)
    const realmId = url.hostname === 'realm' && segments.length === 1
        ? segments[0]
        : segments.at(-2) === 'realm'
            ? segments.at(-1)
            : undefined
    if (realmId) {
        try {
            handlers.onRealm(decodeURIComponent(realmId))
        } catch {
            return false
        }
        return true
    }
    if (url.hostname === 'peer-clone' && url.pathname === '/v1') {
        handlers.onPeerClone(value)
        return true
    }
    if (url.hostname === 'peer-clone' && url.pathname === '/v2') {
        if (!handlers.onDeviceSync) return false
        handlers.onDeviceSync(value)
        return true
    }
    return false
}

import { parseServerSyncDeepLink } from './storage/sync/serverSyncDeepLink'

export interface RisuLocalUrlHandlers {
    onRealm(id: string): void
    onServerSync(uri: string): void
}

export function dispatchRisuLocalUrl(value: string, handlers: RisuLocalUrlHandlers): boolean {
    let url: URL
    try {
        url = new URL(value)
    } catch {
        return false
    }
    if (url.protocol !== 'risunestlocal:') return false
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
    if (parseServerSyncDeepLink(value)) {
        handlers.onServerSync(value)
        return true
    }
    return false
}

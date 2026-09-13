import { parseRisuLocalUrl } from './risuLocalUrl'
import { parseServerSyncDeepLink } from './storage/sync/serverSyncDeepLink'
import { parseServerRegistration } from './storage/sync/serverSyncRegistration'

export interface RisuLocalUrlHandlers {
    onRealm(id: string): void
    onServerSync(uri: string): void
    onServerRegistration?(uri: string): void
}

export function dispatchRisuLocalUrl(
    value: string,
    handlers: RisuLocalUrlHandlers,
): boolean {
    const url = parseRisuLocalUrl(value)
    if (!url) return false
    if (url.hostname === 'sync-server' && url.pathname === '/register') {
        try {
            parseServerRegistration(value)
            if (!handlers.onServerRegistration) return false
            handlers.onServerRegistration(value)
            return true
        } catch {
            return false
        }
    }
    const segments = url.pathname.split('/').filter(Boolean)
    const realmId =
        url.hostname === 'realm' && segments.length === 1
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

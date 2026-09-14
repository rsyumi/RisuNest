// The single definition of the RisuRealm paths. The vitest guard and the CDP
// runners read the same list.
//
// Realm cannot be separated by host. Besides Realm, `sv.risuai.xyz` also serves
// account backup keys (`/cryptokey`), the Drive OAuth callback (`/drive`), the
// embedding-model CDN (`/transformers/`), account login (`/hub/login`) and
// account storage (`/hub/account/`). Blocking the whole host would take account
// sync and model downloads down with it, so match by path.
//
// `realm.risuai.net` is Realm-only, so block all of it.

const REALM_API_HOSTS = ['sv.risuai.xyz', 'nightly.sv.risuai.xyz']
const REALM_SITE_HOSTS = ['realm.risuai.net']

// Only paths starting with these prefixes are Realm. Anything not listed belongs
// to the account or model side and is left alone.
//
// `/rs/` is the one dual-use prefix. Account storage assets and Realm-shared
// assets (the ones a lightning import verifies by hash) live in the same
// content-addressed store. Third-party assets can be mixed in, so it counts as
// Realm. The cost is that agent sessions also lose account-mode asset display,
// which has no practical effect because agents do not log in. Every place in
// product code that builds an `/rs/` URL uses `realmHubURL`, so the vite swap and
// this list block the same things.
const REALM_PATH_PREFIXES = [
    '/realm/',
    '/hub/info/',
    '/hub/realm/',
    '/hub/report',
    '/hub/remove',
    '/resource/',
    '/rs/',
]

/** Wildcard patterns that can be passed straight to CDP `Network.setBlockedURLs`. */
export const REALM_BLOCKED_URL_PATTERNS = [
    ...REALM_SITE_HOSTS.map((host) => `*://${host}/*`),
    ...REALM_API_HOSTS.flatMap((host) =>
        REALM_PATH_PREFIXES.map((prefix) => `*://${host}${prefix}*`),
    ),
]

/**
 * Reports whether the given URL fetches RisuRealm content. Values that cannot be
 * parsed count as non-Realm (relative paths, blob:, data: and so on).
 */
export function isRealmUrl(input) {
    let parsed
    try {
        parsed = new URL(String(input), 'http://localhost')
    } catch {
        return false
    }
    if (REALM_SITE_HOSTS.includes(parsed.hostname)) {
        return true
    }
    if (!REALM_API_HOSTS.includes(parsed.hostname)) {
        return false
    }
    return REALM_PATH_PREFIXES.some((prefix) => parsed.pathname.startsWith(prefix))
}

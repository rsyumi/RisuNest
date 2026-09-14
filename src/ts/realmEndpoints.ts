// Shared entry point for RisuRealm endpoints.
//
// The hub also serves account backup keys, Drive OAuth, embedding models and
// account login. Only Realm paths use these constants so blocking those paths
// does not block unrelated services on the same host.
//
// Vite replaces this module with realmEndpoints.blocked.ts in agent mode.
// Keep it limited to constants so the replacement has the same behavior.
export const REALM_HUB_URL = 'https://sv.risuai.xyz'
export const REALM_NIGHTLY_HUB_URL = 'https://nightly.sv.risuai.xyz'
export const REALM_SITE_URL = 'https://realm.risuai.net'
// The Node server forwards Realm requests under this prefix to the hub.
export const REALM_NODE_PROXY_BASE = '/hub-proxy'

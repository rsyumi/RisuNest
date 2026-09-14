// Agent-mode replacement for realmEndpoints.ts.
//
// Vite substitutes this module only in agent mode. Default builds do not import it.
//
// The reserved .invalid domain prevents resolution and makes blocked requests
// recognizable in errors and logs.
//
// Account login, Drive backup and embedding models use separate endpoints.
export const REALM_HUB_URL = 'https://realm-blocked.invalid'
export const REALM_NIGHTLY_HUB_URL = 'https://realm-blocked.invalid'
export const REALM_SITE_URL = 'https://realm-blocked.invalid'
// Use an absolute address to avoid the Node server's relative hub proxy route.
export const REALM_NODE_PROXY_BASE = 'https://realm-blocked.invalid'

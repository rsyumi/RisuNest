// The single place that holds RisuNest's legal document links.
//
// The two groups are kept apart. `RISUNEST_*` are the documents this project
// provides; `RISU_SERVICE_*` belong to the services the RisuAI maintainers run
// (accounts, RisuRealm). The former is accepted once at startup, the latter only
// where the app actually contacts those services. Neither acceptance stands in
// for the other.
//
// These values are the same in every build mode, so they are not environment
// variables. As env vars, a single missing entry in one `.env.*` would ship an
// empty link without failing the build.
export const RISUNEST_TERMS_URL = 'https://github.com/rsyumi/RisuNest/blob/main/legal/TERMS.md'
export const RISUNEST_PRIVACY_URL = 'https://github.com/rsyumi/RisuNest/blob/main/legal/PRIVACY.md'

export const RISU_SERVICE_TERMS_URL = 'https://account.sionyw.com/terms'
export const RISU_SERVICE_PRIVACY_URL = 'https://account.sionyw.com/privacy'

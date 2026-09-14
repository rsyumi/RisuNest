import { vi } from 'vitest'
import rfdc from 'rfdc'
import { isRealmUrl } from './scripts/realmBlocklist.mjs'

// Suppress warning
vi.mock(import('katex'), () => ({}))

// RisuRealm serves third-party content this project does not control. A test that
// fetches it leaks that content into failure output and snapshots. Throw instead
// of blocking quietly, so the code that touched Realm is visible right away.
// Account and model paths on the same host stay reachable (scripts/realmBlocklist.mjs
// decides which is which).
//
// Two places enforce it.
// 1. A globalThis.fetch wrapper. It throws synchronously, so the caller's stack
//    survives. Assign it directly rather than through vi.stubGlobal: with
//    vi.stubGlobal, a test that stubs fetch and then calls vi.unstubAllGlobals()
//    falls back to the original fetch instead of this wrapper and the guard is
//    gone. A direct assignment makes this wrapper the "original" to fall back to.
// 2. A happy-dom fetch interceptor. iframe and script loading and XHR use
//    happy-dom's internal Fetch rather than globalThis.fetch, so cut those with
//    the same check.
// Both log the URL with console.error before throwing, so it still shows up when
// product code swallows the error in a try/catch.
function realmAccessError(url: unknown): Error {
  const message =
    `테스트에서 RisuRealm에 접근했습니다: ${String(url)}\n` +
    '로컬 합성 fixture를 쓰거나 해당 모듈을 모킹하세요.'
  console.error(message)
  return new Error(message)
}

const realFetch = globalThis.fetch
const guardedFetch: typeof fetch = (input, init) => {
  const url = typeof input === 'object' && input !== null && 'url' in input ? input.url : input
  if (isRealmUrl(url)) {
    throw realmAccessError(url)
  }
  return realFetch(input, init)
}
globalThis.fetch = guardedFetch

type RealmGuardRequest = { request: { url: string } }
const happyDOM = (globalThis as { happyDOM?: { settings?: { fetch?: { interceptor?: unknown } } } }).happyDOM
if (happyDOM?.settings?.fetch) {
  happyDOM.settings.fetch.interceptor = {
    beforeAsyncRequest: async ({ request }: RealmGuardRequest) => {
      if (isRealmUrl(request.url)) throw realmAccessError(request.url)
    },
    beforeSyncRequest: ({ request }: RealmGuardRequest) => {
      if (isRealmUrl(request.url)) throw realmAccessError(request.url)
    },
  }
}

// Mirror the production safeStructuredClone from src/ts/polyfill.ts (structuredClone
// with an rfdc fallback) instead of importing polyfill.ts, which would pull its
// drag-drop/stream/global side effects into every suite. A JSON round-trip must not
// be used here: it silently changes clone semantics (drops undefined/function
// properties, stringifies Dates, mangles typed arrays, throws on cycles) in the
// data-loss-critical storage suites.
const rfdcClone = rfdc({
  circles: false,
})
vi.stubGlobal('safeStructuredClone', <T>(data: T): T => {
  try {
    return structuredClone(data)
  } catch (error) {
    return rfdcClone(data)
  }
})

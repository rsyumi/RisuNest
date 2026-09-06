import { vi } from 'vitest'
import rfdc from 'rfdc'
import { isRealmUrl } from './scripts/realmBlocklist.mjs'

// Suppress warning
vi.mock(import('katex'), () => ({}))

// RisuRealm은 우리가 관리하지 않는 제3자 콘텐츠다. 테스트가 이걸 받아오면 그 내용이
// 실패 출력과 스냅샷을 타고 그대로 흘러 나온다. 조용히 막지 않고 던져서, 어떤 코드가
// Realm을 건드렸는지 바로 드러나게 한다. 같은 호스트의 계정/모델 경로는 통과시킨다
// (판별 기준은 scripts/realmBlocklist.mjs).
//
// 두 지점에서 막는다.
// 1. globalThis.fetch 래퍼. 동기적으로 던지므로 호출자의 스택이 그대로 보인다.
//    vi.stubGlobal이 아니라 직접 대입한다. vi.stubGlobal로 깔면 어떤 테스트가 fetch를
//    stub했다가 vi.unstubAllGlobals()로 되돌릴 때 이 래퍼가 아니라 원본 fetch로
//    돌아가 가드가 사라진다. 직접 대입해 두면 되돌릴 "원본"이 이 래퍼가 된다.
// 2. happy-dom fetch 인터셉터. iframe과 script 로딩, XHR은 globalThis.fetch를 거치지
//    않고 happy-dom 내부 Fetch를 쓰므로 그쪽도 같은 판별로 끊는다.
// 어느 쪽이든 던지기 전에 console.error로 URL을 남긴다. 제품 코드가 try/catch로
// 삼켜도 출력에는 남게 하기 위해서다.
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

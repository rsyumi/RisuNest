// RisuRealm 경로 하나만 정의하는 곳. vitest 가드와 CDP 러너가 같은 목록을 쓴다.
//
// 호스트 단위로는 Realm을 떼어낼 수 없다. `sv.risuai.xyz`는 Realm 외에 계정 백업 키
// (`/cryptokey`), Drive OAuth 콜백(`/drive`), 임베딩 모델 CDN(`/transformers/`),
// 계정 로그인(`/hub/login`), 계정 저장(`/hub/account/`)도 서빙한다. 호스트를 통째로
// 막으면 계정 동기화와 모델 다운로드가 같이 죽으므로 경로로 구분한다.
//
// `realm.risuai.net`은 Realm 전용이라 전부 막는다.

const REALM_API_HOSTS = ['sv.risuai.xyz', 'nightly.sv.risuai.xyz']
const REALM_SITE_HOSTS = ['realm.risuai.net']

// 이 접두사로 시작하는 경로만 Realm이다. 목록에 없는 경로는 계정이나 모델 쪽이므로
// 건드리지 않는다.
//
// `/rs/`만 이중 용도다. 계정 스토리지 자산과 Realm 공유 자산(lightning import가
// 해시로 확인하는 자산)이 같은 콘텐츠 주소 저장소에 산다. 제3자 자산이 섞일 수
// 있으므로 Realm으로 분류한다. 대가로 agent 세션에서는 계정 모드의 자산 표시도
// 끊기는데, 에이전트는 로그인하지 않으므로 실사용 영향은 없다. 제품 코드에서
// `/rs/` URL을 만드는 곳은 모두 `realmHubURL`을 써서 vite 교체와 이 목록이 같은
// 것을 막게 한다.
const REALM_PATH_PREFIXES = [
    '/realm/',
    '/hub/info/',
    '/hub/realm/',
    '/hub/report',
    '/hub/remove',
    '/resource/',
    '/rs/',
]

/** CDP `Network.setBlockedURLs`에 그대로 넘길 수 있는 와일드카드 패턴. */
export const REALM_BLOCKED_URL_PATTERNS = [
    ...REALM_SITE_HOSTS.map((host) => `*://${host}/*`),
    ...REALM_API_HOSTS.flatMap((host) =>
        REALM_PATH_PREFIXES.map((prefix) => `*://${host}${prefix}*`),
    ),
]

/**
 * 주어진 URL이 RisuRealm 콘텐츠를 가져오는지 판별한다. 파싱할 수 없는 값은 Realm이
 * 아닌 것으로 본다(상대 경로, blob:, data: 등).
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

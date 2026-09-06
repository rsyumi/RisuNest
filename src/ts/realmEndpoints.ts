// RisuRealm 엔드포인트를 모아 두는 단일 지점.
//
// `sv.risuai.xyz`는 Realm 전용 호스트가 아니다. 같은 호스트가 계정 백업 키
// (`/cryptokey`), Google Drive OAuth 콜백(`/drive`), 임베딩 모델 CDN
// (`/transformers/`), 계정 로그인(`/hub/login`)도 서빙한다. 그래서 호스트 단위로는
// Realm만 떼어낼 수 없고, Realm 경로를 쓰는 쪽만 이 모듈을 거치게 한다.
//
// `--mode agent`로 빌드하거나 서브하면 vite가 이 모듈 전체를
// `realmEndpoints.blocked.ts`로 교체한다(vite.config.ts의 resolveId 플러그인 참고).
// 그래서 이 파일에는 상수만 두고 조건 분기나 import를 넣지 않는다. 분기가 들어가면
// 교체본과 실제 동작이 갈라진다.
export const REALM_HUB_URL = 'https://sv.risuai.xyz'
export const REALM_NIGHTLY_HUB_URL = 'https://nightly.sv.risuai.xyz'
export const REALM_SITE_URL = 'https://realm.risuai.net'
// node 서버 모드에서 Realm 요청이 붙는 접두사. 서버의 `/hub-proxy/*`가 hub로 중계한다.
export const REALM_NODE_PROXY_BASE = '/hub-proxy'

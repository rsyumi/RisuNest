// `realmEndpoints.ts`의 agent 모드 교체본.
//
// vite가 `--mode agent`에서만 resolveId 플러그인으로 이 파일을 끼워 넣는다. 기본 빌드
// 경로에서는 아무도 import하지 않으므로 번들에 들어가지 않는다.
//
// `.invalid`는 RFC 2606이 해석 불가로 예약한 TLD라 DNS 단계에서 즉시 실패한다.
// 요청이 기기 밖으로 나가지 않고, 실패 화면이나 로그에 이름이 그대로 보여서
// 차단된 상태임을 바로 알 수 있다.
//
// 계정 로그인, Drive 백업, 임베딩 모델은 이 모듈을 거치지 않으므로 그대로 동작한다.
export const REALM_HUB_URL = 'https://realm-blocked.invalid'
export const REALM_NIGHTLY_HUB_URL = 'https://realm-blocked.invalid'
export const REALM_SITE_URL = 'https://realm-blocked.invalid'
// node 서버 모드에서도 상대 경로 `/hub-proxy` 대신 절대 주소를 넣어, 서버의
// hub 중계를 타지 않고 DNS에서 끊기게 한다.
export const REALM_NODE_PROXY_BASE = 'https://realm-blocked.invalid'

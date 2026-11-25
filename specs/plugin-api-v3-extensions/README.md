# RisuAI Plugin API v3 Extensions 규약

> 타입 정의: [`risuai-v3-extensions.d.ts`](./risuai-v3-extensions.d.ts)

이 문서는 RisuAI Plugin API v3.0(`//@api 3.0`) 위에 얹는 **공용 추가형 확장 규약(Plugin API v3 Extensions)**을 정의합니다. 호스트의 내부 저장 구조(단일 DB, SQLite, 서버 파일, 분기 그래프 등)는 규정하지 않으며, 호스트와 플러그인 사이에서 **외부로 관찰되는 동작·타입·오류·권한·브리지 계약**만 규정합니다.

정확한 TypeScript 타입과 메서드 시그니처는 순수 모듈 타입 파일인 [`risuai-v3-extensions.d.ts`](./risuai-v3-extensions.d.ts)(`export interface RisuaiPluginExtensionsAPI`)가 기준이며, 이 문서는 타입만으로 드러나지 않는 동작 규칙·불변 조건·경계 사례를 규정합니다. [`risuai-v3-extensions.d.ts`](./risuai-v3-extensions.d.ts)는 전역 심볼을 선언하지 않으므로 기존 호스트의 `risuai.d.ts`와 함께 두어도 충돌하거나 오버로드를 가리지 않습니다.

---

## 1. 규범 용어와 기본 원칙

이 문서에서 **MUST(반드시)**, **MUST NOT(절대 금지)**, **SHOULD(권장)**, **MAY(선택)**는 RFC 2119 의미로 사용합니다.

### 1.1 추가형 계약 3원칙

1. **기존 V3 비파괴**: 문서화된 V3.0 메서드의 이름·매개변수·반환 동작은 삭제하거나 깨뜨리지 않습니다(MUST NOT). 기존 `//@api 3.0` 플러그인은 수정 없이 그대로 동작해야 합니다.
2. **안전한 확장만 허용**: 새 기능은 다음 형태로만 추가합니다.
   - `risuai` 최상위 중첩 네임스페이스 (`risuai.selection`, `risuai.characters`, `risuai.chats`, `risuai.messages`, `risuai.generation`, `risuai.lorebookEntries`, `risuai.modules`, `risuai.personas`, `risuai.settings`, `risuai.chatView`, `risuai.mcp`)
   - 기존 옵션 객체의 선택 필드 확장 (`runLLMModel`, `nativeFetch`, `registerButton` 등)
   - 기존 등록 메서드의 반환값을 데이터 기반 `{ id }` 핸들로 확장 (`handles`)
   - 기존 콜백의 추가 인자는 **옵트인(`{ requestContext: true }`)** 시에만 전달 (`requestContext`)
3. **기능군 단위 독립 채택**: 호스트는 원하는 기능군부터 순서에 관계없이 구현하고 `getRuntimeInfo().features`로 광고합니다. 확장 기능을 하나도 구현하지 않은 호스트도 유효한 V3 호스트입니다.

### 1.2 버전 정책 (`//@api 3.0` 유지)

- 플러그인 헤더는 **`//@api 3.0`**을 유지하고, `getRuntimeInfo().apiVersion`도 **`'3.0'`**을 유지합니다.
- 이 규약은 **`'v3.1'` 같은 묶음 버전 피처 키를 두지 않습니다.** 플러그인은 자신이 실제로 사용하는 개별 기능 키(예: `chats`, `events`)만 확인해야 합니다.

### 1.3 기존 V3 호환 시 관찰 가능한 미세 차이

문서화된 V3 계약은 유지되지만, 확장을 채택한 호스트에서 기존 플러그인이 아래 차이를 관찰할 수 있습니다.

- `runLLMModel`에 기존에는 무시되던 `maxTokens`·`temperature` 등을 이미 넘기고 있던 플러그인은 `modelOptions` 지원 호스트에서 해당 값이 실제로 적용됩니다.
- `handles` 지원 호스트에서는 기존에 `undefined`를 반환하던 등록 메서드(`addProvider` 등)가 `{ id }` 핸들 객체를 반환합니다.
- `handles` 지원 호스트에서는 다른 플러그인이 등록한 ID를 `unregister*`로 해제하는 동작이 차단됩니다.
- `storage.pluginStorage.bytes === true`인 호스트에서는 `pluginStorage`에 저장한 최상위 `Uint8Array`/`ArrayBuffer`가 숫자 키 객체 대신 바이트 그대로 복원됩니다.

---

## 2. 런타임 기능 탐색 (`getRuntimeInfo()`)

타입 정의: [`PluginRuntimeInfo`](./risuai-v3-extensions.d.ts)

호스트는 기존 `getRuntimeInfo()` 반환 객체(`apiVersion`, `platform`, `saveMethod`)에 다음 선택 필드를 추가합니다.

- **`features?: string[]`**: 호스트가 규약대로 구현한 확장 기능군 및 후기 V3 기능 키 목록.
- **`storage?: PluginStorageCapabilities`**: `pluginStorage`와 `getLocalPluginStorage()`의 격리 범위(`scope`), 저장 위치(`location`), 바이트/구조화 값 보존 여부(`bytes`, `structured`).
- **`app?: { id, version }`**: 로깅·진단용 선택 메타데이터.

### 2.1 기능 키 목록

**Common**은 대부분의 호스트가 점진적으로 채택할 것을 권장하는 범용 기능군이고, **Optional**은 호스트 아키텍처·저장소·웹 플랫폼 제약에 따라 선택적으로 채택하는 기능군입니다. 두 분류 모두 어떤 순서로든 독립적으로 구현·광고할 수 있습니다.

| 기능 키 | 분류 | 요약 |
| --- | --- | --- |
| `chats` | Common | `selection.get`, `characters`(`getOrder`/`setOrder` 포함), `chats`(`getContext`, `getVariables`/`updateVariables`), `messages` |
| `library` | Common | `lorebookEntries`, `modules`, `personas` |
| `events` | Common | `selection.onChanged`, `generation.onStarted`/`onEnded`, `messages.onResponseReceived` |
| `handles` | Common | 등록/이벤트 메서드의 데이터 기반 `{ id }` 핸들, 소유권 검사, 권한 거부 시 `null` 반환 |
| `requestContext` | Common | `addRisuReplacer`·`registerBodyIntercepter`·`addProvider`의 옵트인 `{ requestContext: true }` 요청 문맥 |
| `modelOptions` | Common | `runLLMModel`의 `maxTokens`, `temperature`, `responseSchema`, `signal`, `tag` |
| `fetchOptions` | Common | `nativeFetch`의 Fetch 허용 HTTP 메서드, 바디/헤더 타입 확장, 옵션 및 응답 `url`/`redirected` 복원 |
| `fetchRedirect` | Optional | `nativeFetch`의 `redirect: 'follow' \| 'manual' \| 'error'` 제어 |
| `storageUpdate` | Optional | `pluginStorage.update(key, updater)` 충돌 감지 갱신 |
| `settings` | Optional | `settings` (허용 목록 16개 키 `get`/`update`) |
| `changes` | Optional | `pluginStorage` 및 컬렉션의 키/ID 변경 알림(`onChanged`), 영구 삭제 알림(`chats.onDeleted`, `characters.onDeleted`), 느슨한 메시지 제거 알림(`messages.onDeleted`) |
| `inlayMedia` | Optional | `createInlay` 오디오·비디오 확장(기존 `Promise<string \| null>` 유지) 및 `deleteInlay(id)` |
| `messageButtons` | Optional | `registerButton`의 `location: 'message'` 및 `roles` 필터 |
| `messageSlots` | Optional | `chatView` 행 렌더 이벤트 및 메시지 위/아래 Shadow DOM 슬롯 |
| `workers` | Optional | 플러그인 iframe CSP에 `worker-src blob:` 허용 (전용 Web Worker) |
| `mcp` | Optional | `risuai.mcp` (`list`, `listTools`, `callTool`, `mcpAccess` 권한) |
| `clipboardWrite` | Optional | 플러그인 iframe `allow="clipboard-write"` 위임 (보안 컨텍스트 한정) |

후기 업스트림 V3 기능(`createInlay`, `parseRisuChat`, `getRemovedNodes`, `dbWrite`)도 호스트가 지원하면 동일한 이름의 키를 `features`에 포함할 수 있습니다(MAY). `pluginStorage` 바이트 지원은 별도 피처 키를 두지 않고 `storage.pluginStorage.bytes`로만 표현합니다.

### 2.2 탐색 규칙

- 플러그인은 `app.id`가 아니라 **`features`와 `storage`를 기준으로 분기**해야 합니다(SHOULD).
- 플러그인은 알지 못하는 `features` 키를 무시해야 하며(MUST), `features`가 없으면 어떤 확장 기능도 가정해서는 안 됩니다.
- `window.risuai` 루트가 `Proxy`이므로 `typeof risuai.someMethod === 'function'`은 지원 여부를 증명하지 않습니다. 쓰기 부작용이 있는 메서드(`createInlay` 등)를 `try/catch`로 호출해 존재 여부를 탐색해서는 안 됩니다(MUST NOT).
- 호스트는 `features`에 어떤 키를 포함했다면, 해당 기능군의 본문 규약과 [7장 적합성 기준](#7-적합성-기준-conformance-checklist)의 해당 시나리오를 모두 만족해야 합니다(MUST).

---

## 3. 공통 규약 (모든 신규 API 적용)

### 3.1 식별자, 레코드, 필드 프로젝션

- **저장 필드명 유지**: 레코드는 업스트림 RisuAI의 저장 필드명(`chaId`, `name`, `desc`, `role`, `data`, `chatId` 등)을 그대로 사용합니다.
- **분리된 중첩 컬렉션·맵 제외**:
  - `PluginCharacterRecord`는 `chats`와 `globalLore`를 포함하지 않습니다(MUST NOT).
  - `PluginChatRecord`는 `message`, `localLore`, `scriptstate`를 포함하지 않습니다(MUST NOT).
  - `PluginModuleRecord`는 `lorebook`을 포함하지 않습니다(MUST NOT).
  - 로어북은 `risuai.lorebookEntries`, 채팅 변수는 `risuai.chats.getVariables`/`updateVariables`(또는 `chats.getContext({ variables })`)로만 접근합니다.
- **메시지 `messageId` 별칭**:
  - 모든 `PluginMessageRecord`는 `PluginMessageTarget.messageId`와 이름이 일치하는 **`messageId: string`**을 반드시 포함합니다(MUST).
  - 호환성을 위해 업스트림 저장 필드 `chatId`도 `messageId`와 같은 값으로 유지할 수 있습니다.
  - `chats`를 구현한 호스트에서 `messageId`는 항상 고유한 비어 있지 않은 문자열이어야 합니다(MUST).
- **필드 프로젝션 (`fields`)**:
  - 읽기는 요청된 `fields`와 식별 메타데이터(`chaId`/`id`/`messageId`/`ref`/`index`)만 반환합니다.
  - `fields`에 알 수 없는 필드명이 포함되어도 에러를 내지 않고 무시합니다(단, 위에서 제외된 `chats`·`globalLore`·`message`·`localLore`·`scriptstate`·`lorebook`은 요청해도 반환하지 않습니다).
  - 값이 존재하지 않는 선택 필드는 결과 객체에서 **키 자체를 생략**합니다(MUST). `null`, `'null'` 문자열, 임의 기본값으로 채우지 않습니다.
  - `fields`를 생략했을 때의 기본 반환 필드:
    - `characters.list`: `['chaId', 'name', 'type', 'image']`
    - `chats.list`: `['id', 'name', 'lastDate', 'folderId', 'modules']` (본문 로딩 없이 싸게 조회 가능한 필드)
    - `messages.list`: `['role', 'data', 'time', 'chatId']` (+ 항상 포함되는 `messageId`, `index`)
    - 단건 `get`에서 `fields`를 생략하면 제외된 중첩 컬렉션·맵을 제외한 해당 레코드의 저장 필드를 반환합니다.

### 3.2 영속 `id`와 임시 `ref` 분리 (로어북·페르소나)

- **캐릭터(`chaId`)·채팅(`id`)·메시지(`messageId`)**는 부모 범위 내에서 유일한 **영속 ID**가 필수입니다(MUST).
- **로어북 엔트리와 페르소나**는 저장 ID가 없는 기존 데이터와 호환하기 위해 두 필드를 엄격히 분리합니다.
  - **`ref: string`**: 항상 존재하며, 현재 플러그인 실행 중 `get`/`update`/`delete`/`setSelected`에 사용하는 참조입니다. 영속 ID가 있으면 `ref === id`이고, 없으면 호스트가 세션용 임시 토큰을 발급합니다.
  - **`id: string | null`**: 실제 저장된 영속 ID가 있을 때만 채워집니다. 없으면 반드시 `null`이어야 합니다(MUST). 호스트는 임시 토큰을 `id` 필드에 넣거나 영구 저장소에 기록해서는 안 됩니다(MUST NOT).
  - 만료되었거나, 대상 위치의 항목이 바뀌었거나, 모호해진 `ref`로 쓰기를 시도하면 호스트는 `NOT_FOUND` 또는 `CONFLICT`로 거절해야 합니다(MUST).

### 3.3 부분 수정(`update`) 및 삭제 규칙

모든 `update` 메서드와 `chats.updateVariables`는 동일한 패치 규칙을 따릅니다.

1. **생략 또는 `undefined` → 변경 없음**: 키가 없거나 값이 `undefined`인 최상위 키는 호스트가 무시하고 기존 값을 유지합니다(MUST). *(브리지 `postMessage`의 structured clone은 `undefined` 키를 그대로 전달하므로 호스트가 명시적으로 무시해야 합니다.)*
2. **`null` → 최상위 키 삭제**: 선택 필드나 채팅 변수 키에 `null`을 전달하면 해당 키를 삭제합니다(MUST).
   - **최상위 키에만 적용**합니다. 중첩 객체나 배열 내부의 `null`은 삭제 신호가 아니라 일반 데이터로 그대로 저장합니다.
   - **호스트 저장 형식에서 필수인 필드**(예: 업스트림 기준 캐릭터의 `name`·`desc`·`firstMessage`·`notes`·`chatPage`·`creatorNotes`·`systemPrompt`·`postHistoryInstructions`·`alternateGreetings`·`tags`·`personality`·`scenario`, 채팅의 `name`·`note`, 메시지의 `role`·`data`, 로어북의 `key`·`secondkey`·`insertorder`·`comment`·`content`·`mode`·`alwaysActive`·`selective`, 모듈의 `name`·`description`, 페르소나의 `name`·`personaPrompt`·`icon`, `settings`의 필수 설정 키)에 `null`을 전달하면 `INVALID_ARGUMENT`로 거절해야 합니다(MUST).
   - 이미 존재하지 않는 선택 필드·변수 키에 `null`을 전달하는 것은 멱등하게 성공으로 처리합니다(MUST).
3. **생략된 필드와 확장 필드 보존**: `update`는 전달된 키만 변경하고, 언급되지 않은 필드와 호스트 전용 확장 필드(`swipes`, `swipeId`, `responseVariants`, 커스텀 필드 등)를 손실 없이 보존해야 합니다(MUST).
4. **불변/금지 키 거절**: 식별자(`chaId`, `id`, `messageId`, `chatId`, `ref`, `index`)나 별도 API로 분리된 키(`character.chats`, `character.globalLore`, `chat.message`, `chat.localLore`, `chat.scriptstate`, `module.lorebook`)가 `changes`에 포함되면 `INVALID_ARGUMENT`로 거절해야 합니다(MUST).

### 3.4 페이징, 앵커, 분기 그래프 호스트 규칙

- **위치 기반 페이징 (`startIndex`, `pageSize`)**:
  - `startIndex` 기본값은 `0`입니다. 음수 `startIndex`는 끝에서부터 역산합니다(예: `total === 25`, `startIndex === -10` → 인덱스 `15`부터 시작; `-100`처럼 크기가 `total`보다 크면 `0`으로 클램프).
  - `startIndex >= total`이면 에러가 아니라 `{ items: [], total }`을 반환합니다.
  - `pageSize`는 양의 안전 정수여야 하며(`0` 이하 또는 비정수는 `INVALID_ARGUMENT`), 기본값은 `50`입니다. 호스트는 과도한 `pageSize`를 상한(최소 `100` 이상)으로 클램프할 수 있습니다.
  - 각 반환 항목의 `index`는 조회 시점의 0-기반 절대 위치입니다.
- **앵커 기반 페이징 (`messages.list`)**:
  - `anchorMessageId`는 `startIndex` 및 `pageSize`와 동시에 지정할 수 없습니다(`INVALID_ARGUMENT`).
  - `before`와 `after`는 `0` 이상의 안전 정수이며 **기본값은 둘 다 `0`**입니다(음수·비정수는 `INVALID_ARGUMENT`).
  - **기준 메시지(`anchorMessageId`) 본인이 항상 결과에 포함**되므로 최대 `before + 1 + after`개의 메시지가 오래된 순서대로 반환됩니다(앞/뒤가 경계에 닿으면 존재하는 만큼만 반환).
  - `before + 1 + after`가 호스트가 허용하는 최대 창 크기(최소 `100` 이상)를 초과하면 `pageSize`와 일관되게 호스트 상한 이내로 클램프합니다(기준 메시지 본인은 항상 유지하고 바깥쪽 `before`/`after`를 축소하며, 호출자는 반환 항목의 `index`와 `total`로 실제 반환 범위를 확인).
  - `anchorMessageId`가 현재 활성 분기에 존재하지 않으면 `NOT_FOUND`로 거절합니다(MUST).
- **분기 그래프 호스트**:
  - `messages.list`와 `messages.get`은 **현재 활성 분기 경로**를 기준으로 동작합니다.
  - 페이지 조회 사이에 활성 분기가 바뀌어 앵커가 사라진 경우, 다른 위치의 메시지를 조용히 돌려주지 않고 `NOT_FOUND`로 거절해야 합니다(MUST).
  - `messages.delete` 또는 `messages.create({ before / after })`가 분기점이나 형제 분기 불변 조건을 깨뜨리는 경우, 형제 분기를 손상시키지 말고 `BRANCH_CONFLICT`(또는 `UNSUPPORTED`)로 거절해야 합니다(MUST).
  - 대체 응답(스와이프, 분기 그래프, 응답 변형)은 공통 규약 밖의 호스트 확장 필드로 둡니다.

### 3.5 권한 매트릭스와 오류 모델

#### 권한 요구 표

| 영역 | 요구 권한 | 거부 시 동작 |
| --- | --- | --- |
| 신규 데이터 읽기 및 이벤트 구독 (`selection`, `characters`, `chats`, `messages`, `generation`, `lorebookEntries`, `modules`, `personas`, `settings`, `chatView.onMessage*`, 컬렉션 `onChanged`/`onDeleted`) | `db` | `PERMISSION_DENIED` reject |
| 신규 데이터 쓰기 (`create`, `update`, `delete`, `setOrder`, `setSelected`, `updateVariables`) | `dbWrite` | `PERMISSION_DENIED` reject |
| MCP 조회·호출 (`risuai.mcp.*`) | `mcpAccess` | `PERMISSION_DENIED` reject |
| 신규 인레이 삭제 (`deleteInlay`) | `inlay` | `PERMISSION_DENIED` reject |
| 기존 인레이 생성 (`createInlay`, `inlayMedia` 확장 포함) | `inlay` | **`null` resolve** (기존 V3 호환 유지) |
| 기존 등록 메서드 중 등록 시 권한 확인 (`addRisuReplacer`·`registerBodyIntercepter`·`addRisuChatListener`) | `replacer` | `null` resolve (`handles`) |
| 기존 프로바이더 등록 (`addProvider`) | `provider` (호출 시점 확인, V3 동일) | 등록은 항상 핸들 반환; 호출 거부 시 기존 V3 오류 응답 유지 |
| 플러그인 저장소 (`pluginStorage.*`) 및 메시지 슬롯 UI (`chatView.registerMessageSlot`·`*Slot*`) | 없음 | - |

#### 구조화 에러 코드 (`PluginErrorCode`)

신규 API는 실패 시 `false`나 `null`로 조용히 넘어가지 않고 **`err.code`가 설정된 에러로 reject**해야 합니다(MUST).

- **대상이 없을 때 `null`을 반환하는 메서드**: 단건 레코드 조회(`*.get`, `personas.getSelected`)와 `chats.getContext`만 대상(또는 현재 선택)이 존재하지 않을 때 `null`을 반환합니다.
- **부모 대상이 없을 때 `NOT_FOUND`로 거절하는 메서드**: 부모를 지정하는 목록·변수·MCP 조회(`chats.list`, `messages.list`, `chats.getVariables`, `lorebookEntries.list`, `mcp.*`)와 모든 쓰기·삭제 메서드는 대상이 없으면 `NOT_FOUND`로 거절합니다.

| 코드 | 발생 조건 |
| --- | --- |
| `INVALID_ARGUMENT` | 인자 타입·범위 오류, 필수 필드에 `null`, 불변/금지 키 수정, 허용 목록 밖 `settings` 키, 모순된 옵션 조합(`anchorMessageId` + `startIndex`/`pageSize`), 금지 HTTP 메서드 등 |
| `PERMISSION_DENIED` | 사용자가 필요한 권한(`db`, `dbWrite`, `mcpAccess`, `inlay`)을 거부 |
| `UNSUPPORTED` | 호스트가 지원하지 않는 기능·옵션·대상(예: 선택 채팅 외의 `mcp` 타겟, 미지원 `redirect` 모드) |
| `NOT_FOUND` | 대상 레코드·부모 컬렉션·앵커·변수 대상 채팅·MCP 모듈·인레이가 존재하지 않음 |
| `CONFLICT` | `pluginStorage.update` 경합 실패, 만료/모호한 `ref` |
| `BRANCH_CONFLICT` | 분기 구조 호스트에서 형제 분기 불변 조건을 위배하는 메시지 변경 |
| `ABORTED` | `AbortSignal` 취소 또는 플러그인 언로드로 작업 중단 (`err.name === 'AbortError'`와 `err.code === 'ABORTED'` 동시 만족) |
| `UNAVAILABLE` | 일시적 백엔드 장애, 연결 불가 MCP 서버 등 |

#### 오류 판정 우선순위

권한이 없는 플러그인이 ID 존재 여부를 탐지하지 못하도록 호스트는 다음 순서로 검증해야 합니다(MUST):

1. `INVALID_ARGUMENT` (입력 형태 검증)
2. `PERMISSION_DENIED` (권한 확인)
3. `UNSUPPORTED` (호스트/옵션 제약 확인)
4. `NOT_FOUND` / `BRANCH_CONFLICT` / `CONFLICT` / `UNAVAILABLE` / `ABORTED`

### 3.6 쓰기 완료 시점과 일관성

- 쓰기 메서드가 resolve되면 호스트의 **권위 상태(authoritative state)**에 반영되었음을 보장하며, 직후 같은 대상을 다시 읽었을 때 변경 내용이 보여야 합니다(MUST).
- 디스크 fsync, 서버 백그라운드 플러시, 원격 동기화 완료는 별도의 호스트 보장이며 쓰기 resolve가 이를 약속하지 않습니다.

---

## 4. 최소 브리지 와이어 규약 (`postMessage`)

V3 호스트는 iframe ↔ 호스트 간 `postMessage` 브리지(`factory.ts` 계열)를 공유합니다. 아래 확장으로 호스트 간 게스트 동작을 통일합니다.

### 4.1 구조화 에러 전달 (`PluginRpcErrorExtension`)

호스트는 기존 `RESPONSE.error: string`을 유지하면서 선택 필드 `errorCode`, `errorName`, `errorDetails`를 함께 보냅니다.

게스트 브리지는 `data.error`가 존재할 때 다음과 같이 복원해야 합니다(MUST):

- `data.errorCode === 'ABORTED'` 또는 `data.errorName === 'AbortError'`이면 `new DOMException(data.error, 'AbortError')`(미지원 환경에서는 `name = 'AbortError'`인 `Error`)를 만들고, **`Object.defineProperty(err, 'code', { value: 'ABORTED', configurable: true })`**로 자체 속성 `code`를 정의합니다. *(표준 `DOMException.code`는 읽기 전용 숫자 `20`이므로 일반 대입이 동작하지 않습니다.)*
- 그 외에는 `new Error(data.error)`에 `err.code = data.errorCode`(존재 시)와 `err.details = data.errorDetails`(존재 시)를 부착합니다.

### 4.2 데이터 기반 핸들 (`PLUGIN_HANDLE`)

**`handles`, `events`, `changes`, `messageSlots` 등 핸들을 반환하는 기능군을 하나라도 구현하는 호스트**는 핸들을 호스트 프록시(`REMOTE_REF`)로 등록하지 않고, 단순 직렬화 객체 [`PluginHandleDescriptor`](./risuai-v3-extensions.d.ts) (`{ __type: 'PLUGIN_HANDLE', id, kind, updatable? }`)로 반환합니다.

게스트 브리지는 이를 받아 다음 규칙으로 핸들을 조립해야 합니다(MUST):

- `id`만 열거 가능한(enumerable) 자체 문자열 속성으로 둡니다.
- `dispose()`는 내부 루트 RPC `_disposePluginHandle(kind, id)`를 호출하는 **비열거(non-enumerable)** 함수로 정의합니다. `Symbol.asyncDispose`가 존재하면 같은 함수를 비열거로 연결합니다.
- `updatable === true`이면 `update(...args)`를 `_updatePluginHandle(kind, id, ...args)`로 연결하는 비열거 함수로 정의합니다.

호스트 측 필수 규칙:

- **소유권 검사**: `_disposePluginHandle`과 `_updatePluginHandle`은 호출 플러그인이 `(kind, id)`의 소유자인지 반드시 확인해야 합니다(MUST). 타 플러그인의 ID는 무시하거나 거부합니다.
- **멱등 해제**: 이미 해제되었거나 없는 ID에 대한 `dispose()` 재호출은 에러 없이 성공(no-op)으로 처리합니다(MUST).
- **콜백 인자 제약**: 기존 V3 브리지의 `serializeArg`는 최상위 위치 인자(positional argument)의 함수만 `CALLBACK_REF`로 직렬화합니다. 따라서 콜백을 받는 모든 `update(...)` 시그니처는 콜백을 객체 안에 중첩하지 않고 위치 인자로 전달합니다.

### 4.3 `fetchOptions` 브리지 작업 및 `AbortSignal` 참고

- **`fetchOptions`**: 게스트가 `new Response(value, init)`로 응답을 재조립할 때 `url`과 `redirected`가 소실되므로, 호스트가 두 값을 함께 보내고 게스트가 `Object.defineProperty`로 자체 속성을 정의합니다. 또한 `Headers`·`FormData`·`URLSearchParams`·`Blob` 바디는 `postMessage` 전송 전에 게스트 브리지에서 직렬화합니다. `fetchOptions`를 구현하지 않는 호스트는 이 작업이 필요 없습니다.
- **`AbortSignal` 전달**: 기존 V3 `factory.ts`의 `serializeArg`는 메서드 이름과 무관하게 **일반 객체 인자의 직속 속성으로 전달된 `AbortSignal`**을 이미 호스트로 전달합니다. 따라서 `modelOptions`(`runLLMModel({ signal })`)과 `mcp`(`mcp.callTool(..., { signal })`)에는 추가 브리지 변경이 필요 없으며, 호스트가 전달된 `signal`을 실제로 존중하고 취소 시 `errorCode: 'ABORTED'`(`errorName: 'AbortError'`)로 거절하기만 하면 됩니다.

---

## 5. 기존 V3 API 확장 명세

이름을 유지한 기존 V3 메서드의 확장 동작입니다. 상세 시그니처는 [`risuai-v3-extensions.d.ts`](./risuai-v3-extensions.d.ts) 11장을 따릅니다.

### 5.1 `handles` (Common)

- **핸들 반환 대상**: `registerSetting`, `registerButton`, `setChatPanel`, `registerMCP`, `addProvider`, `addTTSPreprocessor`, `addTTSPostprocessor`, `addRisuReplacer`, `addRisuScriptHandler`, `addRisuChatListener`, `registerBodyIntercepter`, `addPluginChannelListener`, `onUnload`, 그리고 모든 신규 이벤트 구독 메서드.
- **권한 거부 시 일관된 `null`**: 등록 시점에 `replacer` 권한을 묻는 기존 등록 메서드(`addRisuReplacer`, `registerBodyIntercepter`, `addRisuChatListener`)에서 사용자가 권한을 거절하면 가짜 핸들이 아니라 **`null`**을 반환해야 합니다(MUST). `addProvider`는 기존 V3와 동일하게 등록 시점이 아니라 프로바이더 호출 시점에 `provider` 권한을 확인하므로, 등록 호출은 항상 핸들을 반환합니다.
- **레거시 해제 메서드 소유권 검사**: `'handles'`를 광고하는 호스트는 기존 `unregisterUIPart`, `unregisterMCP`, `unregisterBodyIntercepter`, `setChatPanel`(교체·제거)에도 호출 플러그인 소유권 검사를 반드시 적용해야 합니다(MUST).
- **플러그인 언로드 정리**: 플러그인이 언로드되면 호스트는 해당 플러그인이 소유한 모든 활성 핸들을 자동으로 정리해야 합니다(MUST).

### 5.2 `requestContext` (Common)

- **대상**: `addRisuReplacer` (`beforeRequest`, `afterRequest`), `registerBodyIntercepter`, `addProvider`.
- **옵트인 규칙**: 등록 옵션에 **`{ requestContext: true }`**를 명시한 경우에만 콜백 마지막 인자로 [`PluginRequestContext`](./risuai-v3-extensions.d.ts)를 전달합니다(MUST). 미지정 또는 `false`이면 기존 V3 콜백 인자 개수를 그대로 유지해야 합니다(MUST).
- **`origin` 정의**:
  - `'run'`: 주 채팅 생성 실행(전송·이어쓰기·리롤, 플러그인 `sendChat` 포함).
  - `'host'`: 호스트 자체의 보조 모델 호출(메모리 요약, 감정 분석, 번역, 응답 제안 등).
  - `'script'`: Lua 스크립트 또는 트리거가 유발한 모델 호출.
  - `'plugin'`: 플러그인이 `runLLMModel`로 유발한 모델 호출(`pluginName`과 선택적 `tag` 동반).
- **대상 채팅 정확성**: `characterId`와 `chatId`는 호출 시점의 활성 UI 선택이 아니라 **해당 요청을 유발한 실제 대상 채팅**이어야 합니다(MUST). 다중 채팅 동시 생성 호스트에서도 각 요청의 대상 채팅이 섞여서는 안 됩니다.
- **Provider `mode` 복원**: `addProvider`에서 기존 `arg.mode`가 `'v3'`로 고정되어 있더라도, `requestContext.mode`는 실제 모델 모드(`'model'`, `'submodel'`, `'memory'`, `'emotion'`, `'otherAx'`, `'translate'` 등)를 전달해야 합니다(MUST).

### 5.3 `modelOptions` (Common)

- `runLLMModel(options)`에 `maxTokens`, `temperature`, `responseSchema`, `signal`, `tag`를 추가합니다.
- **우선순위**: 호출 시 전달된 `maxTokens`와 `temperature`는 해당 모드의 사용자 기본 설정보다 우선합니다(MUST).
- **`temperature` 단위**: RisuAI 저장 설정의 `0..200` 정수가 아니라 **표준 LLM 실수 스케일(예: `0.0`–`2.0`)**을 사용합니다(MUST).
- **`responseSchema`**: 구조화 출력을 지원하는 프로바이더에 전달하며, 미지원 프로바이더는 에러 없이 무시합니다.
- **`tag`**: 이 호출로 발생한 요청의 `requestContext.tag`에 전달되며, `requestContext.origin`은 `'plugin'`, `requestContext.pluginName`은 호출 플러그인 이름이어야 합니다(MUST).

### 5.4 `fetchOptions` (Common) 및 `fetchRedirect` (Optional)

- **HTTP 메서드**: Fetch 표준이 허용하는 모든 비금지 메서드를 지원합니다(`GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS` 등). Fetch 금지 메서드(`CONNECT`, `TRACE`, `TRACK`)는 `INVALID_ARGUMENT`로 거절합니다(MUST).
- **바디/헤더**: `Headers`, `FormData`, `URLSearchParams`, `Blob`을 허용합니다.
- **`requestTimeoutMs`**: 응답 데이터가 도착하지 않는 **비활성(inactivity) 타임아웃**(ms)으로 동작합니다(SHOULD). 전체 경과 시간 제한은 `signal: AbortSignal.timeout(ms)`를 사용합니다.
- **`networkRoute` / `logFetch`**: `networkRoute`는 업스트림과 동일하게 `'auto' | 'local_network'`를 받습니다(`'local_network'` 지정 시 지원하는 호스트는 프록시 대신 직접 로컬/LAN으로 요청하며, 웹 빌드 등 미지원 환경은 `UNSUPPORTED`). `logFetch: false`는 `getFetchLogs()` 기록을 생략합니다.
- **응답 복원**: 반환 `Response`에 `url`과 `redirected`를 복원합니다.
- **`fetchRedirect` (Optional)**: 광고 시 `redirect: 'follow' | 'manual' | 'error'`를 지원합니다. `fetchOptions`는 광고하지만 `fetchRedirect`는 광고하지 않는 호스트에서 `'follow'` 외의 값을 받으면 `UNSUPPORTED`로 거절합니다(MUST).

### 5.5 저장소 계약 및 기타 기존 메서드 확장

- **`getLocalPluginStorage().setItem` / `getItem`**: `storage.localPluginStorage.structured === true`인 호스트는 바이너리 및 구조화 값을 직접 보존합니다.
- **`pluginStorage.setItem` / `getItem`**: 모든 호스트에서 JSON 값을 지원하며, `storage.pluginStorage.bytes === true`인 호스트는 **최상위 `Uint8Array` 및 `ArrayBuffer`**를 바이트 그대로 보존해야 합니다(MUST).
- **`storageUpdate` (Optional)**: `pluginStorage.update(key, updater)`는 키를 읽고 `updater(current)`를 실행한 뒤 그 사이 다른 쓰기가 없었을 때만 커밋합니다. 충돌 시 최신 값으로 몇 차례 재시도한 뒤에도 경합이 해소되지 않으면 `CONFLICT`로 거절합니다. `updater`가 `null`을 반환하면 키를 삭제합니다.
- **공식 V3 유지 편입**: 기존 여러 호스트에 이미 존재하는 `setChatPanel`, `alert`, `alertConfirm`, `alertError`를 공식 유지 API에 포함합니다.
- **`inlayMedia` (Optional)**: 기존 `createInlay(data, options?)`의 반환 형태(`Promise<string | null>`, 권한 거부 시 `null`)를 그대로 유지하면서 입력을 오디오(`wav`, `mp3`, `ogg`, `flac`)·비디오(`webm`, `mp4`, `mkv`)까지 확장합니다(호스트가 항상 새 ID 발급). 신규 메서드 `deleteInlay(id)`는 해당 인레이를 삭제합니다(`inlay` 권한, 거부 시 `PERMISSION_DENIED`, 부재 시 `NOT_FOUND`).
- **`messageButtons` (Optional)**: `registerButton`에 `location: 'message'`와 `roles?: ('user' | 'char')[]`를 추가하며, 클릭 콜백에 [`PluginMessageButtonTarget`](./risuai-v3-extensions.d.ts)을 전달합니다.
- **`workers` (Optional)**: 새 JS 메서드는 없으며, 호스트가 플러그인 iframe CSP에 `worker-src blob:`을 허용해 `new Worker(blobUrl)` 전용 워커를 실행할 수 있게 합니다(기존 `connect-src 'none'` 등 네트워크 격리는 유지).
- **`clipboardWrite` (Optional)**: 새 JS 메서드는 없으며, 플러그인 iframe을 `allow="clipboard-write"`로 생성해 브라우저의 user-activation 및 Secure Context 규칙 하에 `navigator.clipboard.writeText()`가 동작하게 합니다. 호스트는 `navigator.clipboard`가 실제로 사용 가능한 보안 컨텍스트에서만 `'clipboardWrite'`를 광고해야 합니다(SHOULD). 클립보드 읽기는 위임하지 않으며, 호스트 문서 클릭(`registerButton`, `getRootDocument()`)은 프레임 포커스·활성화가 없어 대상에서 제외됩니다.

---

## 6. 신규 API 명세

정확한 타입과 파라미터는 [`risuai-v3-extensions.d.ts`](./risuai-v3-extensions.d.ts) 10장을 참조하세요.

### 6.1 `chats` (Common)

네임스페이스: `risuai.selection`, `risuai.characters`, `risuai.chats`, `risuai.messages`

- **`selection.get()`**: 현재 선택된 `{ characterId, chatId, personaId, personaRef }`를 반환합니다. 선택된 캐릭터나 채팅이 없으면 해당 필드는 `null`입니다. `personaId`/`personaRef`는 채팅 바인딩 페르소나가 유효하면 그것을, 아니면 전역 선택 페르소나를 따릅니다.
- **`characters.*`**: `get`, `list`, `create`, `update`, `delete`, `getOrder`, `setOrder`를 제공합니다. `create`는 호스트가 새 `chaId`를 발급하고 사이드바 순서에도 반영하며, `getOrder()` / `setOrder(order)`는 업스트림 `characterOrder` 형식(캐릭터 ID 문자열과 폴더 객체 배열)으로 사이드바 순서를 읽고 교체합니다. `chats`와 `globalLore`는 읽기와 수정 모두에서 제외·금지됩니다.
- **`chats.*`**:
  - `get`, `list`, `create`, `update`, `delete`를 제공합니다. `chats.list`의 기본 필드는 본문 로딩 없이 반환되어야 합니다(SHOULD). `message`·`localLore`·`scriptstate`는 읽기와 수정 모두에서 제외·금지됩니다.
  - **`chats.getContext(target | null, options?)`**: 한 번의 RPC로 캐릭터 필드, 채팅 필드, 메시지 창, 채팅 변수를 함께 읽습니다.
    - `target === null`이면 호스트가 현재 선택된 채팅을 일관되게 해석해 결과의 `characterId`/`chatId`에 채워 반환합니다(선택 없음 시 `null`).
    - 서로 다른 엔티티 간의 분산 스냅샷 트랜잭션까지는 요구하지 않으며, 모든 구획이 **같은 대상 채팅을 일관되게 가리키는 것**을 보장합니다.
  - **`chats.getVariables(target, options?)` / `chats.updateVariables(target, changes)`**:
    - 채팅 변수 이름은 **내부 `$` 접두사를 떼고** 전달·반환합니다(예: `'hp'` ↔ 저장 `scriptstate['$hp']`). 이는 CBS(`{{getvar::hp}}`)·Lua(`getChatVar(triggerId, 'hp')`)와 일치합니다.
    - `names`와 `prefix`를 모두 생략하면 `$` 접두사로 저장된 모든 변수를 반환합니다. 둘 다 지정하면 교집합을 반환합니다.
    - `names`에 없는 변수명은 결과 객체에서 생략합니다. 빈 문자열 키 `''`는 `INVALID_ARGUMENT`로 거절하며, 대상 채팅이 없으면 `NOT_FOUND`로 거절합니다.
- **`messages.*`**:
  - `get`, `list`, `create`, `update`, `delete`를 제공합니다.
  - `create`는 호스트가 새 고유 `messageId`(및 동일 값 `chatId`)를 발급해 반환합니다.
  - `update`는 지정한 필드만 변경하고 미지정 필드 및 호스트 확장 필드를 유지합니다.

### 6.2 `library` (Common)

네임스페이스: `risuai.lorebookEntries`, `risuai.modules`, `risuai.personas`

- **`lorebookEntries.*`**: 모든 호출은 소유자(`character`, `chat`, `module`, `persona`)를 명시합니다. `get`/`update`/`delete`는 `ref`로 대상을 식별합니다([3.2절](#32-영속-id와-임시-ref-분리-로어북페르소나)).
- **`modules.*`**: `get`, `list`, `create`, `update`, `delete`를 제공합니다. 중첩 `lorebook`은 제외하며 `lorebookEntries`로 접근합니다.
- **`personas.*`**: `get`, `list`, `create`, `update`, `delete`, `getSelected`, `setSelected`를 제공합니다. 식별에는 `ref`를 사용합니다.

### 6.3 `events` (Common)

네임스페이스: `risuai.selection`, `risuai.generation`, `risuai.messages`

- **`selection.onChanged`**: `(characterId, chatId, personaId, personaRef)`가 실제로 바뀔 때만 발화합니다(MUST). 목록 맨 앞에 새 채팅이 삽입되어 인덱스만 밀린 경우에는 발화해서는 안 됩니다(MUST NOT). 한 프레임 내 연속 전환은 최종 상태로 병합(coalesce)할 수 있습니다.
- **`generation.onStarted` / `generation.onEnded`**:
  - 하나의 생성 실행(전송·이어쓰기·리롤, 플러그인 `sendChat` 포함)마다 고유한 `runId`를 부여하고 `onStarted` 1회와 `onEnded` 1회를 짝지어 발화합니다(MUST). 자동 이어쓰기·재시도는 같은 `runId`에 포함됩니다.
  - 보조 모델 호출(감정·메모리·번역·서브모델·`runLLMModel`)은 생성 실행이 아니므로 발화하지 않습니다(`requestContext`로 구분).
  - `onEnded`가 `status: 'completed'`로 전달된 직후, `messageIds`의 모든 메시지는 `messages.list`·`messages.get`으로 즉시 읽을 수 있어야 합니다(MUST).
- **`messages.onResponseReceived`**: 모델 응답이 채팅에 커밋된 뒤 ID 기반으로 알립니다(기존 `addRisuChatListener`의 경량 대체).

### 6.4 `settings` (Optional)

네임스페이스: `risuai.settings`

- 허용되는 설정 키는 다음 **16개**로 닫혀 있습니다([`PluginSettings`](./risuai-v3-extensions.d.ts)):  
  `enabledModules`, `moduleIntergration`, `temperature`, `askRemoval`, `maxContext`, `maxResponse`, `frequencyPenalty`, `PresensePenalty`, `theme`, `textTheme`, `lineHeight`, `seperateModelsForAxModels`, `seperateModels`, `customCSS`, `guiHTML`, `colorSchemeName`.
- 허용 목록 밖의 키를 `get`이나 `update`에 넘기거나 필수 설정 키에 `null`을 넘기면 `INVALID_ARGUMENT`로 거절합니다(MUST).
- `settings.onChanged`는 호스트가 `settings`와 `changes`를 함께 광고하는 경우에 제공됩니다.

### 6.5 `changes` (Optional)

- **기능군 교차 게이팅**: `changes`를 광고하는 호스트는 `pluginStorage.onChanged`를 반드시 제공하며(MUST), 도메인 컬렉션의 `onChanged`/`onDeleted`는 해당 도메인 기능군을 함께 광고하는 경우에 제공합니다(MUST):
  - `chats` + `changes`: `characters.onChanged`, `characters.onDeleted`, `chats.onChanged`, `chats.onDeleted`, `messages.onDeleted`
  - `library` + `changes`: `modules.onChanged`, `personas.onChanged`
  - `settings` + `changes`: `settings.onChanged`
- **변경 알림 (`onChanged`)**: `pluginStorage.onChanged(filter, listener)`와 `characters`/`chats`/`modules`/`personas`/`settings`의 `onChanged(listener)`를 제공합니다.
  - 이벤트에는 **병합된 키 또는 ID만** 포함하며, 절대로 변경된 값 본문을 담지 않습니다(MUST NOT).
  - 변경량이 내부 버퍼를 초과하거나 세부 ID를 열거할 수 없을 때는 `resync: true`를 설정해 전달합니다(MUST).
  - 로컬 편집인지 원격 동기화인지 출처 구분은 보장하지 않습니다.
- **영구 삭제 알림 (`chats.onDeleted`, `characters.onDeleted`)**:
  - **영구 삭제(복구 불가) 시점에만** 발화합니다(MUST). 휴지통 이동(soft-delete)에서는 발화하지 않습니다(MUST NOT).
  - 개별 채팅이 영구 삭제되면 `chats.onDeleted({ characterId, chatId })`가 1회 발화합니다.
  - 캐릭터가 영구 삭제되면 **`characters.onDeleted({ characterId })`만 1회 발화**하며, 소속 채팅마다 `chats.onDeleted`를 중복 발화하지 않습니다(MUST NOT).
  - *(플러그인이 언로드된 동안이나 오프라인 동기화로 삭제된 항목은 실시간 이벤트를 받지 못하므로, 완전한 정리가 필요하면 `characters.get`/`chats.get` 확인을 병행해야 합니다.)*
- **메시지 제거 알림 (`messages.onDeleted`)**:
  - 직접 삭제(`messages.delete`·UI 단건 삭제)와 대량 삭제("이 메시지 이후 모두 삭제" 등)로 저장된 채팅에서 하나 이상의 메시지가 제거될 때는 `{ characterId, chatId, messageIds, resync }`를 반드시 전달해야 합니다(MUST). `onChanged`와 마찬가지로 메시지 본문은 절대로 담지 않습니다(MUST NOT).
  - **느슨한 의미론 (ID 재등장 허용)**: `chats.onDeleted`·`characters.onDeleted`와 달리 **영구·복구 불가 삭제를 보장하지 않습니다**. 업스트림형 리롤이나 다중 메시지 응답 후보 전환처럼 이전 `messageId`가 저장 채팅에서 빠지는 동작에서도 발화할 수 있으며(MAY), 보고된 `messageId`는 리롤 복원(`unReroll`), 실행 취소(undo), 응답 후보 재선택 등을 통해 같은 채팅에 **다시 나타날 수 있습니다(MAY)**. 플러그인은 이를 영구 삭제 증명이 아니라 메시지별 캐시 정리·무효화 신호로 다루고, ID가 재등장하더라도 안전하게 동작해야 합니다.
  - **단일 메시지 내부 대안 전환 제외**: 메시지 내부 스와이프(`swipes`)나 단일 메시지 응답 변형처럼 `messageId`가 유지된 채 내용만 바뀌는 경우는 삭제가 아니라 변경(`chats.onChanged`)입니다.
  - **분기 그래프 호스트**: 활성 분기 전환만으로 메시지가 활성 경로에서 벗어난 경우에는 발화해서는 안 됩니다(MUST NOT). 저장소의 모든 분기에서 제거될 때만 발화합니다.
  - **부모 삭제 시 팬아웃 금지**: 채팅이나 캐릭터가 삭제될 때 소속 메시지에 대해 `messages.onDeleted`를 중복 발화해서는 안 됩니다(MUST NOT).
  - **대량 제거와 `resync`**: 한 번의 작업이나 병합 창에서 제거된 ID는 `messageIds` 배열로 묶어 전달합니다. 전체 채팅 교체(복원·동기화 가져오기), 내부 버퍼 초과, 또는 제거된 ID 차분을 특정할 수 없는 경우에는 `resync: true`를 설정해 전달합니다(MUST).
  - *(뷰포트 언마운트 알림인 `chatView.onMessageRemoved`는 스크롤·채팅 전환에서도 발화하므로, 저장 메시지 제거 감지는 `messages.onDeleted`를 사용합니다.)*

### 6.6 `messageSlots` (Optional)

네임스페이스: `risuai.chatView`

- **행 렌더 이벤트**: `chatView.onMessageRendered`와 `chatView.onMessageRemoved`는 스트리밍 완료 후 확정된 메시지 행의 마운트·재렌더(번역 토글 등 본문 갱신 포함)·언마운트를 `messageId` 기준으로 알립니다.
- **Shadow DOM 슬롯**:
  - `chatView.registerMessageSlot({ position, style })`는 각 메시지 본문 위(`aboveBody`) 또는 아래(`belowBody`)에 플러그인 전용 Shadow Root 슬롯을 예약하고, `update({ style })`와 `dispose()`를 가진 핸들을 반환합니다.
  - `setSlotContent(slotId, target, html)`의 HTML은 호스트가 새니타이즈해 활성 채팅 메모리에 유지하며, 가상 스크롤로 행이 재마운트될 때 플러그인을 다시 부르지 않고 재적용합니다(영구 저장하지 않으며 채팅 전환·언로드 시 폐기).
  - `getSlotElement(slotId, target)`는 현재 화면에 마운트된 슬롯의 `SafeElement`(미마운트 시 `null`)를 반환합니다. Shadow Root 경계 밖의 호스트 DOM에는 접근할 수 없습니다.
  - 새니타이저가 인라인 이벤트 핸들러를 제거하므로, 슬롯 내 `data-action` 속성이 붙은 요소 클릭은 `chatView.onSlotAction(slotId, listener)`로 전달됩니다.
  - 모든 `*Slot*` 메서드는 `slotId` 소유권을 검사하며 타 플러그인의 슬롯 접근을 거절합니다(MUST).

### 6.7 `mcp` (Optional)

네임스페이스: `risuai.mcp`

- `mcp.list(target)`, `mcp.listTools(target)`, `mcp.callTool(target, toolName, args, options?)`를 제공합니다.
- **범위**: `target` 채팅에 활성화된 MCP 모듈(내장 프로바이더가 해당 채팅에서 받는 도구와 동일 집합)만 노출합니다. `mcpId`는 해당 모듈 ID와 일치하며, 응답에 MCP 서버 URL/주소는 절대 포함하지 않습니다(MUST NOT).
- **대상 채팅 엄격성**: 모든 호출은 `{ characterId, chatId }`를 명시합니다. 현재 선택된 채팅의 모듈만 해석할 수 있는 호스트는 선택되지 않은 채팅 타겟을 **`UNSUPPORTED`로 거절**해야 하며, 선택된 채팅의 도구를 대신 반환해서는 안 됩니다(MUST NOT).
- **권한·부작용·실패**:
  - 모든 메서드는 `mcpAccess` 권한을 요구합니다(거부 시 `PERMISSION_DENIED`).
  - 대상 채팅에 활성화되지 않은 `mcpId`는 `NOT_FOUND`, 로드·연결 실패는 `UNAVAILABLE`로 거절합니다. 단 `mcp.list`는 로드에 실패한 개별 MCP를 건너뛰고 나머지를 반환합니다.
  - `list`·`listTools`는 MCP 서버 연결·승인·로그인 프롬프트를 유발할 수 있고, `callTool` 역시 내장/로컬 MCP 도구에 대한 호스트 확인 프롬프트나 정책 검사를 거칠 수 있습니다.
  - `mcp.callTool` 호출 결과는 채팅 기록에 자동 기록되지 않습니다.

---

## 7. 적합성 기준 (Conformance Checklist)

호스트가 `getRuntimeInfo().features`에 특정 기능 키를 광고하려면, 해당 기능군에 속한 아래 시나리오를 모두 통과해야 합니다(MUST).

| # | 기능군 | 시나리오 | 기대 결과 |
| --- | --- | --- | --- |
| 1 | 탐색 · 저장소 | `getRuntimeInfo()` 검사 및 `Uint8Array` 왕복 저장 | `apiVersion === '3.0'`, `features`는 `string[]`, 미지 키 무시; `storage.localPluginStorage.structured`·`storage.pluginStorage.bytes` 광고가 실제 재로딩 후 바이트 보존과 일치 |
| 2 | `chats` | 열지 않은 채팅 vs 실제로 빈 채팅 조회 (`chats.get` / `messages.list`) | 열지 않은 채팅은 실제 메시지 반환(빈 플레이스홀더 금지); 빈 채팅은 `{ items: [], total: 0 }` |
| 3 | `chats` | 채팅 A 조회 후 인덱스 0에 새 채팅을 삽입하고 채팅 A에 `messages.update`, `characters.getOrder`/`setOrder` 왕복 | 채팅 A의 대상 메시지만 수정되고 인덱스 0의 새 채팅은 무변경; 사이드바 순서가 `characterOrder` 형식으로 보존되며 `db`/`dbWrite` 권한 적용 |
| 4 | `chats` | 부분 `update`(`null` 삭제·`undefined` 무시·필수 필드 `null`), 금지 키(`globalLore`/`scriptstate`/`chats`/`message`) 읽기·수정 | 지정 키만 반영, `undefined` 무시, 호스트 확장 필드 보존; 필수 필드 `null` 및 금지 키 수정은 `INVALID_ARGUMENT`; 읽기에서 `globalLore`/`scriptstate` 제외 |
| 5 | `chats` | `chats.getVariables` / `updateVariables` (`$` 없이 조회·수정·`null` 삭제) 및 부재 채팅 조회 | `$` 자동 매핑으로 CBS/Lua와 일치, 없는 변수는 결과에서 생략, 부재 채팅은 `NOT_FOUND` |
| 6 | `chats` | 존재하지 않는 `messageId` 수정·삭제, `dbWrite` 거부, 앵커 페이징(`anchorMessageId` + `before`/`after` 및 `pageSize` 혼용) 및 분기 충돌 | 권한 검사가 존재 여부보다 선행(`PERMISSION_DENIED` vs `NOT_FOUND`); 앵커+`pageSize` 혼용은 `INVALID_ARGUMENT`; 사라진 앵커는 `NOT_FOUND`; 분기 불변 조건 위배 변경은 `BRANCH_CONFLICT`(또는 `UNSUPPORTED`) |
| 7 | `library` | 저장 ID가 없는 로어북 엔트리·페르소나 조회·수정 | `id: null`과 비어 있지 않은 `ref` 반환, `ref`로 `update` 성공, 만료 `ref`는 `NOT_FOUND` 또는 `CONFLICT` |
| 8 | `events` | 인덱스 0 새 채팅 삽입 시 선택 이벤트, 생성 중 UI 전환 후 `generation.onEnded`, 동시 다중 채팅 생성 | `(characterId, chatId)` 미변경 시 `selection.onChanged` 미발화; `generation.onEnded`는 원래 대상 `(characterId, chatId)`·`runId`·`status`·`messageIds` 보고 및 `completed` 직후 `messages.list` 조회 가능; 동시 생성 시 각자의 대상과 고유 `runId` 분리 |
| 9 | `handles` | 반환 형태·직렬화·멱등 해제·소유권 검사·권한 거부 | `JSON.stringify(h)`·`{ ...h }`가 `{ id }`; `dispose()` 2회 호출 안전; 타 플러그인 `unregister*` 차단; 권한 거부된 `addRisuReplacer`/`registerBodyIntercepter`/`addRisuChatListener`는 `null` 반환(`addProvider`는 등록 시 항상 핸들 반환) |
| 10 | `requestContext` | 기본 3번째 인자를 가진 레거시 콜백 `(m, t, flag = 'keep')` vs `{ requestContext: true }` 옵트인 및 provider `mode` | 레거시 콜백은 `flag === 'keep'` 유지; 옵트인 콜백만 `PluginRequestContext`(`origin`, 대상 채팅, 실제 `mode`) 수신 |
| 11 | `modelOptions` | `runLLMModel`에 `maxTokens`, `temperature`(0.0–2.0 스케일), `signal`, `tag` 전달 | 유저 기본 설정보다 호출 옵션 우선 적용; `signal` 중단 시 `err.code === 'ABORTED'` 및 `err.name === 'AbortError'` reject; `tag`가 `requestContext.tag`로 전달 |
| 12 | `fetchOptions` | `PUT`/`PATCH`/`DELETE` 및 금지 메서드(`TRACE`), `FormData`/`Blob`/`Headers`/`URLSearchParams`, `networkRoute`, 응답 `url`/`redirected` | 허용 메서드·바디 정상 전송 및 `Response.url`/`redirected` 복원; 금지 메서드는 `INVALID_ARGUMENT` |
| 13 | `fetchRedirect` | `nativeFetch`에 `redirect: 'manual'` / `'error'` 전달 (`fetchRedirect` 광고 호스트 vs `fetchOptions`만 광고한 호스트) | 광고 호스트에서는 지정 모드 동작; `fetchOptions`만 광고하고 `fetchRedirect`는 광고하지 않는 호스트에서 `'follow'` 외의 값은 `UNSUPPORTED` reject |
| 14 | `storageUpdate` | 동일 키에 대한 동시 `pluginStorage.update` 경합 및 `null` 반환 삭제 | 재시도 후 정합성 유지 또는 `CONFLICT` reject; `updater`가 `null` 반환 시 키 삭제 |
| 15 | `settings` | 허용 목록 16개 키 조회·수정, 미허용 키 및 필수 키 `null` 전달, 권한 거부 | 허용 키만 반영·`undefined` 무시; 16개 허용 목록 밖 키 및 필수 키 `null`은 `INVALID_ARGUMENT`; `db`/`dbWrite` 거부 시 `PERMISSION_DENIED` |
| 16 | `changes` | `onChanged` 페이로드 검증, 영구 삭제(`chats.onDeleted`, `characters.onDeleted`) vs 휴지통, `messages.onDeleted` 단건·대량 제거 및 교차 게이팅 | `onChanged`·`messages.onDeleted`는 본문 없이 키/ID만 전달(초과·전체 교체 시 `resync: true`); 휴지통 이동 시 `chats`/`characters.onDeleted` 미발화; 캐릭터·채팅 삭제 시 하위 `onDeleted` 미팬아웃; 분기 전환만으로 `messages.onDeleted` 미발화; 도메인별 이벤트는 해당 기능군과 함께 광고 시 제공 |
| 17 | `inlayMedia` | 오디오/비디오 `createInlay` 및 권한 거부, `deleteInlay` 삭제·부재 삭제 | `createInlay`는 ID 문자열 반환(권한 거부 시 `null` 유지); `deleteInlay`는 삭제 성공, 부재 시 `NOT_FOUND`, 권한 거부 시 `PERMISSION_DENIED` |
| 18 | `messageButtons` | `registerButton({ location: 'message', roles: ['char'] }, cb)` 등록 및 클릭 | `char` 메시지에만 표시되고 클릭 시 `chatId`·`messageId`·`messageIndex`·`role`이 포함된 `PluginMessageButtonTarget` 전달 |
| 19 | `messageSlots` | `chatView.onMessageRendered`/`onMessageRemoved`, 슬롯 스타일 격리, `setSlotContent`/`onSlotAction`, 소유권 검사 | Shadow DOM CSS가 호스트로 누출되지 않음; `data-action` 클릭 전달; 타 플러그인 `slotId` 조작 차단 |
| 20 | `workers` | 샌드박스 프레임 내 `new Worker(blobUrl)` 실행 및 워커 내 네트워크 시도 | Blob 전용 워커 정상 실행 및 메시지 왕복; 워커 내부 `fetch`/XHR/WebSocket은 `connect-src 'none'`으로 차단 유지 |
| 21 | `mcp` | `mcp.list` 응답 검증, 비활성 채팅 타겟 호출, `mcpAccess` 권한 거부 | 응답에 MCP URL 미포함; 선택 채팅 외 미지원 호스트는 활성 채팅으로 대체하지 않고 `UNSUPPORTED` reject; 권한 거부 시 `PERMISSION_DENIED` |
| 22 | `clipboardWrite` | 보안 컨텍스트의 프레임 내 사용자 클릭에서 `navigator.clipboard.writeText()` vs 클립보드 읽기 | 보안 컨텍스트에서만 광고, 프레임 내 클릭에서 쓰기 성공, `clipboard-read`는 계속 거부 |

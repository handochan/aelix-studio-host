# 실행 중인 Aelix 세션으로 ACP 확장 메시지 전달하기

**설계안 · host #32 · 2026-11-19.** 첫 검증 대상은 Studio #24다. 사용자가 보는 대화가 실행 중일 때 steer와 follow-up을 해당 실행 세션의 native 큐에 보낼 수 있어야 한다. host는 호출 자격, 대상 연결, 전달 한도를 책임진다. 입력의 의미와 실제 실행 시점은 ACP와 native가 결정한다. 기존 host에 이 계약이 구현됐다는 뜻으로 예시를 사용하지 않는다. [host #32](https://github.com/handochan/aelix-studio-host/issues/32) · [Studio 결정 D05·D12·D16·D17 및 확인 기록](https://github.com/handochan/aelix-studio/blob/844e6b30/docs/studio-release-train/decisions.md)

## 출발점과 변경 위치

아래 경로는 host `8c7baece`, Studio `844e6b30`, ACP `3bdb9451`에서 확인했다.

| 경계 | 현재 경로와 설계에 주는 제약 |
| --- | --- |
| renderer → host | Studio `packages/desktop/src/common/adapter/ipcBridge.ts`는 대화별 HTTP API를 연결한다. `packages/desktop/src/renderer/pages/conversation/platforms/acp/AcpSendBox.tsx`에는 busy 검사와 별도 로컬 큐가 있다. host의 `crates/aionui-conversation/src/service.rs`와 `runtime_state.rs::try_claim_turn`은 새 메시지의 턴을 선점한다. 확장 요청은 진행 중인 턴을 다시 선점하거나 두 번째 일반 메시지를 저장하지 않는다. |
| host → ACP | `crates/aionui-ai-agent/src/manager/acp/agent.rs`와 `session.rs`가 연결과 ACP 세션 ID를 보유한다. `protocol/acp.rs:432–455`의 요청·단방향 송신 함수에는 제품 호출자가 없다. 후자는 **host에서 agent로** 보낸다. 반대 방향은 `acp.rs:711–797`의 수신 등록부에 추가해야 한다. 기존 `session/update`, permission, terminal 핸들러는 각 표준 경로를 사용한다. |
| wire 이름 | verified: `agent-client-protocol-schema-1.5.0/src/v1/ext.rs`는 메서드와 raw params를 분리한다. verified: `agent-client-protocol-2.0.0/src/schema/mod.rs`의 request/notification enum은 송신할 때 variant의 method를 반환하고 수신 파싱 때 선행 `_`를 벗긴다. host `acp.rs::ext_request` 안의 `format!("_{}", …)`은 공용 송신 함수의 로그·오류 이름에도 쓰인다. 실제 stdio에서 단 하나의 `_`가 있어야 한다. 로그 이름만 검사하면 틀린 철자를 놓친다. |
| ACP → native | verified: `aelix-acp@3bdb9451/src/aelix_acp/server.py:217–235,353–358,380–435`. initialize 최상위 `_meta["aelix/control"]`은 `_aelix/manage`도 광고한다. 이 메서드는 prompt와 같은 mutation lock을 확인하고 설정 쓰기·인증·확장 작업도 취급한다. 이를 그대로 노출하면 세션 경계가 깨진다. verified: `src/aelix_acp/session.py::rpc,read_rpc,prompt`는 native 요청 ID별 응답과 활성 턴을 관리한다. |
| host → renderer | `crates/aionui-realtime/src/handler.rs`는 WS 연결의 사용자와 connection ID를 inbound router로 전달하고, `manager.rs::send_to_or_disconnect`는 특정 연결에 보낸다. 일반 사용자 단위 fan-out은 한 사용자가 연 여러 대화를 구별하지 못한다. Studio `packages/desktop/src/renderer/pages/conversation/platforms/acp/useAcpMessage.ts`는 표준 메시지 스트림을 구독한다. 새 알림에는 별도의 대화 구독이 필요하다. |

## 한 번 협상하는 세션 전용 통로

1. Studio가 신뢰하는 **로컬 Aelix adapter 실행 사양**으로 생성한 task에만 통로를 켠다. host composition이 supervisor의 신뢰된 실행 설정과 실제 task를 연결한다. 이름, renderer의 backend ID, agent가 주장한 `agentInfo.name`, 광고 중 어느 하나만으로 이 권한을 주지 않는다. 구현안은 supervisor가 검증한 설치 manifest의 generation·절대 실행 파일·고정 argv/entrypoint를 보호된 시작 설정으로 host에 넘기고, host가 자기 spawn 사양과 비교해 task에 신뢰 표시를 붙이는 것이다. 임의 interpreter 인자, custom agent 설정, workspace/PATH에서 찾은 동명이인은 매칭하지 않는다. 설치 generation이 교체되면 이전 표시를 재사용하지 않는다. 그 바인딩을 구성할 수 없으면 비활성이다. 다른 ACP agent에는 새 capability도 보내지 않는다. outbound에는 검증된 전체 wire 이름을 직접 받는 SDK 경계를 쓸 수 있다(verified: `agent-client-protocol-2.0.0/src/jsonrpc.rs::UntypedMessage::new`). inbound에서는 수신한 이름 표현을 wire 이름으로 한 번만 정규화한다.
2. host는 `initialize.params.clientCapabilities._meta["aelix/sessionRelay"]`에 `{"version":1,"notifications":true}`를 보낸다. ACP는 `initialize.result.agentCapabilities._meta["aelix/sessionRelay"]`에 버전과 정확한 `requests[]`, `notifications[]`를 보낸다. 두 목록은 **서로 다른 방향**이다. 실제 연결의 응답으로만 협상하고 연결 교체 시 폐기한다. 지원 버전·타입 불일치, 중복, 양쪽 목록 겹침, 범위 밖 이름이 하나라도 있으면 이 통로만 끈다. 광고는 가용성의 상한이다. 세션별 native 기능·권한·상태는 adapter가 호출마다 확인한다. [협상과 발견 예시](examples/channel.json)
3. 메서드는 ASCII 전체 일치 `^_aelix/session/[a-z][a-z0-9-]*(/[a-z][a-z0-9-]*)*$`, UTF-8 96바이트 이내이며 해당 방향 목록에 있어야 한다. 별칭·와일드카드·대소문자 보정·percent decoding은 없다. prefix만 허용하면 agent가 받는 임의 동작이 열리고, host에 기능별 허용표를 두면 매번 host를 바꿔야 한다. **좁은 prefix와 ACP가 광고한 유한 목록의 교집합**을 권고한다. `_aelix/manage`, 범용 native RPC 실행기, 관리/쉘/파일 프록시를 이 이름 공간에 넣지 않는다. ACP의 등록표는 세션 전용 typed handler만 노출한다.
4. renderer → ACP는 응답이 있는 HTTP POST, ACP → renderer는 구독된 WS로 보내는 단방향 알림이다. agent → renderer 임의 요청, renderer → agent 단방향 알림은 v1에 없다. 자식 승인·프로젝트 파일 생성 동의는 응답할 수 있는 기존 승인 경로에서 처리한다. tool call의 `_meta`와 표준 ACP 호출에는 협상이 필요 없다.

### 발견, 요청, 알림의 wire

| 작업 | v1 계약 |
| --- | --- |
| 발견 | `GET /api/conversations/{conversationId}/acp-extensions`. 소유한 활성 세션에서 `ApiResponse.data = {version,bindingId,requests,notifications}`. 응답은 `Cache-Control: no-store`. bindingId는 열려 있는 task·ACP 연결·세션 수명의 불투명 ID다. 자격 증명이 아니다. 구형 host의 404 또는 미협상이면 renderer는 컨트롤을 숨긴다. 조회로 task를 시작하지 않는다. |
| 호출 | 같은 URL에 `POST`, `Content-Type: application/json`, body `{bindingId,method,payload}`. 세 필드만 받는다. payload는 JSON 객체다. host가 소유권 확인 뒤 얻은 **같은 live handle**에 `{jsonrpc:"2.0",id,method,params:{sessionId,bindingId,payload}}`를 보낸다. JSON-RPC ID는 SDK가 할당한다. 성공하면 HTTP 200 `{success:true,data:{result:<ACP result>}}`. 명시적인 JSON null도 보존한다. |
| 구독 | 기존 `/ws`에 `conversation.subscribeAcpExtensions`와 `{conversationId,bindingId}`를 보낸다. 소켓에서 얻은 주체로 소유권을 검사한다. 등록과 `conversation.acpExtensionsSubscribed` ACK(`conversationId,bindingId,seq`)를 원자적으로 직렬화한다. 동일 소켓·바인딩 재구독은 멱등이다. `conversation.unsubscribeAcpExtensions`는 같은 키로 즉시 해제한다. 구독은 조회 권한만 부여한다. |
| 수신 | ACP 알림 `{jsonrpc:"2.0",method,params:{sessionId,bindingId,payload}}`에는 id가 없다. host는 실제 발신 연결·sessionId·bindingId가 현재 등록된 값과 모두 일치하는지, 목록과 크기를 통과하는지 검사한다. `WebSocketMessage`의 `name="conversation.acpExtension"`, `data={conversationId,bindingId,seq,method,payload}`로 해당 구독 소켓에만 보낸다. reply는 없다. [알림 프레임 예시](examples/channel.json) |

host가 읽는 값은 바깥 계약뿐이다. renderer는 URL의 대화만 선택한다. envelope의 여분 키·중복 키를 거절한다. payload의 중첩된 ID는 feature 데이터일 수 있지만 host의 세션·사용자·라우팅 키로 사용하지 않는다. ACP는 메서드별 입력을 검증하고 native 대상은 확인한 세션에서만 찾는다. 어느 쪽도 payload를 일반 RPC 메시지로 다시 dispatch하지 않는다. 통과의 기준은 UTF-8 JSON 값의 동등성이다. 객체 키 순서와 공백은 계약에서 제외하고, 배열 순서, null, Unicode, 미지 필드를 보존한다. v1 숫자는 모든 소비자가 정확히 읽을 수 있는 범위로 제한한다(정수 ±(2⁵³−1), 유한 실수). 큰 식별자는 문자열이다.

renderer의 공용 HTTP bridge는 현재 요청 본문을 console.debug에 남기고 401 뒤에는 refresh 후 원 요청을 재발행한다. CSRF getter는 빈 문자열을 반환한다. U1의 통로 전용 호출은 본문 로그를 생략하고 발견 GET으로 받은 `aionui-csrf-token` 쿠키를 `x-csrf-token`에 담는다. 세션을 갱신한 뒤 바인딩을 다시 발견하되 POST는 자동 재발행하지 않는다. 같은 파일의 WS 수신기(479행)도 payload를 console.debug에 넣으므로 통로 이벤트는 dispatch 전 본문 로그를 생략한다. renderer console과 진단 breadcrumb도 비밀 표식 검사에 포함한다. [Studio HTTP bridge](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/desktop/src/common/adapter/httpBridge.ts#L184-L267) · [host CSRF 쿠키](../../crates/aionui-auth/src/cookie.rs)

### 소유권, 출처와 수명

- **브라우저.** Studio #26의 공개 리스너가 Host·Origin·fetch-site를 검사하고 backend용 Origin을 정규화한다. 브라우저가 보낸 desktop/internal 헤더는 제거한다. host #44의 보호된 리스너에서 활성 operator JWT·만료·session generation을 확인한 브라우저만 발견/호출/구독할 수 있다. POST는 정확한 Origin, JSON, double-submit CSRF(cookie와 header 일치)를 모두 요구한다. 로컬 identity 모드에서 일반 csrf 미들웨어는 설치되지 않으므로, 보호 모드의 발견 GET에서 기존 csrf cookie 발급을 재사용하고 새 POST에는 브라우저 분기에서 검사를 **명시적으로** 붙인다. WS upgrade는 정확한 Origin과 동일 인증이 필요하며 기존 WS 만료·폐기 guard를 유지한다. [host admission](../../crates/aionui-app/src/router/studio_auth.rs) · [router 구성](../../crates/aionui-app/src/router/routes.rs) · [CSRF](../../crates/aionui-auth/src/csrf.rs) · [Studio proxy](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/static-server.ts) · [WS proxy](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/webSocketProxy.ts)
- **Desktop.** supervisor가 신뢰한 출처에서만 붙인 launch credential을 보호 리스너가 검증·소비한다. 검증된 브라우저/desktop의 종류를 host 내부 request extension으로 전달한다. credential이 있어도 외부 Origin을 거부하고 upgrade에는 canonical Origin을 요구한다. Origin 없는 supervisor HTTP의 기존 예외와 desktop의 비쿠키 인증은 유지한다. 업스트림 local 기본 사용자, loopback 주소, renderer가 보낸 신분 헤더는 발급 근거가 아니다. 보호 모드를 쓰지 않는 리스너에서는 HTTP와 WS 양쪽 기능을 비활성화한다. [host #44](https://github.com/handochan/aelix-studio-host/pull/44) · [Studio launcher](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/backend-launcher.ts)
- **Helper·내부 호출.** runtime/helper/hook 토큰에는 발견·호출·구독 권한이 없다. 인증 종류가 혼재한 요청도 거부한다. helper 허용표나 CSRF 예외, 공개 preflight에 새 권한을 추가하지 않는다. D05의 전역 설정·설치·로그인·자격 증명 처리는 Studio의 별도 stdio helper가 맡는다. browser 요청을 supervisor 자격으로 재발행하지 않는다. [helper route 검사](../../crates/aionui-app/src/router/studio_helper_routes.rs)
- **바인딩.** `ensure_owned_conversation(user, conversation)`와 같은 DB 조회 후 이미 열린 task에서 (owner, conversation, task/connection generation, ACP sessionId)를 묶는다. host는 신뢰된 v1 연결에서 `session/new|load|fork` 전에 매번 새 nonce를 만들고 `params._meta["aelix/sessionRelay"].bindingId`에 넣는다(fork는 새 자식용). 성공 응답이 같은 값을 `result._meta`의 같은 키로 돌려준 뒤 결과 세션에 binding을 활성화한다. 실패·echo 누락·활성화 전 이벤트는 버린다. ACP는 각 요청과 생산한 이벤트에 당시의 bindingId를 붙인다. 대기 이벤트를 보낼 때 현재 bindingId로 바꿔 쓰지 않는다. 같은 연결에서 같은 sessionId를 다시 load해도 이전 nonce를 가진 알림과 명령은 거절된다. WS만 다시 연결하면 기존 live binding은 유지한다. 전송 직전에는 소유권과 live generation을 같은 등록/폐기 순서에서 재검사한다. 늦은 응답·알림을 새 handle에 배달하지 않는다. 삭제·권한 회수·재시작·logout은 구독과 신규 dispatch를 폐기한다. 이미 보낸 명령의 효력은 별도로 조정한다. 알림 수신과 enqueue, 인증 폐기와 소켓 송신도 순서화해 폐기된 구독에 대기 중이던 메시지를 버린다. 존재하지 않거나 남의 대화는 동일한 404다. [owner 검사](../../crates/aionui-conversation/src/service_ops.rs) · [live session](../../crates/aionui-ai-agent/src/manager/acp/agent.rs) · [WS guard](../../crates/aionui-realtime/src/handler.rs)

## 전달 한도와 실패 계약

아래 값은 측정 전 v1 기본안이다. host가 주체를 확인한 뒤 허용 비용을 예약하며, 같은 사용자가 탭을 여러 개 열어도 예산은 하나다.

| 대상 | 상한과 동작 |
| --- | --- |
| 입력·결과 | HTTP 본문 64 KiB, 결과와 알림은 각각 **전체 ACP JSON-RPC 프레임** 256 KiB. UTF-8 바이트로 재며 경계값은 허용한다. chunked body도 스트리밍 상한을 적용하고 압축 요청은 415로 거부한다. envelope 검증 전 중첩은 32단계로 제한한다. 광고는 방향당 32개·합계 8 KiB다. 초과한 결과/알림을 잘라서 정상으로 보내지 않는다. |
| stdio | 확장을 켤 신뢰된 adapter 연결은 줄바꿈 전에 byte reader에서 16 MiB 절대 상한을 둔다. 해당 연결에서만 일반 ACP 프레임에도 적용되는 값이므로 현행 표준 경로의 큰 메시지 회귀를 검사한다. 분류 직후 위의 더 작은 확장 상한을 적용하고 SDK, WS, 로그에 큰 값을 복제하기 전에 거절한다. 절대 상한 위반은 해당 연결 종료 및 in-flight 결과 불명으로 처리한다. 현행 host의 프레이밍 지점은 `protocol/acp.rs::run_sdk_background` 안의 `FramedRead`다. verified: `aelix-acp@3bdb9451/src/aelix_acp/wire/transport.py::decode_frame`의 ACP 입력 상한도 16 MiB다. |
| 빈도·동시성 | 발견·호출·구독 변경 합계는 주체당 10회/초, burst 20. 대화 binding당 동시에 ACP 요청 1개, 주체당 8개. 대기열은 두지 않고 초과는 429. 각 binding에서 알림 30개/초, burst 60. 소켓당 최대 8개 binding과 확장 대기 32프레임 또는 512 KiB 중 먼저 닿는 값. 초과 시 해당 스트림을 무효화하고 재구독을 요구한다. backpressure를 ACP 이벤트 펌프·일반 permission에 전파하지 않는다. |
| 시간 | SDK에 넘긴 뒤 응답 대기 최대 120초(사용자 승인 시간 포함). HTTP disconnect·취소·시간 초과는 호출자의 대기만 끝낸다. SDK reader는 늦은 reply를 회수한다. 점유 슬롯은 응답 또는 binding 폐기까지 보유하고, deadline 뒤 미해결이면 해당 binding의 통로를 격리해 슬롯 누적을 막는다. 일반 ACP 턴을 kill·재시작하지 않는다. 정상 응답을 회수하면 격리를 풀 수 있다. |

오류 body는 `ErrorResponse`의 `success:false,error,code,details`다. host가 안전한 고정 문구를 만들고 대상 renderer에는 필요한 코드만 준다. 원격 오류 설명·stack·native 경로·원시 `error.data`를 싣지 않는다. host의 오류 분류는 transport 정보이며 adapter의 기능별 진단은 renderer가 소유한다. WS 구독 오류는 요청 소켓에만 `conversation.acpExtensionError`로 같은 code를 준다.

| 상태 | 코드·처리 |
| --- | --- |
| admission·대상 | 401 `AUTH_REQUIRED`, 403 `SOURCE_FORBIDDEN` 또는 `CSRF_INVALID`, 404 `CONVERSATION_NOT_FOUND`. 다른 owner와 없는 대화는 같은 응답. |
| dispatch 전 | 400 `EXT_INVALID`(문법, envelope), 403 `EXT_METHOD_DENIED`(범위·방향·광고), 409 `EXT_UNAVAILABLE`(미협상·비활성), 409 `EXT_STALE_BINDING`(교체·종료), 413 `EXT_TOO_LARGE`, 415 `EXT_MEDIA_TYPE`, 429 `EXT_RATE_LIMITED`(+ `Retry-After`). 이 지점에서 ACP 송신은 0회다. |
| agent가 답한 오류 | HTTP 422 `EXT_AGENT_ERROR` + `details.rpcCode`(정수)만 전달한다. 예시의 adapter code -33101은 턴 불일치, -33102는 세션 범위 밖, -33103은 자식/epoch 불일치다. adapter가 native dispatch 가능성을 배제하지 못할 때는 계약 공통 -33100을 반환하고 host는 아래 결과 불명으로 매핑한다. 명시적 오류를 받았다는 사실만으로 이전 native 쓰기가 롤백됐다고 표시하지 않는다. |
| SDK handoff 이후 EOF·timeout·응답 유실/크기 초과·binding 교체 | 502(transport/응답), 504(deadline) 또는 409(교체), `EXT_OUTCOME_UNKNOWN`, `details.retryable=false`. HTTP 자체가 끊기면 renderer도 같은 상태로 취급한다. 응답 넘김 여부가 모호한 쓰기는 자동 재시도하지 않는다. 기존 prompt 경계의 선례는 `crates/aionui-ai-agent/src/protocol/acp.rs::prompt`와 `protocol/error.rs`다. [오류·결과 불명 예시](examples/channel.json) |

dispatch 직전 취소라면 보내지 않는다. 이후 브라우저의 AbortSignal은 native 취소 확인이 아니다. 명시적인 turn Stop은 표준 `session/cancel`, 자식 Stop은 대상 자식을 지정한 별도 확장 요청, 개별 예약 입력 제거는 native가 그 기능과 수신 확인을 제공할 때만 노출한다. 원격에서 오류가 나도 동일 입력을 일반 prompt나 renderer 로컬 큐로 자동 전환하지 않는다.

## 순서와 다시 연결하기

host는 binding별 ACP **수신 순서**에 따라 허용된 알림에 1부터 단조 증가하는 `seq`를 붙인다. ACK의 seq는 등록 시점의 마지막 값(없으면 0)이다. 포화·허용된 알림의 유실 또는 기형 때문에 연속성을 보장할 수 없으면 `conversation.acpExtensionError`에 `EXT_RESYNC_REQUIRED`를 보내고 구독을 무효화한다. 이 신호도 보낼 수 없으면 해당 소켓을 닫는다. 소켓당 bounded unicast로 보내며 사용자 전체 broadcast 후 renderer 필터링에 의존하지 않는다. HTTP 응답과 WS 알림의 상대 도착 순서는 보장하지 않는다. renderer는 같은 binding의 seq 중복을 무시하고 누락 시 다시 구독한다.

다시 열면 발견 → 구독 ACK → 해당 기능의 권위 있는 snapshot 조회 순서로 복구한다. snapshot 동안 알림은 작은 임시 버퍼에 두고 **기능의 revision/cursor**로 병합한다. 커서가 없는 기능은 알림을 무효화 신호로 사용해 다시 조회한다. 버퍼 포화 때도 재조회한다. host seq는 재생 cursor가 아니며 이전 binding에 쓸 수 없다. 요청·결과·알림·cursor·원문을 host DB에 저장하지 않는다. 다시 열어도 남아야 하는 자식의 마지막 상태는 adapter가 부모 tool call의 `_meta["aelix/subagents"]`에 반영하고 기존 메시지 이력에서 읽는다. 세션 불러오기와 host DB 동기화는 host #4 소유다. [adapter의 부모별 진행 상태](https://github.com/handochan/aelix-acp/blob/3bdb9451/src/aelix_acp/runtime/subagents.py) · [Studio의 v1 parser](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/desktop/src/renderer/pages/conversation/Messages/acp/SubagentProgress/parser.ts)

## 소비자별 D16 계약

JSON 파일의 HTTP·ACP 교환은 **제안하는 v1 golden 예시**다. 서로 독립적인 예시이므로 binding, seq, JSON-RPC ID는 fixture 안에서 해석한다. 각 소비자 파일에는 한 건의 완전한 왕복이 있다. 반복 구간을 생략한 교환에서는 같은 규칙으로 HTTP 요청의 method와 payload에 openResponse의 sessionId와 bindingId를 붙이며, HTTP 성공은 acpResponse.result를 data.result에 그대로 담는다. 예시는 실제 제품의 wire capture가 아니다. `initialize` 조각은 다른 표준 capability를 생략한다.

| 소비자 | 전달 계약과 수용 기준 |
| --- | --- |
| [#24 실행 중 입력](examples/running-input.json) | `input/state`로 adapter가 관찰하는 현재 턴의 불투명 `turnId`를 얻고 `input/steer`, `input/follow-up`에 되돌려 준다. adapter는 자신이 관찰한 턴이 일치할 때만 바인딩된 native stdin으로 보낸다. native가 읽기 전 턴이 끝날 수 있다. prompt 전체 mutation lock이 끝나기를 기다리면 안 된다. renderer는 연속 클릭을 직렬화하고 수락 응답을 받은 뒤 다음 입력을 보낸다. `accepted:true, delivery:"native-queue"`는 native 큐 접수다. 응답의 turnId는 전송 전 adapter가 확인한 값이다. 턴이 끝나는 순간의 입력은 이후 처리될 수 있으므로 UI에는 “큐에 접수됨”으로 표시한다. 실제 모델 소비·최종 답변 순서는 별도로 관찰한다. 첫 예시는 text를 고정한다. 첨부가 있는 입력은 별도 지원이 광고될 때까지 원문을 보존하고 전송하지 않는다. verified: `aelix-ai@cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/rpc_mode.py:1197–1224`의 native 명령은 `steer`·`follow_up`, 필드는 `message`다. [Studio #24](https://github.com/handochan/aelix-studio/issues/24) |
| [#7 도구·스킬 선택](examples/selections.json) | `selections/list`의 key를 `selections/set`의 ordered changes에 그대로 넣는다. adapter가 현재 세션의 workspace와 native 권한을 확인한다. 제안은 세션에서 목록 읽기 및 **project** 선택 쓰기를 허용하고, **global** 쓰기는 adapter에서 native 호출 전에 거부하여 D05 helper로 안내한다. 예시는 기존 project 파일을 쓰는 경우다. 새 파일은 아래 native 확인 왕복 뒤에만 쓴다. payload의 `confirmed:true`로 대체할 수 없다. renderer는 scope 잠금·mismatches·saved/applied/inForce/pending과 apply.deferred를 구분하고 수정 후 목록을 다시 읽는다. 프로필 selector는 기존 `agents/use`→`activeAgent` 흐름을 계속 쓴다. 예시의 project 성공 데이터는 #7의 global 실행 자료를 바탕으로 한 제안값이다. verified: `aelix-ai@cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/control_selection.py`, 특히 `_consent_to_new_project_file`. [#7의 native 요청·응답](https://github.com/handochan/aelix-studio/issues/7#issuecomment-7009272038) |
| [S3 자식 제어](examples/children.json) | 부모 binding에서 `children/list`로 얻은 childId·native epoch로 steer/wrap-up/stop을 요청한다. adapter/native가 해당 부모 소속과 epoch, 허용 상태를 다시 검사한다. 다른 부모의 자식·종료 후 재사용한 ID에 효력이 없어야 한다. adapter가 제안된 ACP 메서드를 native `aelix_control`의 승인된 자식 동작에 개별 대응시킨다. 최종 native 이름·필드·종료 사유는 아직 open인 #612의 capture/schema와 맞춘 뒤 동결한다. 자식 활동 알림과 `children/events`는 **같은 프로세스·같은 native epoch**에서만 이어 읽는다. 이전/미래/만료 cursor는 snapshot을 다시 읽고, 프로세스 교체 뒤에는 영속 이력으로 돌아간다. 자식 승인은 표준 `session/request_permission`, 자식 직접 resume은 #836 범위다. 이 예시로 S3 출시를 약속하지 않는다. [aelix-ai #612](https://github.com/handochan/aelix-ai/issues/612) · [Studio #31](https://github.com/handochan/aelix-studio/issues/31) |

**#7의 새 project 파일 확인.** verified: `aelix-acp@3bdb9451/src/aelix_acp/session.py:348–350,779–797`, `server.py:76–81`. 현재 native UI 활성화와 dialog 응답은 `aelix/clientActions` 및 `_aelix/client_action`에 묶여 있다. A2에서는 해당 세션의 진행 중인 selection 작업과 native confirm ID를 묶어 표준 `session/request_permission`의 일회성 허용/거부로 번역한다. 확인 문구에는 영향 받는 workspace와 생성 파일을 표시한다. 사람이 이 요청에 직접 일회성 허용을 선택해야 원래 native ID의 `extension_ui_response`로 확인한다. 일반 승인 자동화·다른 dialog의 응답·클라이언트 boolean을 사용하지 않는다. 검증 가능한 사람 응답이 없는 모드에서는 생성을 지원하지 않는다. native의 45초 기한, 거부, 종료, 연결 상실에는 거절 또는 취소로 정리한다. native 이벤트를 읽는 루프가 동의를 기다리지 않도록 별도 bounded task에서 처리한다. [native 확인 코드](https://github.com/handochan/aelix-ai/blob/cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/control_selection.py#L291-L326)

**#24의 턴 경합.** verified: `aelix-ai@e19ab55e/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/rpc_types.py:68–84`, `packages/aelix-agent-core/src/aelix_agent_core/harness/core.py:1783–1812` 및 `cad65ce9`의 같은 두 파일. native 명령은 turnId를 받지 않고 harness는 현재 phase와 무관하게 큐에 넣는다. 특정 턴에만 적용하는 계약을 선택하면 native에서 “예상 턴 일치 확인과 enqueue”를 한 연산으로 만들고 그 capability가 있을 때만 공개해야 한다. host 변경은 두 선택 모두 동일하다.

## 관측과 검증

저빈도 `info`에는 통로 협상·종료, binding 교체, 결과 불명에 대한 안전한 correlation ID와 단계만 남긴다. `warn`은 malformed·범위/한도 거절·gap을 집계/샘플링하고, `error`는 세션 바인딩 계약 위반에 쓴다. 고빈도 성공은 방향·건수·바이트·지연·outcome의 bounded 집계, `debug`는 단계별 타이밍이다. 원격에서 선택한 메서드 문자열을 무제한 metric label로 쓰지 않는다. payload/result/error.data, prompt, 키·경로·자식 메시지·헤더·토큰·원본 프레임은 모든 기본 로그, HTTP tracing, stderr 전달 및 crash 진단에서 제외한다. 공용 `acp.rs::send_request`는 요청과 응답을 직렬화해 로그 요약 경로로 건넨다. 통로에서는 크기/상태만 기록하는 송수신 경로를 쓰고 원격 오류 문구를 HTTP 5xx나 로그로 복사하지 않는다.

| 시험 | 검증할 결과 |
| --- | --- |
| Host wire 계약 | fixtures의 HTTP→실제 ACP stdio→HTTP, stdio→WS를 각각 대조한다. 누락·중복된 선행 underscore, id 혼선, unknown future field·Unicode·배열·null 변형을 검출한다. 새로운 가상 세션 메서드 하나를 ACP 광고에만 추가해 동일 host에서 양방향 통과를 보인다. 무광고/구버전·비Aelix·위장 agent, 표준 메서드와 `_aelix/manage`, 반대 방향 요청은 ACP 송신/WS 전달 0회다. 호환성 대조군에서 표준 prompt, permission, tool metadata가 기존과 같은 프레임과 이력을 낸다. |
| Auth·격리 | 보호된 router/실제 WS에서 로그인 전·만료·폐기 401, 잘못된/중복 Host·Origin, CSRF 없음/불일치, 위조 desktop, runtime/helper 및 비보호 local 리스너를 각각 검사한다(자격 증명 오류 401, 출처·권한 403, 기능 미등록 404). desktop·로그인 WebUI 각각 정상 POST/WS를 검증한다. owner A/B, 같은 owner의 서로 다른 대화/탭, 같은 프로세스의 다른 ACP 세션, 같은 connection/sessionId를 다시 load한 뒤 도착한 이전 nonce의 알림, 구독 해제·삭제·재시작 직전/직후를 경합시켜 잘못된 쓰기와 수신을 **0건**으로 단언한다. |
| 경계와 장애 | 바이트 한도 바로 아래·경계·바로 위, chunked, 중복 envelope 필드, 깊이, 양방향 burst·슬롯 고갈, 큰 오류 응답, 한 소켓만 느린 경우를 각각 고정 code·계속 동작하는 대조군과 확인한다. handoff 직전/직후·native 적용 뒤 reply 직전 연결 손실, 늦은 응답·알림, ACK 전후 재연결을 주입한다. 미발송 0건/전달 후 native 최대 1회, unknown·강제 resync, 늦은 결과가 새 binding에 들어가지 않음을 검사한다. 비밀 표식은 renderer의 허용된 본문에서만 보이고 로그·DB·비인가 소켓에는 없어야 한다. |
| 첫 수직 검증 | #24에서 **실제 번들 adapter와 native**, 제어 가능한 모델 응답으로 턴을 열어 둔다. desktop와 로그인 WebUI에서 steer 1회와 follow-up 1회를 보내어 native ack, 현재/다음 처리 순서, 모드와 명시적 Stop의 경합, adapter가 이미 끝난 것으로 아는 stale turn 거절, ACP 송신과 native 수신 사이 턴이 끝나는 경합에서 보장한 접수 수준, 재접속 뒤 중복 입력 0건을 관찰한다. 가짜 agent의 echo만으로 이 항목을 통과시키지 않는다. |
| 후속 수직 검증 | #7은 native 파일과 다음 요청의 실제 도구·스킬을 관찰한다. project 파일 생성 승인/거절, global 쓰기 거부와 helper 경로, 잠금·deferred를 모두 확인한다. S3는 #612가 들어온 조합에서 동시에 실행되는 두 자식, 대상 하나만 변경, 다른 부모 거절, 승인 대기 중 이벤트 진행, epoch 전후 replay/영속 이력을 확인한다. synthetic model, 실 provider, 설치 target 근거를 구분한다. |

## 구현 PR 순서와 사용자 선택

| PR 단위 | 선행·산출물과 종료 조건 |
| --- | --- |
| H1 · host 요청 | 이 설계 승인 후 보호 모드 admission 증명, trusted task binding, v1 협상/GET/POST, stdio request·크기·outcome 테스트를 묶는다. `aionui-api-types`에 얇은 DTO, conversation route/service에 소유권, ACP manager에 live handle을 둔다. capability에 notifications=true를 먼저 광고하지 않는다. |
| H2 · host 알림 | H1 위에 기존 realtime inbound router의 구독, 수신 hook, strict unicast와 폐기/gap 처리를 붙인다. 양방향 보안·과부하 테스트가 통과한 빌드에서만 v1 전체를 켠다. `AppServices`가 의존성을 조립한다. 새 DB migration, 별도 worker/port, 기능별 host route는 필요 없다(D12). |
| A1 → U1 · #24 | ACP가 D16 fixture대로 입력 상태와 busy-safe handler를 추가하고 native 큐 접수와 실행 턴 종료 경합을 확인한다. Studio는 H1에서 정한 신뢰 실행 사양을 supervisor에서 전달하고, A1+H2의 정확한 branch SHA로 기존 입력 UI와 전용 HTTP 호출을 연결하고 WebUI/desktop 수직 시험을 수행한다. 첫 완료 기준은 하나의 host 빌드로 실제 실행 중 입력 성공이다. |
| A2 → U2 · #7 | native selection pin/계약과 project 쓰기 방침이 선행이다. ACP 범위 검증·native 동의 후 Studio 설정 컨트롤에 연결한다. H2 바이너리 그대로 #7 예시와 실제 적용을 검증한다. |
| A3 → U3 · S3 | #612의 native wire·capability·기록 수명과 지원 버전이 선행이다. 예시의 native mapping을 대조해 ACP에서 승인/이벤트를 연결한 뒤 Studio 자식 패널에서 검증한다. 합의 전에는 해당 메서드를 광고하지 않는다. #612가 beta.3에 빠지면 S3는 다음 릴리스로 보낸다. |
| 통합 | 각 기능은 host/ACP/Studio SHA와 native wheel hash를 함께 기록한다. 기능별로 host release나 Studio 배포 pin을 올리지 않는다. D16 통합 시점에 승인받아 host release·Studio pin을 올리고 설치본에서 재검증한다. 이 PR의 완료 범위는 설계와 예시다. |

사용자 판단이 필요한 항목은 다음 네 가지다. 수치 조정은 실제 #24/#7 payload·지연을 측정한 구현 PR에서 제안한다.

1. **노출 범위** — 모든 확장 통과, Aelix prefix만, host 고정표, 좁은 prefix와 동적 광고의 교집합 중 **교집합**을 권고한다. 전역 관리 접근을 닫으면서 새 세션 기능은 ACP와 renderer만 바꿀 수 있다.
2. **로그인 WebUI의 초기 공개** — desktop 우선 또는 동일 기능 동시 공개 중 **#26 보호 모드에서 두 클라이언트 동시**를 권고한다. 실제 브라우저의 세션·Origin·CSRF·WS 철회 시험을 출시 조건으로 둔다.
3. **#7의 project 쓰기** — 파일 변경 전체를 helper로 보내거나 현재 workspace의 project 변경을 세션에서 적용할 수 있다. **project 쓰기는 native 동의·권한 아래 세션에서, global 쓰기는 helper에서**를 권고한다. project 설정은 같은 workspace를 쓰는 이후 세션에도 영향을 준다고 UI에 표시한다.
4. **#24의 턴 종료 경합** — 현재 native와 호환되는 **큐 접수 보장**을 권고한다. 특정 턴이 끝났으면 반드시 거부해야 한다면 native의 원자적 턴 검사 capability를 선행으로 두고 그때까지 엄격한 입력 모드를 숨긴다.

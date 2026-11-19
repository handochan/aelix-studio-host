# 실행 중인 Aelix 세션으로 ACP 확장 메시지 전달하기

**설계안 · host #32 · 2026-11-19.** 첫 검증 대상은 Studio #24다. 사용자가 보는 대화가 실행 중일 때 steer와 follow-up을 해당 실행 세션의 native 큐에 보낼 수 있어야 한다. host는 호출 자격, 대상 연결, 전달 한도를 책임진다. 입력의 의미와 실제 실행 시점은 ACP와 native가 결정한다. 기존 host에 이 계약이 구현됐다는 뜻으로 예시를 사용하지 않는다. [host #32](https://github.com/handochan/aelix-studio-host/issues/32) · [Studio 결정 D05·D12·D16·D17 및 확인 기록](https://github.com/handochan/aelix-studio/blob/844e6b30/docs/studio-release-train/decisions.md)

**사용자 확정 (2026-11-19).** U1은 세션 prefix와 방향별 광고의 교집합, U2는 desktop과 로그인 WebUI 동시 공개, U3은 세션 내 project 쓰기와 helper의 global 쓰기, U4는 native 큐 접수 표시다. U5-b로 활성화하고 U6-a로 실행 중 추가 입력의 이력을 H1에 넣는다. 아래 PR과 D16 JSON은 이 결정을 구현할 계약이다. [사용자 결정](https://github.com/handochan/aelix-studio-host/issues/32#issuecomment-7010830532) · [독립 설계 리뷰](https://github.com/handochan/aelix-studio-host/pull/48#issuecomment-7010774663)

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

1. 보호된 host가 owner의 대화와 실제 task에서 결정한 **내장 Aelix backend**를 후보로 고른다. initialize 응답의 `agentInfo.name == "aelix-acp"`까지 일치해야 통로를 켠다(U5-b). 표시 이름, 요청 body의 backend, 일반 custom ACP 또는 같은 이름을 광고한 타 backend만으로는 열리지 않는다. host의 기존 owner별 세션/agent metadata 해석과 handshake 이름 검사를 조합한다. 번들 환경 수용 시험에서는 내장 등록을 확인한다. custom agent를 생성하는 기존 smoke만으로는 이 양성 조건을 검증할 수 없다. 실행 파일·argv·설치 manifest 대조나 새로운 supervisor 계약은 없다. [host의 이름 검사](../../crates/aionui-ai-agent/src/manager/acp/agent.rs#L687-L693) · [대화 backend 해석](../../crates/aionui-conversation/src/session_context.rs#L285-L347) · [기존 Studio smoke](https://github.com/handochan/aelix-studio/blob/844e6b30/scripts/studio-preview/hostSmoke.mjs#L32-L53)
2. 첫 handshake에서 이름을 알기 전에는 내장 backend 후보에게만 `initialize.params.clientCapabilities._meta["aelix/sessionRelay"]`를 보낸다. H1은 `{"version":1,"notifications":false}`, H2 지원 host는 `notifications:true`다. `channel.json`의 H2 자식 광고는 #612를 제공하는 시험 구성에만 해당한다. ACP는 `initialize.result.agentCapabilities._meta["aelix/sessionRelay"]`에 버전과 정확한 `requests[]`, `notifications[]`를 보낸다. false일 때 두 번째 목록은 비어야 한다. host는 응답의 이름과 광고를 검증한 뒤에만 발견·호출을 열고, 연결을 교체하면 폐기한다. 미지원 버전, 잘못된 타입/범위, 중복·방향 겹침은 통로를 비활성화한다. H1에서 빈 알림 목록은 정상이며 표준 `session/update`에는 영향이 없다. adapter는 세션별 native 기능·권한·상태도 호출마다 검사한다. [협상·발견 예시](examples/channel.json)
3. 메서드는 ASCII 전체 일치 `^_aelix/session/[a-z][a-z0-9-]*(/[a-z][a-z0-9-]*)*$`, UTF-8 96바이트 이내이며 해당 방향 목록에 있어야 한다. 별칭·와일드카드·대소문자 보정·percent decoding은 없다. prefix만 허용하면 agent가 받는 임의 동작이 열리고, host에 기능별 허용표를 두면 매번 host를 바꿔야 한다. **좁은 prefix와 ACP가 광고한 유한 목록의 교집합**을 적용한다(U1). `_aelix/manage`, 범용 native RPC 실행기, 관리/쉘/파일 프록시를 이 이름 공간에 넣지 않는다. ACP의 등록표는 세션 전용 typed handler만 노출한다. renderer는 수신 결과·알림을 신뢰할 수 없는 데이터로 파싱하고, 알림으로 shell·파일 열기·인증·관리 작업을 실행하지 않는다.
4. renderer → ACP는 응답이 있는 HTTP POST, ACP → renderer는 구독된 WS로 보내는 단방향 알림이다. agent → renderer 임의 요청, renderer → agent 단방향 알림은 v1에 없다. 자식 승인·프로젝트 파일 생성 동의는 응답할 수 있는 기존 승인 경로에서 처리한다. tool call의 `_meta`와 표준 ACP 호출에는 협상이 필요 없다.

### 발견, 요청, 알림의 wire

| 작업 | v1 계약 |
| --- | --- |
| 발견 | `GET /api/conversations/{conversationId}/acp-extensions`. 소유한 활성 세션에서 `ApiResponse.data = {version,bindingId,requests,notifications}`. 응답은 `Cache-Control: no-store`. bindingId는 열려 있는 task·ACP 연결·세션 수명의 불투명 ID다. 자격 증명이 아니다. 소유한 대화의 닫힌 task는 409 `EXT_NO_LIVE_TASK`다. 구형 host의 404 또는 미협상이면 지원하지 않는 기능을 숨긴다. 조회로 task를 시작하지 않는다. |
| 호출 | 같은 URL에 `POST`, `Content-Type: application/json`, body `{bindingId,method,payload}`. 세 필드만 받는다. payload는 JSON 객체다. host가 소유권 확인 뒤 얻은 **같은 live handle**에 `{jsonrpc:"2.0",id,method,params:{sessionId,bindingId,payload}}`를 보낸다. JSON-RPC ID는 SDK가 할당한다. 성공하면 HTTP 200 `{success:true,data:{result:<ACP result>}}`. 명시적인 JSON null도 보존한다. |
| 구독 | 기존 `/ws`에 `conversation.subscribeAcpExtensions`와 `{conversationId,bindingId}`를 보낸다. 소켓에서 얻은 주체로 소유권을 검사한다. 등록과 `conversation.acpExtensionsSubscribed` ACK(`conversationId,bindingId,relaySeq`)를 원자적으로 직렬화한다. 동일 소켓·바인딩 재구독은 멱등이다. `conversation.unsubscribeAcpExtensions`는 같은 키로 즉시 해제한다. 구독은 조회 권한만 부여한다. |
| 수신 | ACP 알림 `{jsonrpc:"2.0",method,params:{sessionId,bindingId,payload}}`에는 id가 없다. host는 실제 발신 연결·sessionId·bindingId가 현재 등록된 값과 모두 일치하는지, 목록과 크기를 통과하는지 검사한다. `WebSocketMessage`의 `name="conversation.acpExtension"`, `data={conversationId,bindingId,relaySeq,method,payload}`로 해당 구독 소켓에만 보낸다. reply는 없다. [알림 프레임 예시](examples/channel.json) |

host가 읽는 값은 바깥 계약뿐이다. renderer는 URL의 대화만 선택한다. envelope의 여분 키·중복 키를 거절한다. payload의 중첩된 ID는 feature 데이터일 수 있지만 host의 세션·사용자·라우팅 키로 사용하지 않는다. ACP는 메서드별 입력을 검증하고 native 대상은 확인한 세션에서만 찾는다. 어느 쪽도 payload를 일반 RPC 메시지로 다시 dispatch하지 않는다. 통과의 기준은 UTF-8 JSON 값의 동등성이다. 객체 키 순서와 공백은 계약에서 제외하고, 배열 순서, null, Unicode, 미지 필드를 보존한다. v1 숫자는 모든 소비자가 정확히 읽을 수 있는 범위로 제한한다(정수 ±(2⁵³−1), 유한 실수). 큰 식별자는 문자열이다.

renderer의 공용 HTTP bridge는 현재 요청 본문을 console.debug에 남기고 401 뒤에는 refresh 후 원 요청을 재발행한다. CSRF getter는 빈 문자열을 반환한다. D20에 따라 Studio는 발견·호출·구독, CSRF, binding별 접수 상태와 재연결을 독립 renderer 모듈에 둔다. 기존 입력창과 S3 패널은 이 모듈을 사용한다. Aion 대화 store와 ipcBridge에 기능별 dispatch를 추가하지 않는다. 이 모듈의 통로 전용 호출은 본문 로그를 생략하고 발견 GET으로 받은 `aionui-csrf-token` 쿠키를 `x-csrf-token`에 담는다. 세션을 갱신한 뒤 바인딩을 다시 발견하되 POST는 자동 재발행하지 않는다. 같은 파일의 WS 수신기(479행)도 payload를 console.debug에 넣으므로 통로 이벤트는 dispatch 전 본문 로그를 생략한다. renderer console과 진단 breadcrumb도 비밀 표식 검사에 포함한다. [Studio HTTP bridge](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/desktop/src/common/adapter/httpBridge.ts#L184-L267) · [host CSRF 쿠키](../../crates/aionui-auth/src/cookie.rs)

### 소유권, 출처와 수명

- **브라우저.** Studio #26의 공개 리스너가 Host·Origin·fetch-site를 검사하고 backend용 Origin을 정규화한다. 브라우저가 보낸 desktop/internal 헤더는 제거한다. host #44의 보호된 리스너에서 활성 operator JWT·만료·session generation을 확인한 브라우저만 발견/호출/구독할 수 있다. POST는 정확한 Origin, JSON, double-submit CSRF(cookie와 header 일치)를 모두 요구한다. Origin이 없는 브라우저 POST와 cross-site 요청은 기존 source 검사에서 거부된다. Double-submit은 여기에 더하는 방어다. 로컬 identity 모드에서 일반 csrf 미들웨어는 설치되지 않으므로, 보호 모드의 발견 GET에서 기존 csrf cookie 발급을 재사용하고 새 POST에는 브라우저 분기에서 검사를 **명시적으로** 붙인다. WS upgrade는 정확한 Origin과 동일 인증이 필요하며 기존 WS 만료·폐기 guard를 유지한다. [host admission](../../crates/aionui-app/src/router/studio_auth.rs) · [router 구성](../../crates/aionui-app/src/router/routes.rs) · [CSRF](../../crates/aionui-auth/src/csrf.rs) · [Studio proxy](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/static-server.ts) · [WS proxy](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/webSocketProxy.ts)
- **Desktop.** supervisor가 신뢰한 출처에서만 붙인 launch credential을 보호 리스너가 검증·소비한다. 검증된 브라우저/desktop의 종류를 host 내부 request extension으로 전달한다. credential이 있어도 외부 Origin을 거부하고 upgrade에는 canonical Origin을 요구한다. Origin 없는 supervisor HTTP의 기존 예외와 desktop의 비쿠키 인증은 유지한다. 업스트림 local 기본 사용자, loopback 주소, renderer가 보낸 신분 헤더는 발급 근거가 아니다. 보호 모드를 쓰지 않는 리스너에서는 HTTP와 WS 양쪽 기능을 비활성화한다. [host #44](https://github.com/handochan/aelix-studio-host/pull/44) · [Studio launcher](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/web-host/src/backend-launcher.ts)
- **Helper·내부 호출.** runtime/helper/hook 토큰에는 발견·호출·구독 권한이 없다. 인증 종류가 혼재한 요청도 거부한다. helper 허용표나 CSRF 예외, 공개 preflight에 새 권한을 추가하지 않는다. D05의 전역 설정·설치·로그인·자격 증명 처리는 Studio의 별도 stdio helper가 맡는다. browser 요청을 supervisor 자격으로 재발행하지 않는다. [helper route 검사](../../crates/aionui-app/src/router/studio_helper_routes.rs)
- **바인딩.** `ensure_owned_conversation(user, conversation)`와 같은 DB 조회 후 이미 열린 task에서 (owner, conversation, task/connection generation, ACP sessionId)를 묶는다. host는 신뢰된 v1 연결에서 `session/new|load|fork` 전에 매번 새 nonce를 만들고 `params._meta["aelix/sessionRelay"].bindingId`에 넣는다(fork는 새 자식용). 성공 응답이 같은 값을 `result._meta`의 같은 키로 돌려준 뒤 결과 세션에 binding을 활성화한다. 실패·echo 누락·활성화 전 이벤트는 버린다. ACP는 각 요청과 생산한 이벤트에 당시의 bindingId를 붙인다. 대기 이벤트를 보낼 때 현재 bindingId로 바꿔 쓰지 않는다. 같은 연결에서 같은 sessionId를 다시 load해도 이전 nonce를 가진 알림과 명령은 거절된다. WS만 다시 연결하면 기존 live binding은 유지한다. 전송 직전에는 소유권과 live generation을 같은 등록/폐기 순서에서 재검사한다. 늦은 응답·알림을 새 handle에 배달하지 않는다. 삭제·권한 회수·재시작·logout은 구독과 신규 dispatch를 폐기한다. 이미 보낸 명령의 효력은 별도로 조정한다. 알림 수신과 enqueue, 인증 폐기와 소켓 송신도 순서화해 폐기된 구독에 대기 중이던 메시지를 버린다. 존재하지 않거나 남의 대화는 동일한 404다. [owner 검사](../../crates/aionui-conversation/src/service_ops.rs) · [live session](../../crates/aionui-ai-agent/src/manager/acp/agent.rs) · [WS guard](../../crates/aionui-realtime/src/handler.rs)

## 전달 한도와 실패 계약

아래 값은 측정 전 v1 기본안이다. host가 주체를 확인한 뒤 허용 비용을 예약하며, 같은 사용자가 탭을 여러 개 열어도 예산은 하나다.

| 대상 | 상한과 동작 |
| --- | --- |
| 입력·결과 | HTTP 본문 64 KiB, 결과와 알림은 각각 **전체 ACP JSON-RPC 프레임** 256 KiB. UTF-8 바이트로 재며 경계값은 허용한다. chunked body도 스트리밍 상한을 적용하고 압축 요청은 415로 거부한다. envelope 검증 전 중첩은 32단계로 제한한다. 광고는 방향당 32개·합계 8 KiB다. 초과한 결과/알림을 잘라서 정상으로 보내지 않는다. |
| stdio | host의 `protocol/acp.rs::run_sdk_background`는 `FramedRead`와 `LinesCodec::new()`를 쓴다. 공통 상한을 설정한 LinesCodec으로 한 줄을 받은 뒤, JSON 파싱 전에 줄 구분자를 제외한 UTF-8 바이트 수를 잰다. 파싱 후 메서드로 확장 알림을 식별하고, 메서드가 없는 응답은 ID로 대기 중인 확장 요청과 연결한다. 확장 프레임이면 앞서 잰 길이를 256 KiB와 비교해 초과분의 전달을 거부한다. 표준 프레임에는 공통 줄 상한을 적용한다. **공통 줄 상한은 H1의 측정 항목**이다. 고정 번들의 최대 지원 이미지 tool 결과·embedded resource·load replay·일반 텍스트의 방향별 전체 줄 길이(UTF-8 bytes)를 실제 stdio에서 재고, 최대 관측치·지원 크기와 여유분·경계 바로 위/아래 회귀를 H1에 기록한 후 수치를 고른다. 선정 전에는 16 MiB를 표준 프레임의 상한으로 고정하지 않는다. 미분류 프레임은 측정으로 정한 공통 상한에서 중단하고 영향받은 진행 중 요청은 결과 불명으로 끝낸다. verified: `aelix-acp@3bdb9451/src/aelix_acp/wire/transport.py::decode_frame`의 16 MiB는 adapter 입력 방향이다. |
| 빈도·동시성 | 발견·호출·구독 변경 합계는 주체당 10회/초, burst 20. binding당 실행 슬롯 **4개**, 주체당 8개를 원자적으로 예약하고 초과는 429로 즉시 거부한다. 하나의 project 동의가 열려 있어도 다른 세 슬롯으로 state·steer·자식 Stop을 동시에 보낼 수 있다. host에는 메서드별 우선순위가 없다. ACP도 사용자 동의를 기다리는 동안 prompt/세션 공용 mutex나 native 이벤트 reader를 점유하지 않는다. 충돌하는 쓰기만 해당 자원에서 조정한다. 알림은 binding당 30개/초, burst 60. 소켓당 구독 8개, 확장 대기 32프레임 또는 512 KiB 중 먼저 닿는 값에서 해당 스트림을 무효화한다. |
| 시간 | handoff 뒤 120초가 deadline이다. HTTP 취소·disconnect가 먼저 와도 SDK 요청은 원래 deadline까지 실행 슬롯에 남는다. deadline에 실행 슬롯에서 빼고 binding당 **별도 미회수 예산(정상 허용 2개)**에 계상한다. 1~2개로는 신규 요청을 격리하지 않는다. 세 번째 미회수 건이 생길 때에만 해당 binding의 신규 확장 호출을 닫고 이미 보낸 요청의 reply를 계속 회수한다. 새 송신은 없고 이미 예약한 요청 수만큼만 더 남을 수 있다(binding당 SDK pending 최대 6). 미회수가 2개 이하로 줄면 재개한다. 프로세스 전체에도 주체별 pending 총량을 제한한다(실행 8 + 미회수 예산 8, 합계 16). 새 binding을 열어도 예전 pending이 회수되거나 연결이 끝날 때까지 총량에 포함한다. 표준 턴 Stop(`session/cancel`)·permission 응답은 확장 슬롯·격리와 독립이다. |

오류 body는 `ErrorResponse`의 `success:false,error,code,details`다. host가 안전한 고정 문구를 만들고 대상 renderer에는 필요한 코드만 준다. 원격 오류 설명·stack·native 경로·원시 `error.data`를 싣지 않는다. host의 오류 분류는 transport 정보이며 adapter의 기능별 진단은 renderer가 소유한다. HTTP 200의 `result`는 typed 기능 결과를 그대로 싣는다. 실행 여부·잠금·동의 거절·stale turn·같은 부모 안의 stale epoch 등 정상적으로 판정한 기능 거절은 `{accepted:false,reason,...}`이고 안전한 상세(예: scope·mismatches)를 result에 둔다(S5). 부분 적용이 있으면 적용 상태도 명시한다. JSON-RPC error는 잘못된 계약·권한 범위·전송/결과 불명에 쓴다. 임의 stderr·예외를 기능 진단으로 렌더링하지 않는다. WS 구독 오류는 요청 소켓에만 `conversation.acpExtensionError`로 같은 code를 준다.

| 상태 | 코드·처리 |
| --- | --- |
| admission·대상 | 401 `AUTH_REQUIRED`, 403 `SOURCE_FORBIDDEN` 또는 `CSRF_INVALID`, 404 `CONVERSATION_NOT_FOUND`. 다른 owner와 없는 대화는 같은 응답. |
| dispatch 전 | 400 `EXT_INVALID`(문법, envelope), 403 `EXT_METHOD_DENIED`(범위·방향·광고), 409 `EXT_NO_LIVE_TASK`(소유한 대화가 아직 열리지 않음), 409 `EXT_UNAVAILABLE`(지원·협상 실패), 409 `EXT_STALE_BINDING`(교체·종료), 413 `EXT_TOO_LARGE`, 415 `EXT_MEDIA_TYPE`, 429 `EXT_RATE_LIMITED`(+ `Retry-After`). 이 지점에서 ACP 송신은 0회다. |
| agent가 답한 오류 | 잘못된 요청과 binding/부모 권한 범위 위반은 HTTP 422 `EXT_AGENT_ERROR` + `details.rpcCode`(정수)만 전달한다. 세션 경로로 global 쓰기를 강제한 경우는 범위 오류 -33102다. 동일 requestId에 다른 내용은 -32602다. native dispatch 결과를 알 수 없으면 adapter의 공통 -33100을 아래 결과 불명으로 매핑한다. 응답만으로 이전 native 쓰기의 롤백을 추론하지 않는다. |
| SDK handoff 이후 EOF·timeout·응답 유실/크기 초과·binding 교체 | 502(transport/응답), 504(deadline) 또는 409(교체), `EXT_OUTCOME_UNKNOWN`, `details.retryable=false`. HTTP 자체가 끊기면 renderer도 같은 상태로 취급한다. 응답 넘김 여부가 모호한 쓰기는 자동 재시도하지 않는다. 기존 prompt 경계의 선례는 `crates/aionui-ai-agent/src/protocol/acp.rs::prompt`와 `protocol/error.rs`다. [오류·결과 불명 예시](examples/channel.json) |

dispatch 직전 취소라면 보내지 않는다. 이후 브라우저의 AbortSignal은 native 취소 확인이 아니다. 명시적인 turn Stop은 표준 `session/cancel`, 자식 Stop은 대상 자식을 지정한 별도 확장 요청, 개별 예약 입력 제거는 native가 그 기능과 수신 확인을 제공할 때만 노출한다. 원격에서 오류가 나도 동일 입력을 일반 prompt나 renderer 로컬 큐로 자동 전환하지 않는다.

### 쓰기 중복 제거 (S2·S9)

renderer는 새 쓰기 의도마다 UUIDv7 `payload.requestId`를 한 번 생성한다. steer·follow-up, selections/set, 자식 steer·wrap-up·stop에 적용한다. **최초 실행**은 쓰기 메서드로 보내고, **수동 재확인**은 읽기 전용 `_aelix/session/requests/lookup`으로 보낸다. 조회 payload는 원래 쓰기의 `requestId` 하나다. H1부터 ACP가 lookup을 광고한다. host는 다른 광고 메서드와 같은 envelope로 전달하고 SDK는 조회마다 새 JSON-RPC id를 붙인다. renderer는 HTTP 재인증·재연결·수동 재확인 때 쓰기 메서드를 재발행하지 않는다.

**쓰기 접수.** ACP는 소유 세션·권한 확인 뒤 현재 binding의 키를 조회하고, 새 키면 native 송신 전에 원자적으로 예약한다. 10분 창의 `firstSeen`은 ACP가 그 binding에서 쓰기 키를 처음 받은 단조 시각이다. UUIDv7은 고유성에만 사용한다. 기존 키로 중복 전달된 쓰기는 메서드·정규화한 payload digest를 대조한 뒤 이미 진행 중인 작업이나 결과를 재사용한다. 턴·epoch·파일 잠금 상태가 바뀌어도 저장된 결과는 유지한다. 다른 내용으로 키가 충돌하면 native 호출 없이 -32602로 거부한다. binding당 최대 256개 키와 응답 바이트 예산을 예약하며, 공간 부족이면 새 쓰기는 dispatch 전 `accepted:false,reason:"dedup-capacity"`로 거부한다. 기한 안의 항목을 일찍 퇴출하지 않는다. 미회수 작업은 기한이 지나도 정산할 때까지 슬롯에 보관한다.

**수동 재확인.** ACP가 요청을 처리할 때 같은 owner·binding 및 원 작업의 조회 권한을 확인하고, 기존 항목과 경과 시간을 함께 검사한다. 이 handler는 키 예약, `firstSeen` 갱신, native 호출 경로에 진입하지 않는다.

| ACP의 조회 결과 | renderer 동작 |
| --- | --- |
| 600초 미만이며 완료, 조회 응답 ≤ 256 KiB | `{status:"completed",result:…}`의 저장된 결과를 표시한다. 저장된 확정 RPC 오류는 `{status:"completed",rpcCode:…}`로 안전한 숫자 코드만 표시한다. |
| 600초 미만이며 완료, 조회 응답 > 256 KiB | ACP는 저장된 결과를 싣지 않고 같은 조회 RPC id로 `{status:"unavailable",reason:"result-too-large"}`를 반환한다. host는 이를 정상 조회 결과(HTTP 200)로 전달하고 renderer는 해당 기능 상태와 대화 이력을 읽는다. |
| 600초 미만이며 진행 중 / 결과 불명 | `{status:"pending"}`이면 기존 작업이 진행 중임을 표시한다. `{status:"unknown"}`이면 현재 상태와 이력을 조회한다. |
| 경과 시간 ≥ 600초 또는 키 없음 | `{status:"unavailable"}`을 반환한다. 물리적 퇴출 시점과 무관하며, 최초 쓰기보다 조회가 먼저 도착해도 같은 결과다. renderer는 해당 기능의 상태와 대화 이력을 읽는다. 조회 실패를 미실행으로 표시하거나 기존 쓰기를 재전송하지 않는다. |

ACP는 **송신 전**에 실제 조회 RPC id, JSON-RPC envelope, `status`와 중첩 `result`까지 직렬화한 전체 UTF-8 바이트 수를 줄 구분자 없이 검사한다. 262,144 B까지는 완료 응답 전체를 보낸다. 초과하면 작은 대체 응답만 보내므로 host의 프레임 제한을 통과한다. 원래 쓰기의 완료 상태·저장 결과·만료 시각은 그대로다. 조회로 인한 native 호출, 새 예약, 쓰기 재전송은 각각 0회다. [직렬화 경계 예시](examples/running-input.json)

binding이 바뀌었거나 lookup을 광고하지 않는 세션에서도 상태·이력을 다시 읽는다. #24에서는 현재 binding의 `input/state`와 owner의 `GET /api/conversations/{id}/messages`를 사용한다. 기한 안에 출발한 lookup도 ACP에서 처리할 때 이미 만료됐으면 native 호출은 **0회**다. [만료 전후 조회와 renderer 후속 읽기](examples/running-input.json)

## 순서와 다시 연결하기

host는 binding별 승인한 알림의 수신 순서에 따라 1부터 `relaySeq`를 붙인다. ACK의 relaySeq는 등록 당시 마지막 값(없으면 0)이다. native `epoch,seq`는 기능의 재생 커서다. gap·포화·기형에는 해당 구독을 무효화하고 `conversation.acpExtensionError/EXT_RESYNC_REQUIRED`를 한 번 보낸다. 이 신호도 보낼 수 없으면 소켓을 닫는다. 개별 소켓에 순서대로 보내되 HTTP 응답과 WS 간 도착 순서는 독립이다. renderer는 같은 binding의 relaySeq 중복을 무시한다.

ACP는 자식별로 달라진 상태를 묶어 binding당 초당 최대 10개의 알림 프레임을 보낸다. 마지막 상태와 native cursor를 포함하고 중간 활동은 `children/events`에서 읽는다. 서로 다른 자식이나 terminal 상태를 조용히 잃어버리지 않는다. 한 프레임에 담지 못하면 snapshot 무효화로 전환한다. renderer는 binding당 하나의 resync만 유지한다. 연속 실패는 1·2·4·8·16·30초 상한에 jitter를 더해 재구독하며, backoff 중 신호를 합친다. 상한은 jitter 포함 30초다. ACK와 snapshot 후 30초 안정적으로 수신해야 시도 횟수를 초기화한다. 계속 실패하면 마지막 상태에 동기화 대기를 표시한다.

다시 열면 발견 → 구독 ACK → 해당 기능의 권위 있는 snapshot 조회 순서로 복구한다. snapshot 동안 알림은 작은 임시 버퍼에 두고 **기능의 revision/cursor**로 병합한다. 커서가 없는 기능은 알림을 무효화 신호로 사용해 다시 조회한다. 버퍼 포화 때도 재조회한다. 알림 목록이 비어 있는 H1에서는 구독 단계를 건너뛰고 재발견 뒤 snapshot만 조회한다. relaySeq는 이전 binding에 쓸 수 없다. 확장 POST·결과·확장 알림·cursor는 host DB에 저장하지 않는다. U6-a의 일반 사용자 이력은 아래 표준 ACP 업데이트로만 남긴다. 다시 열어도 남아야 하는 자식의 마지막 상태는 adapter가 부모 tool call의 `_meta["aelix/subagents"]`에 반영하고 기존 메시지 이력에서 읽는다. 세션 불러오기와 host DB 동기화는 host #4 소유다. [adapter의 부모별 진행 상태](https://github.com/handochan/aelix-acp/blob/3bdb9451/src/aelix_acp/runtime/subagents.py) · [Studio의 v1 parser](https://github.com/handochan/aelix-studio/blob/844e6b30/packages/desktop/src/renderer/pages/conversation/Messages/acp/SubagentProgress/parser.ts)

## 소비자별 D16 계약

JSON 파일의 HTTP·ACP 교환은 **제안하는 v1 golden 예시**다. 서로 독립적인 예시이므로 binding, seq, JSON-RPC ID는 fixture 안에서 해석한다. 각 소비자 파일에는 한 건의 완전한 왕복이 있다. 반복 구간을 생략한 교환에서는 같은 규칙으로 HTTP 요청의 method와 payload에 openResponse의 sessionId와 bindingId를 붙이며, HTTP 성공은 acpResponse.result를 data.result에 그대로 담는다. 예시는 실제 제품의 wire capture가 아니다. `initialize` 조각은 다른 표준 capability를 생략한다.

| 소비자 | 전달 계약과 수용 기준 |
| --- | --- |
| [#24 실행 중 입력](examples/running-input.json) | `input/state`로 adapter가 관찰하는 현재 턴의 불투명 `turnId`를 얻고 `input/steer`, `input/follow-up`에 되돌려 준다. adapter는 자신이 관찰한 턴이 일치할 때만 바인딩된 native stdin으로 보낸다. native가 읽기 전 턴이 끝날 수 있다. prompt 전체 mutation lock이 끝나기를 기다리면 안 된다. renderer는 연속 클릭을 직렬화하고 수락 응답을 받은 뒤 다음 입력을 보낸다. `accepted:true, delivery:"native-queue"`는 native 큐 접수다. 응답의 turnId는 전송 전 adapter가 확인한 값이다. 턴이 끝나는 순간의 입력은 이후 처리될 수 있으므로 UI에는 “큐에 접수됨”으로 표시한다. 실제 모델 소비·최종 답변 순서는 별도로 관찰한다. 첫 예시는 text를 고정한다. 첨부가 있는 입력은 별도 지원이 광고될 때까지 원문을 보존하고 전송하지 않는다. verified: `aelix-ai@cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/rpc_mode.py:1197–1224`의 native 명령은 `steer`·`follow_up`, 필드는 `message`다. [Studio #24](https://github.com/handochan/aelix-studio/issues/24) |
| [#7 도구·스킬 선택](examples/selections.json) | 활성 세션의 `selections/list`에서 global scope는 `writable:false,reason:"management-helper"`로 투영한다. project scope의 native 쓰기 권한과 잠금은 보존한다. 받은 key로 ordered changes를 만들고, 기존 project 파일 쓰기 후 목록을 다시 읽어 saved·applied·inForce·pending·mismatches를 표시한다. 잠금 등 기능 거절은 result의 reason으로 보인다. global set을 강제하면 ACP는 native 송신 전에 -33102로 거부한다. **첫 prompt 전 등 live task가 없으면** 선택 컨트롤은 비활성이고 “세션을 연 뒤 이 대화의 도구·스킬을 변경할 수 있습니다”를 표시한다. 발견 GET이 task나 prompt를 시작하지 않는다. 정상 세션 열기/attach가 끝나면 재발견·재조회한다. 전역 관리는 helper로 연다. 새 project 파일은 아래 native 확인을 거친다. 프로필은 기존 `agents/use`→`activeAgent` 경로를 쓴다. project 응답은 #7의 global 자료에서 유도한 계약 예시다. verified: `aelix-ai@cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/control_selection.py`. [#7 native 예시](https://github.com/handochan/aelix-studio/issues/7#issuecomment-7009272038) |
| [S3 자식 제어](examples/children.json) | 부모 binding에서 `children/list`로 얻은 childId·native epoch로 steer/wrap-up/stop을 요청한다. adapter/native가 해당 부모 소속과 epoch, 허용 상태를 다시 검사한다. 다른 부모의 자식·종료 후 재사용한 ID에 효력이 없어야 한다. adapter가 제안된 ACP 메서드를 native `aelix_control`의 승인된 자식 동작에 개별 대응시킨다. 최종 native 이름·필드·종료 사유는 아직 open인 #612의 capture/schema와 맞춘 뒤 동결한다. 자식 활동 알림과 `children/events`는 **같은 프로세스·같은 native epoch**에서만 이어 읽는다. 이전/미래/만료 cursor는 snapshot을 다시 읽고, 프로세스 교체 뒤에는 영속 이력으로 돌아간다. 자식 승인은 표준 `session/request_permission`, 자식 직접 resume은 #836 범위다. 이 예시로 S3 출시를 약속하지 않는다. [aelix-ai #612](https://github.com/handochan/aelix-ai/issues/612) · [Studio #31](https://github.com/handochan/aelix-studio/issues/31) |

**#7의 새 project 파일 확인.** verified: `aelix-acp@3bdb9451/src/aelix_acp/session.py:348–350,779–797`, `server.py:76–81`. 현재 native UI 활성화와 dialog 응답은 `aelix/clientActions` 및 `_aelix/client_action`에 묶여 있다. A2에서는 해당 세션의 진행 중인 selection 작업과 native confirm ID를 묶어 표준 `session/request_permission`의 일회성 허용/거부로 번역한다. 확인 문구에는 영향 받는 workspace와 생성 파일을 표시한다. 사람이 이 요청에 직접 일회성 허용을 선택해야 원래 native ID의 `extension_ui_response`로 확인한다. 일반 승인 자동화·다른 dialog의 응답·클라이언트 boolean을 사용하지 않는다. 검증 가능한 사람 응답이 없는 모드에서는 생성을 지원하지 않는다. native의 45초 기한, 거부, 종료, 연결 상실에는 거절 또는 취소로 정리한다. native 이벤트를 읽는 루프가 동의를 기다리지 않도록 별도 bounded task에서 처리한다. [native 확인 코드](https://github.com/handochan/aelix-ai/blob/cad65ce9/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/control_selection.py#L291-L326)

### #24의 큐 접수와 이력 경계 (B1 · U4 · U6-a)

`input/state`가 `active:false,turnId:null`이면 새로 누른 보내기는 기존 일반 send를 쓴다. 이전 steer POST를 일반 send로 바꾸지 않는다. 활성 턴으로 전송했더라도 종료와 native enqueue가 경합할 수 있다. 접수 영수증을 본 renderer는 턴 종료 시 **세션 단위 안내** “큐에 접수됨 · 소비가 확인되지 않은 입력은 다음 프롬프트에서 처리될 수 있음”을 표시한다. 개별 쓰기는 접수까지만 표시한다. 표준 이력의 messageId와 쓰기 requestId는 독립이므로 어느 영수증이 소비됐는지, 남은 건수나 텍스트를 추정하지 않는다. 안내는 사용자가 닫거나 대화를 바꾸면 지운다. 다시 열면 서버에 저장된 사용자 이력만 표시한다. 다음 prompt를 자동으로 시작하지 않는다. 새 prompt에서 실제로 소비되면 그때 그 prompt의 이력에 남는다. verified: `aelix-ai@e19ab55e/packages/aelix-coding-agent/src/aelix_coding_agent/rpc/rpc_types.py:68–84`, `packages/aelix-agent-core/src/aelix_agent_core/harness/core.py:1783–1812`(turnId 없는 enqueue).

| 시점 | ACP와 host가 기록할 일 |
| --- | --- |
| 일반 prompt 시작 | host가 원래 사용자 입력을 기존 경로로 한 번 저장하고 턴을 선점한다. adapter는 이 prompt 자체의 native user 이벤트를 live `user_message_chunk`로 재송신하지 않는다. 원래 prompt와 추가 입력의 **발생 경로**를 구분한다. text 비교로 제거하면 동일한 문장을 두 번 보낸 경우가 사라진다. |
| 큐 접수 | `accepted:true,delivery:"native-queue"`는 접수 영수증이다. host DB에 사용자 행을 추가하거나 새 턴을 선점하지 않는다. 취소되어 소비되지 않은 입력에는 이력 행이 없다. |
| 실제 소비 | native가 추가 사용자 메시지를 이번 prompt의 모델 context에 반영한 시점에 adapter가 표준 `session/update`의 `user_message_chunk`를 보낸다. v1 text 입력 하나는 완전한 text block 하나와 안정적인 표준 `messageId` 하나다. host는 U5-b 세션의 **현재 실행 중인 prompt**에 속한 업데이트만 정상 사용자 이력으로 저장·표시한다. (owner, 대화, live binding, prompt, messageId)로 중복 반영을 막고, 동일 문장의 별도 입력은 별도 ID로 남긴다. host는 feature requestId와 native 입력 내용을 대조하지 않는다. |
| follow-up 출력과 종료 | 현재 prompt에서 drain한 follow-up의 사용자 행과 후속 assistant 출력은 **같은 미완료 ACP prompt 및 host turn**에 속한다. adapter는 native prompt의 실제 idle과 해당 prompt에서 발생한 표준 업데이트 송신이 끝난 뒤 `session/prompt` 응답을 보낸다. host도 그 응답 전에 받은 이벤트의 저장을 마친 뒤 claim을 해제한다. 마지막 drain을 놓쳐 큐에 남은 입력은 이번 턴을 붙잡지 않는다. 다음 prompt에서 drain·출력·저장하며 끝난 턴으로 출력하지 않는다. |
| load·재접속 | host는 load replay 중, prompt 바깥, 폐기된 binding 및 다른 agent의 추가 입력을 이 경로에서 저장하지 않는다. 다시 열 때 GET messages로 기존 사용자·assistant 이력을 읽는다. 확장 메시지를 재생해 이력을 만들지 않는다. |

현행 host의 `SessionUpdate::UserMessageChunk` 분기는 비어 있고 load에는 별도 replay 억제가 있다. H1은 표준 update의 수신 시점에 binding·prompt 세대를 고정하고 기존 사용자 메시지 저장/표시 경로에 연결한다. host는 Assistant text/thinking을 segment로 버퍼링한다. 추가 사용자 행 앞에서 기존 segment를 flush·종료하고, 사용자 행을 저장·전달한 뒤 같은 턴의 다음 assistant 출력을 새 segment로 연다. live 화면과 다시 연 이력 모두에서 “이전 답변 → 추가 입력 → 다음 답변”의 순서·내용·개수를 검사한다. host가 발급하는 assistant 행 ID는 wire의 messageId와 독립이다. [기존 segment 처리](../../crates/aionui-conversation/src/stream_relay.rs#L441-L485) · verified: `agent-client-protocol-schema-1.5.0/src/v1/client.rs::ContentChunk`의 `messageId`는 메시지 식별자다. [현재 번역 분기](../../crates/aionui-ai-agent/src/protocol/events/translate.rs#L45) · [load 억제](../../crates/aionui-ai-agent/src/protocol/acp.rs#L711-L736)

native 소스의 loop는 원래 prompt와 큐에서 꺼낸 메시지 모두에 user start/end를 발생시키고, follow-up을 drain한 뒤 loop를 계속한다. adapter의 현재 prompt 경로는 최초 입력을 자체 이력에 append하고 native prompt 뒤 idle을 기다린다. A1은 일반 prompt의 최초 사용자 batch를 분리하고 이후 drain된 user 메시지의 확정 내용당 한 번만 내보낸다. 같은 문장이 반복되거나 extension 훅이 메시지를 바꾸는 경우에도 native context의 최종 내용을 사용한다. 원본 prompt/반복된 텍스트/끝 경합/취소를 고정 native에서 캡처해 관찰 가능한 구분과 출력 순서를 입증한 뒤 입력 메서드를 광고한다. 임의 queue 길이·본문 일치·단독 `agent_end`를 소비 증거로 쓰지 않는다. verified: `aelix-ai@e19ab55e/packages/aelix-agent-core/src/aelix_agent_core/loop.py:108–143,187–211,268–278`, `aelix-acp@3bdb9451/src/aelix_acp/session.py::event,_prompt,wait_until_idle`. [정상·늦은 입력의 순서 예시](examples/running-input.json)

## 관측과 검증

저빈도 `info`에는 통로 협상·종료, binding 교체, 결과 불명에 대한 안전한 correlation ID와 단계만 남긴다. `warn`은 malformed·범위/한도 거절·gap을 집계/샘플링하고, `error`는 세션 바인딩 계약 위반에 쓴다. 고빈도 성공은 방향·건수·바이트·지연·outcome의 bounded 집계, `debug`는 단계별 타이밍이다. 원격에서 선택한 메서드 문자열을 무제한 metric label로 쓰지 않는다. payload/result/error.data, prompt, 키·경로·자식 메시지·헤더·토큰·원본 프레임은 모든 기본 로그, HTTP tracing, stderr 전달 및 crash 진단에서 제외한다. 공용 `acp.rs::send_request`는 요청과 응답을 직렬화해 로그 요약 경로로 건넨다. 통로에서는 크기/상태만 기록하는 송수신 경로를 쓰고 원격 오류 문구를 HTTP 5xx나 로그로 복사하지 않는다.

| 시험 | 검증할 결과 |
| --- | --- |
| Host wire 계약 | fixtures의 HTTP→실제 ACP stdio→HTTP, stdio→WS를 각각 대조한다. 누락·중복된 선행 underscore, id 혼선, unknown future field·Unicode·배열·null 변형을 검출한다. 새로운 가상 세션 메서드 하나를 ACP 광고에만 추가해 동일 host에서 양방향 통과를 보인다. 무광고/구버전·비Aelix·위장 agent, 표준 메서드와 `_aelix/manage`, 반대 방향 요청은 ACP 송신/WS 전달 0회다. 호환성 대조군에서 표준 prompt, permission, tool metadata가 기존과 같은 프레임과 이력을 낸다. |
| Auth·격리 | 보호된 router/실제 WS에서 로그인 전·만료·폐기 401, 잘못된/중복 Host·Origin, CSRF 없음/불일치, 위조 desktop, runtime/helper 및 비보호 local 리스너를 각각 검사한다(자격 증명 오류 401, 출처·권한 403, 기능 미등록 404). desktop·로그인 WebUI 각각 정상 POST/WS를 검증한다. owner A/B, 같은 owner의 서로 다른 대화/탭, 같은 프로세스의 다른 ACP 세션, 같은 connection/sessionId를 다시 load한 뒤 도착한 이전 nonce의 알림, 구독 해제·삭제·재시작 직전/직후를 경합시켜 잘못된 쓰기와 수신을 **0건**으로 단언한다. |
| 경계와 장애 | 바이트 한도 바로 아래·경계·바로 위, chunked, 중복 envelope 필드, 깊이, 양방향 burst·슬롯 고갈, 큰 오류 응답, 한 소켓만 느린 경우를 각각 고정 code·계속 동작하는 대조군과 확인한다. handoff 직전/직후·native 적용 뒤 reply 직전 연결 손실, 늦은 응답·알림, ACK 전후 재연결을 주입한다. 미발송 0건/전달 후 native 최대 1회, unknown·강제 resync, 늦은 결과가 새 binding에 들어가지 않음을 검사한다. 릴레이 전용 비밀 표식은 host·ACP adapter·renderer 로그, 진단, host DB, 비인가 소켓에 없어야 한다. 별개 표식을 넣은 실제 소비 사용자 텍스트는 U6-a 시험에서 owner의 일반 이력에 정확히 한 번 있어야 한다. |
| 첫 수직 검증 | #24에서 **실제 번들 adapter와 native**, 제어 가능한 모델 응답으로 턴을 열어 둔다. desktop와 로그인 WebUI에서 steer 1회와 follow-up 1회를 보내어 native ack, 현재/다음 처리 순서, 모드와 명시적 Stop의 경합, adapter가 이미 끝난 것으로 아는 stale turn 거절, ACP 송신과 native 수신 사이 턴이 끝나는 경합에서 보장한 접수 수준, 재접속 뒤 중복 입력 0건을 관찰한다. 가짜 agent의 echo만으로 이 항목을 통과시키지 않는다. |
| 후속 수직 검증 | #7은 native 파일과 다음 요청의 실제 도구·스킬을 관찰한다. project 파일 생성 승인/거절, global 쓰기 거부와 helper 경로, 잠금·deferred를 모두 확인한다. S3는 #612가 들어온 조합에서 동시에 실행되는 두 자식, 대상 하나만 변경, 다른 부모 거절, 승인 대기 중 이벤트 진행, epoch 전후 replay/영속 이력을 확인한다. synthetic model, 실 provider, 설치 target 근거를 구분한다. |

- **동시성과 수동 재확인.** project 동의 1건을 열어 둔 채 state·steer·자식 Stop의 전달을 검사한다. 5번째 동시 요청의 429, deadline 뒤 미회수 1·2·3건의 admission 전이, 늦은 reply·binding 폐기에서 예산 회수, 격리 중 표준 cancel을 단언한다. 모든 쓰기에서 동일 키의 동시/완료/결과 불명 재확인, 다른 입력과의 충돌, 만료·포화·재시작을 검사하고 native 호출 횟수를 센다.
- **다른 기기의 WebUI 시계.** ACP 시각을 고정하고 로그인한 브라우저 두 대의 시계를 각각 30분 늦게·빠르게 설정한다. 각 기기가 만든 UUIDv7으로 첫 쓰기가 모두 접수되는지 확인한다. ACP가 599,999 ms에 처리한 lookup은 저장된 결과를 반환한다. 같은 가상 시계에서 599,999 ms에 출발한 별도 lookup을 지연시켜 600,001 ms에 도착하게 한다. 600,000 ms 경계와 그 이후에는 퇴출 전후 모두 unavailable, 조회에 의한 native 호출 0회, 최초 쓰기를 포함한 실행은 1회인지 검사한다. renderer가 상태·이력만 읽고 쓰기를 재발행하지 않는지도 확인한다. [시계 오차별 요청·기대값](examples/running-input.json)
- **완료 조회 프레임 크기.** 기한 안의 저장 결과로 lookup 응답 전체가 262,143 / 262,144 / 262,145 B가 되도록 구성한다. 앞의 두 경우에는 온전한 결과를, 마지막에는 작은 `unavailable` 프레임 하나만 stdout에서 관찰한다. 최초 쓰기 응답이 정확히 262,144 B이고 조회 후보가 262,176 B인 예시도 같은 대체 경로를 검증한다. 예시의 32 B 차이는 명시한 compact JSON과 같은 자릿수 ID에서 나온다. 구현 시험은 실제 encoder와 조회 ID로 측정한다.
- **ACP 등록표가 브라우저 노출 경계다.** A1부터 등록한 session typed handler와 방향의 정확한 snapshot을 승인된 D16 fixture와 대조한다. 메서드 추가 시 snapshot의 실제 diff를 리뷰한다. 후보별 schema·허용 scope와 native 호출 대상을 검증하고, arbitrary method/command/path를 받는 범용 dispatcher, `_aelix/manage`, 쉘·파일 프록시나 전역 handler를 등록하면 실패시킨다. 이름만 바꿔 감싼 프록시도 body/negative dispatch 시험에서 잡는다. H1 전용과 H2, #612 미지원/지원 조합의 광고를 따로 검사한다.
- **이력·과부하.** 원래 prompt가 두 번 저장되지 않는지, 소비 전후 및 동일 문장 두 건·낱개 취소·load replay·turn 종료 뒤 늦은 이벤트에서 행 수, 메시지 ID, 실제 turn과 출력 순서를 검사한다. 30/s 초과 원천 이벤트를 발생시켜 aggregator의 10/s, terminal 반영, native cursor로 누락 복구, renderer의 단일 resync와 backoff 상한, 다른 소켓 진행을 검사한다.

## 구현 PR 순서와 선행 관계

| PR | 선행과 완료 조건 |
| --- | --- |
| H1 · host 요청과 표준 입력 이력 | U5-b의 두 조건, protected admission, binding, v1 발견/POST, 동시 요청·결과 불명 경계를 구현한다. U6-a의 live user chunk 저장·표시, 원래 prompt 중복 방지, load 억제, prompt 종료 fence를 **같은 PR**에서 검증한다. H1은 요청만 협상한다. [바이트 측정과 큰 표준 프레임 회귀](stdio-measurement.md)를 이 PR에 첨부한다. |
| A1 · #24 ACP | H1 계약을 기준으로 state/steer/follow-up, 쓰기 requestId 처리와 기존 결과 전용 requests/lookup, 소비 시 표준 chunk와 prompt 완료 순서, 승인된 광고 snapshot을 구현한다. H2와 병행할 수 있다. |
| Studio #24 | **H1 + A1의 branch SHA**로 독립 클라이언트와 기존 입력 UI를 연결한다. desktop·로그인 WebUI에서 실제 입력, 늦은 큐 표시, 소유자의 재열기 이력, 표준 Stop을 검증한다. H2와 supervisor 변경은 선행이 아니다. |
| H2 · host 확장 알림 | H1 위에 기존 realtime router의 인증된 구독·nonce 검사·ordered unicast·폐기·backpressure를 붙인다. 통과한 조합만 notifications=true를 광고한다. H2 병합 전에도 #24는 H1으로 검증한다. `AppServices`가 조립하며 별도 port·worker는 두지 않는다(D12). |
| A2 → Studio #7 | H1 요청 경로, A1 공통 중복 방지, native selection pin을 재사용한다. ACP에서 scope 투영·쓰기 제한·동의·기능 거절을 검증한 뒤 Studio가 비활성 세션 안내와 선택 UI를 연결한다. 표준 permission은 H2에 의존하지 않는다. |
| A3 → Studio S3 | #612의 확정 wire와 H1 요청/H2 알림이 선행이다. ACP가 부모별 제어·cursor·병합·등록표를 검증하고 renderer는 독립 모듈로 자식 패널과 backoff를 연결한다. 그 전에는 children 메서드를 광고하지 않는다. #612가 beta.3에 빠지면 S3는 다음 릴리스 대상이다. |
| 통합 | 각 기능에서 host/ACP/Studio SHA와 native wheel hash를 함께 남긴다. 승인된 D16 통합 시점에 host release와 Studio pin을 올리고 설치본에서 확인한다. |

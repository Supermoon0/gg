# GG Browser 개발 체크리스트

기준일: 2026-07-23. 우선순위는 P0(안전·정확성)부터 P3(제품화) 순서다.
각 항목은 구현, 회귀 테스트, 실제 실행 경로 확인까지 끝나야 완료로 본다.

## 현재 진행 요약

> 전체 궤적(처음부터 완성까지)과 완성의 정의는
> [completion-checklist.md](completion-checklist.md)에 있다. 이 문서는
> 기능 단위의 상세 이력을 담는다. 두 축을 혼동하지 않기 위해 적어두면 —
> 아래 "P0 9/9, P1 8/8, P2 17/17 완료"는 *기능* 축이고, *정합성* 축은
> Test262 26.67%로 별개다.


- 완료: P0 9/9, P1 8/8, P2 17/17, Boa 제거와 gg-js 단일화
- 진행할 핵심: renderer crash isolation 완료 → shared frame/network service →
  renderer sandbox/quota → P3 제품화
- 제품화: P3 0/6
- 검증 부채: 새 GitHub Actions workflow의 Windows/Linux 최초 green run 확인

## 바로 진행할 작업 큐

### 0. 실사용 전환 검증 기반

- [x] Boa 런타임·소스·Cargo 의존성 제거
- [x] 브라우저와 헤드리스 드라이버를 gg-js 단일 경로로 통일
- [x] push/PR용 Windows·Linux Rust 및 브라우저 통합 CI 작성
- [x] network/CSS/JSVM 검증기를 실패 종료 코드가 있는 gate로 전환
- [x] 외부 사이트 바스켓을 화·금 정기 및 수동 workflow로 분리
- [x] GitHub에서 CI 최초 실행 후 플랫폼별 실패 수정 — **완료
      (2026-07-25, run #30 = 30149478741: Windows/Linux Rust + 통합
      4개 잡 전부 green)**. 고친 것 네 가지: ① 런 1~26이 잡 0개로 즉시
      실패한 근본 원인 = 세 워크플로 job 레벨 `env`의
      `${{ runner.temp }}` 표현식(그 자리에서 `runner` 컨텍스트는
      무효 → 파일 전체 무효화; push 이벤트는 파스 에러를 숨김) →
      첫 스텝에서 `$GITHUB_ENV` 주입으로 교체, ② Windows 콘솔
      cp1252에서 한글 테스트 출력이 UnicodeEncodeError →
      `PYTHONUTF8=1`, ③ Linux CSS gauntlet의 구식 기대값 2종(abs 박스
      ICB 원점, line-height 40px 계약)을 현행 엔진 동작으로 갱신,
      ④ Windows 8.3 단축 경로와 resolve된 경로의 relative_to 불일치
      (test_capped_selection_is_round_robin).
- [x] WPT 정적 testharness 하위 집합 실행기와 JSON 점수판 작성
- [x] test262 하위 집합 실행기와 기능·variant별 실패 분류
- [x] 내장 9개 계약 probe 기준선을 PR CI 회귀 gate로 연결
- [x] 고정 upstream SHA에서 공식 하위 집합 최초 기준 점수 확인·저장
- [x] [브라우저/renderer/network IPC 설계](process-isolation-ipc.md)와 완료 조건 확정
- [x] `RendererSession`/`NetworkBackend` process-neutral interface 추출
- [x] renderer child, 검증된 JSON IPC, crash recovery 구현
- [ ] shared-memory frame triple buffer와 browser chrome composite 구현
- [x] cookie/cache/socket을 network service로 이동하고 IPC gauntlet 통과
      (2026-07-25, browser/process/network_host.py) — 쿠키 jar·커넥션
      풀·HTTP 캐시는 **프로필의 권한 그 자체**다(사용자가 방문한 모든
      사이트의 자격 증명). 브라우저 프로세스에 두면 그 프로세스의 버그
      하나가 전부에 닿으므로, renderer가 이미 쓰던 검증된 JSON 채널
      뒤로 옮겼다. `GG_NETWORK_MODEL=service`로 선택한다(기본은 아직
      local — 제품 기본 전환은 M4).
      전송 설계에서 두 가지가 결정적이었고 둘 다 "브로커가 병목이
      되지 않기"에 관한 것이다. ① **request-id 라우팅**: 페이지 로드는
      서브리소스 fetch 열몇 개를 워커 스레드로 부채질하는데, lock-step
      파이프면 전부 직렬화된다. 브라우저 쪽이 리더 스레드 하나로
      `reply_to`를 맞춰 각 호출자에게 자기 응답을 준다 — **3초짜리 6개가
      18초가 아니라 3.01초**에 끝나는 것으로 확인. ② **I/O에서 블록하지
      않는 리더 루프**: 서비스는 모든 fetch를 스레드 풀로 넘겨서 리더
      루프가 다음 요청, 무엇보다 **cancel**을 즉시 받는다 — 멈추려는
      그 요청 뒤에 줄 서는 cancel은 cancel이 아니다.
      예외 타입도 hop을 건너간다(`net.RequestCancelled`) — 호출자가
      취소된 탐색과 실패한 탐색을 타입으로 가르기 때문이다.
      kwargs 마샬링은 `browser/ipc/wire.py` 하나로 합쳤다(중복이 바로
      직전 쿠키 유출 버그의 원인이었다).
      게이트: net gauntlet 47/47 — 서비스 경로에서 **jar가 브라우저
      프로세스에 없음**·SameSite cross-site 차단·in-flight cancel·
      crash 복구 4종 추가, unittest 9종, 두 renderer 모델 모두에서
      실제 페이지 로드(탐색+서브리소스 CSS+쿠키) 확인.
      **crash 복구(2026-07-25)**: 죽은 서비스가 브라우저를 영구히
      못 쓰게 만들면 안 된다. 다음 요청이 서비스를 되살리고 그 요청을
      처리한다. 메모리 jar·캐시는 사라지는데 **그게 crash의 비용**이고,
      그러나 **capability는 브라우저가 준 것**이므로 이미 발급한
      context를 새 서비스에 같은 id로 재발급한다 — context를 들고 있던
      호출자는 서비스가 죽은 줄 모른다. 죽는 순간 in-flight였던 요청은
      정직하게 `NetworkCrashed`로 실패한다. gauntlet
      `service-crash-restarts-and-restores-grants` + unittest.
      **cookie replica 갱신(2026-07-25)**: `document.cookie`는 동기
      읽기라 VM이 replica를 들고 있는데(로드 시 seed), 그 뒤 아무도
      갱신하지 않았다. 즉 **`fetch()`로 로그인하면 서버가 준 세션 쿠키를
      페이지가 탐색 전까지 볼 수 없었다** — local/service 양쪽 모두. 이제
      fetch를 처리한 틱마다 jar의 현재 가시 상태로 스냅샷을 다시 민다.
      `seed_cookies`는 merge라 **줄어들 수가 없어서** 새로
      `set_cookie_snapshot`(clear 후 적용)을 추가했다 — 서버가 만료시킨
      쿠키가 페이지 시야에서 사라져야 로그아웃이 성립한다. jar가 이미
      HttpOnly를 걸러내므로 스냅샷은 정확히 스크립트가 알아도 되는
      것뿐이다. smoke 3종 + unittest(로그인 왕복).
      남은 후속: OS 샌드박스.
      이전 진행 기록: net gauntlet을 **IPC 경로에서도** 돌리기 시작했고
      (`ipc-initiator-survives-broker`, `ipc-cookie-samesite-cross-site-block`),
      그 과정에서 **isolated 모델의 쿠키 유출 결함**을 발견·수정했다
      (2026-07-25). renderer가 브라우저 프로세스로 넘기는 kwargs를
      JSON 스칼라만 통과시키는 필터가 걸러내고 있었는데,
      `site_for_cookies`는 `net.URL`이라 **조용히 사라지고 있었다**.
      이건 힌트 하나를 잃는 정도가 아니다: `_same_site_allows`는
      `None`을 **같은 사이트로** 읽고 `_mixed_content_blocked`는 막을
      게 없다고 읽는다. 실제로 gauntlet에서 cross-site 서브리소스 요청에
      `Cookie='server=secret; corsid=1'`(HttpOnly·SameSite=Lax)이
      실려 나가는 것을 확인했다. **네이티브 셸의 기본이 isolated
      모델이므로 제품 기본 경로의 결함**이었다. URL 값 kwarg는 이제
      문자열로 실려 반대편에서 `net.URL`로 복원된다.
- [x] Windows/Linux renderer sandbox와 CPU/RSS/blob quota 적용
      (2026-07-25, browser/process/sandbox.py — POSIX setrlimit
      CPU/AS/NOFILE/core + umask 077 + no_new_privs, Windows Job object
      메모리/프로세스 상한·kill-on-close. env 튜닝 가능
      (GG_RENDERER_CPU_S/MEMORY_MB/NOFILE, SANDBOX=0로 해제), 적용
      내역은 hello_ack로 브라우저에 보고. BlobStore에 스토어 누적
      256MB quota 추가. Linux에서 CPU 스핀 킬·1GiB 할당 폭탄 봉쇄까지
      실검증(unittest 15종); Windows Job object 경로는 CI 차단으로
      미실행 — CI 복구 후 확인 필요. seccomp급 syscall 필터는 후속)

### 1. Transition과 keyframes — 완료 (2026-07-25, browser/animation.py)

- [x] CSS의 `transition-*`/`animation-*` shorthand와 longhand 정규화
- [x] `@keyframes` 이름, `from`/`to`/백분율 frame 파싱 및 스타일 저장
- [x] 이전 computed style과 새 style 사이에서 시작값·종료값 캡처
- [x] color, opacity, 길이, transform의 안전한 보간기 구현
- [x] duration/delay, iteration-count, direction, fill-mode, play-state 적용
- [x] 단조 시계 기반 animation sampling과 rAF/timer frame 합류
- [x] paint-only 속성과 layout 속성의 무효화 경로 분리
- [x] tkinter와 native shell이 같은 frame scheduler를 사용하도록 연결
- [x] transition 종료·취소 및 DOM 제거 시 animation 정리
- [x] 단위·통합·시각 회귀 테스트 추가 (smoke 36종: 정규화·보간·엔진·layout 통합)

완료 조건 충족: transform/opacity transition과 다구간 `@keyframes`가 실제
프레임에서 움직이고(두 shell 모두 같은 `AnimationEngine.on_frame`을 frame tick에서
호출), 애니메이션이 없으면 기존 80ms/500ms idle 백오프를 그대로 유지한다.
남은 후속 후보: transitionend/animation* DOM 이벤트 발화, per-keyframe
timing-function 오버라이드.

### 2. iframe 문서 격리 — 완료 (2026-07-25, browser/frames.py)

- [x] 자식 문서의 URL/base URL, origin, cookie/storage context 분리
      (프레임마다 독립 gg-js Doc/세션, 쿠키는 자식 origin jar로만,
      allow-same-origin 없는 sandbox는 쿠키 전면 차단)
- [x] 부모-자식 event loop와 load/error lifecycle 연결
      (FrameManager.tick이 두 셸의 live tick에서 자식 이벤트 루프·애니메이션을
      구동, load/error는 부모 문서의 iframe 요소에 dispatch)
- [x] cross-origin DOM 접근 차단 — 문서 간 DOM arena를 공유하지 않으므로
      구조적으로 불가능; 드라이버는 Page.frame()으로 자식 문서 핸들 제공
- [x] same-origin 동기 스크립팅(contentDocument/contentWindow)과
      postMessage 채널 — **완료 (2026-07-25, browser/frame_bridge.py)**.
      문서마다 독립 VM이므로 `postMessage`는 **객체 그래프가 아니라
      바이트의 채널**이다: 보내는 쪽 VM에서 JSON으로 직렬화하고, 호스트가
      라우팅하고, 받는 쪽 VM이 자기 힙으로 파싱한다. 한 문서의 Value에서
      다른 문서로 가는 코드 경로가 **존재하지 않는 것**이 여기서
      cross-origin 격리가 정책 검사가 아니라 구조인 이유다. 구현:
      `window.postMessage`(기존 no-op 스텁 대체), `iframe.contentWindow`
      프록시(핸들별 캐시 — `e.source === f.contentWindow`가 성립),
      `window.parent`/`top`/`origin`/`frameElement`, MessageEvent를
      매크로태스크로 전달(따라서 항상 비동기)하며 `addEventListener`와
      `window.onmessage`를 **둘 다** 발화한다(dispatchEvent는 후자를
      부르지 않는다). 호스트 정책: `targetOrigin` 불일치는 조용히 폐기,
      sandbox에 `allow-same-origin`이 없으면 **같은 사이트라도 불투명
      origin**(`e.origin === "null"`), 컨텍스트 핸들은 호스트만 발급하므로
      문서가 받지 못한 컨텍스트는 이름조차 지을 수 없다. 순환 참조는
      DataCloneError처럼 throw, 함수는 null로 clone, 128KB 상한.
      cross-origin 프레임은 `contentWindow`는 주되 `document`/`location`은
      `undefined`, `contentDocument`는 null이다. 양쪽 셸 + 드라이버
      (`Page.pump_frames`) + isolated 프로세스 모델 전부 연결.
      함께 고친 것: `FrameManager.dispose()`가 프레임 예산을 반환하지
      않아 프레임 트리를 갈아끼우는 페이지가 쓰지도 않는 예산을 소진하던
      누수. smoke 7종 + IPC 경계 unittest.
      **contentDocument(2026-07-25)**: 자식 DOM을 부모 VM 안에
      **미러**로 세운다 — 핸들이 아니라 복사본이다. 호스트가 자식 arena를
      직렬화하고 부모가 자기 안에 `dom::Document`를 재구축한 뒤, 엔진의
      **진짜 셀렉터 엔진**(`dom_api::query`가 임의의 `&Document`를 받는다)과
      직렬화기가 그 위에서 돈다. 즉 `contentDocument.querySelector(...)`는
      문서가 자기 자신에게 쓰는 것과 **같은 코드 경로**이면서, 여전히 한
      문서의 힙에서 다른 문서로 가는 포인터는 없다.
      읽기: `getElementById`/`querySelector(All)`/`getElementsByTagName`/
      `body`/`documentElement`/`title`/`URL`, 요소의 `textContent`/
      `innerHTML`/`id`/`className`/`tagName`/`value`/`children`/
      `parentElement`/`getAttribute`/`hasAttribute`/`matches`. 프로퍼티는
      스냅샷이 아니라 **실제 accessor**다 — 미러는 쓰기나 재푸시로 아래에서
      바뀔 수 있고 스냅샷은 조용히 거짓말을 하게 된다. 요소 래퍼는 노드별
      캐시라 `d.getElementById('x') === d.getElementById('x')`가 성립한다.
      쓰기(`setAttribute`/`removeAttribute`/`textContent=`/`className=`/
      `value=`/`click()`)는 **미러에 먼저 적용하고 호스트가 실제 자식에
      재생**한다(스크롤 쓰기와 같은 낙관적 큐 패턴) — 같은 턴 안의
      read-after-write가 일관된다. 미러는 자식 DOM 버전이 움직였을 때만
      다시 푸시한다(재구축은 부모가 들고 있던 래퍼를 전부 무효화하므로).
      `load` 발화 **전에** 푸시하는데, `iframe.onload`에서
      `this.contentDocument`를 읽는 것이 same-origin 케이스의 전부이기
      때문이다. cross-origin·불투명 origin 프레임은 미러를 아예 받지
      못하므로 `contentDocument`가 null이다. 4000노드 상한.
      함께 추가: `Document::set_text_content`, `dom_api::query_within`
      (서브트리 한정 결과), `Doc.set_text_content` pyo3 seam.
      남은 후속: MessageChannel의 문서 간 port 전달, `frameElement` 실제
      노드, `innerHTML` 쓰기(파서가 필요).
- [x] iframe layout/clip/scroll과 중첩 hit-test 구현
      (대체 요소 300x150 기본, width/height 속성, 프레임 내부 휠 스크롤,
      자식 링크 클릭은 프레임 내 탐색)
- [x] navigation·CSP/sandbox 최소 정책 및 회귀 테스트
      (X-Frame-Options DENY/SAMEORIGIN, CSP frame-ancestors
      'none'/'self'/*/host, sandbox allow-scripts/allow-same-origin,
      srcdoc, 중첩 depth 3 + 총 16 프레임 상한, smoke 18종)
- [x] 프레임 자체 히스토리(뒤로/앞으로가 프레임 탐색을 되돌리기)
      (2026-07-25) — 프레임 안의 링크를 따라가는 것은 **그 자체로 세션
      히스토리 항목**이다. 따라서 뒤로가기는 페이지를 다시 받는 것이
      아니라 프레임을 되돌려야 한다: `HistoryEntry.frame_state`가 깊이별
      **문서 순서 경로**(`(0,)`, `(0,1)` ...)로 프레임마다 한 행을 들고
      있고, 프레임 항목은 앞 항목의 url·body 객체를 **그대로 재사용**하므로
      복원 시 같은 문서임을 `is` 비교로 알아보고 렌더를 건너뛴다(다시
      렌더하면 프레임을 버리게 된다). arena index가 아니라 위치 경로를
      쓰는 이유는 DOM 재구축마다 index가 갈리기 때문이고, 이는 form-state
      복원이 이미 하고 있는 것과 같은 거래다. 부모 프레임이 탐색하면 그
      **자식 행들은 폐기**되며(그 문서가 교체되므로 자식이 존재하지 않는다),
      복원은 얕은 경로부터 재생한다(부모 탐색이 자식 프레임을 다시 만들기
      때문). `FrameDocument.navigate`는 load **전에** 호스트에 알린다 —
      스냅샷해야 할 것은 떠나는 상태이기 때문이다. 양쪽 셸 + 드라이버
      (`FramePage.navigate`/`post_message`). smoke 8종.

### 3. 접근성 트리 확대 — 완료 (2026-07-25, browser/accessibility.py)

- [x] 기존 implicit/explicit role snapshot에 accessible-name 계산 확대
      (Python 트리와 Rust snapshot()이 같은 역할 맵·이름 우선순위를 미러;
      landmark·리스트·테이블·img·dialog·slider 등 역할 대폭 추가)
- [x] `aria-label`/`aria-labelledby`/`aria-describedby`와 hidden 상태 반영
      (display/visibility/hidden/aria-hidden 서브트리 프루닝,
      labelledby는 숨겨진 대상도 참조 가능)
- [x] checked/selected/expanded/disabled/value 상태 노출
      (+required/readonly/pressed/heading level, aria-* tristate가 네이티브
      상태를 오버라이드)
- [x] label-control 관계, heading level, landmark 계층 검증
      (label[for]/감싸는 label, aria-level 우선, landmarks()/headings()
      아웃라인, 이름 없는 form/section은 landmark 제외,
      article 내부 header/footer는 banner/contentinfo 제외)
- [x] 키보드 focus/activation과 접근성 snapshot 일관성 테스트
      (focus order ⊆ ax-focusable ∪ aria-hidden, disabled 제외,
      focused 마킹, checked 토글이 다음 트리에 반영 — smoke 21종)

후속 후보: aria-live/알림, aria-activedescendant, 접근성 트리의 Rust
계층화(현재 Rust는 flat snapshot, 계층 트리는 Python).

### 4. 릴리스 검증 게이트

- [x] 현재 Rust/Python 소스로 release wheel 재빌드·재설치
- [x] PR용 Windows/Linux 자동 검증 workflow 추가
- [x] 실사이트 바스켓 정기·수동 workflow 추가
- [x] Python smoke 전체 실행 — **437/437 통과 (2026-07-29, Windows)**
- [x] network/CSS/JSVM gauntlet 재실행 — **network 47/47, CSS 23/23,
      JSVM promised CLAIM 전부 통과 (2026-07-29)**
- [x] home/demo/css/js render evidence 재생성 — **4종 생성·육안 확인
      (2026-07-29)**. 이 과정에서 `<circle>`·`<rect>`가 회색 broken-image
      placeholder로 나오던 SVG shape 누락을 발견해 path lowering과 smoke
      회귀 검증을 추가했다.
- [x] 네이버와 사이트 바스켓 실브라우징 회귀 — **9곳 완주, 8곳
      읽을만함·의도적 소형 페이지 example만 빈약 (2026-07-29, Windows)**

## P0 — 안전성과 동적 렌더 정확성

- [x] 쿠키 header injection 차단과 thread-safe jar
- [x] Domain/Path/Secure/HttpOnly/SameSite/Expires/Max-Age 적용
- [x] `__Secure-`/`__Host-` 접두사와 JS의 HttpOnly 덮어쓰기 차단
- [x] `document.cookie` 속성을 네이티브 VM에서 네트워크 jar까지 보존
- [x] 쿠키가 있는 응답 및 `Set-Cookie` 응답의 URL-only 캐시 오염 차단
- [x] 실제 DOM version으로 비동기 settle 변이 판정
- [x] self-rescheduling `requestAnimationFrame`의 초기 로드 폭주 방지
- [x] `position: fixed`를 viewport 원점 기준으로 검증
- [x] Rust/Python/network/CSS/pixel-render 회귀 게이트 통과

## P1 — 웹 요청·탐색의 기본 정확성

- [x] POST 폼, request body, content type 및 파일 업로드의 최소 지원
- [x] 301/302/303/307/308의 method/body 보존 규칙 구현
- [x] submit/reset 이벤트, constraint validation, 외부 `form=` owner
- [x] origin 모델과 CORS/preflight/credentials/mixed-content 정책
- [x] 쿠키의 Public Suffix List와 schemeful site 판정 정밀화
- [x] 캐시 `no-store`/`private`/`Vary`/ETag/Last-Modified 재검증
- [x] 탐색 취소, 네트워크 timeout, 뒤로/앞으로 history state 복원
- [x] 키보드 탐색, focus 순서, 기본 버튼·링크 동작 회귀 테스트

## P2 — 사이트 호환성

- [x] `script async`/`defer`/동적 삽입의 실행 순서 완성
- [x] ES modules 정적 import/export와 URL별 fetch/cache 그래프
- [x] 리터럴 동적 `import()`의 지연 평가, Promise 정산, 실패 격리와 URL 캐시
- [x] 비리터럴 `import(expression)`의 실행 시 fetch, import-map 해석과 약한 수명 관리
- [x] top-level await의 의존성 순서 및 DOMContentLoaded 대기
- [x] import maps exact/prefix/scopes 해석과 파싱 오류 전파
- [x] named/default import의 일반 식별자 읽기 live binding
- [x] 정적·동적 import attributes와 JSON module default export
- [x] shadowing·축약 속성·템플릿·선언 위치를 포함한 lexical live binding
- [x] Proxy/Reflect 실제 시맨틱과 React main 번들 렌더·커밋
- [x] table rowspan/colspan과 grid 숫자·named-area span
- [x] grid named line·명시 row 배치와 occupancy 기반 자동 배치
- [x] `position: sticky` 스크롤 시점 paint·containing-block 제한·hit-test
- [x] 부모-자식·빈 블록과 양수/음수 집합을 포함한 margin collapsing
- [x] transition/@keyframes와 렌더 프레임 스케줄링
- [x] iframe과 문서별 origin/event loop 격리
- [x] 접근성 트리와 ARIA role/name/state 확대

## P3 — 제품화·성능·격리

- [ ] 탭, 다운로드, 북마크, 권한·인증서 오류 UI
- [ ] 브라우징 데이터 삭제와 프로필별 저장소 분리
- [ ] [페이지/네트워크 프로세스 격리와 리소스 제한](process-isolation-ipc.md)
- [ ] 크래시 복구 및 세션 복원
- [ ] 프로파일 기반 GC/JIT/레이아웃 Rust 이식 여부 결정
- [ ] Windows 패키징과 CI 회귀 실행, 릴리스 서명

## 네이버 실측 대조 (2026-07-26, 실제 Chrome 렌더와 1:1 비교)

참고 스크린샷과 대조해 **엔진 결함 3건을 찾아 수정**했다.

- **컴파일 실패: `var a={},b={},…` 다중 선언자.** 레지스터가 u8이라
  함수당 250개인데, 초기화식의 temp를 **선언자마다 회수하지 않고**
  문장 끝까지 들고 있었다. minifier는 선언자 100개 넘는 문장을 그냥
  뱉는다(core-js가 그렇다) → `expression too deep`으로 **번들이 아예
  컴파일되지 않았다**. `var`/`let`/`const` 세 경로 모두 수정.
  실제로 실패하던 naver 쇼핑 프레임의 core-js 폴리필(91KB)이 이제
  컴파일·실행된다.
- **`document.currentScript`가 undefined.** 로더가
  `document.currentScript.getAttribute('src')`로 자기 태그를 찾는 것은
  가장 흔한 관용구인데, 그 첫 줄에서 throw했다(쇼핑 프레임에서
  `.getAttribute() of undefined` 14건). 호스트가 스크립트 실행 동안만
  노드를 지정하는 seam(`set_current_script`)을 추가 → **14건 → 0건**.
- **`firstElementChild`/`lastElementChild` 미구현** → undefined.
  텍스트 노드를 건너뛰도록 구현하고, 없을 때는 다른 DOM 순회와 같이
  **null**을 답한다(undefined면 `if (el.firstElementChild)` 체인이 깨진다).

결과: DOM 1,268 → 1,436~1,628 엘리먼트, 페인트 1,539 → 1,926 명령.
쇼핑 탭 줄·책방 신간/베스트셀러·푸터가 새로 렌더된다.

정정: 처음에 "우측 레일이 잘린다"고 본 것은 **엔진 버그가 아니었다** —
`#wrap { min-width: 1340px }`인 페이지를 1280px로 렌더한 탓이다. 1440px
(참고 스크린샷과 같은 폭)에서는 증시 숫자까지 온전히 나온다.

미해결: 일부 서빙 변형에서 settle 중 **문자열 힙 폭주**로 512MB backstop이
걸리며 페이지가 218 엘리먼트로 붕괴한다(오늘 ~14회 중 2회). 정상 실행은
**35MB**만 쓰므로 상한이 빠듯한 게 아니라 진짜 폭주이고, 근본 원인은
기록해 둔 GC 부재다. 온디맨드 재현이 안 돼 이번 수정이 유발한 것인지
원래 있던 것인지는 **확인하지 못했다**. 진단용으로 `GG_JS_HEAP_MB`
튜닝과 `Doc.heap_bytes()`를 추가했다(기본값은 종전 512MB 그대로).

## 실사이트 바스켓 소견 (2026-07-25, 첫 CI 실행 → 원인 규명·수정)

- 네이버 "실패"(218 노드)의 원인 규명: 봇 차단 아님 — 엔진은 전체
  홈페이지(254KB)를 그대로 받고(GGBrowser UA·curl·Chrome UA 응답 동일)
  스크립트도 오류 0으로 실행된다. `basket_test.py`가 **settle 전에
  측정**해서 React 피드가 마운트되기 전의 쉘(218 엘리먼트)을 채점한
  것이 원인. `settle_lazy` 후에는 1,622 엘리먼트/531 텍스트/4,134px로
  "읽을만함"이다. (07-19의 546 텍스트는 당시 기본이던 reader-mode
  주입의 수치 — 현재는 GG_READER=1 opt-in.)
- 수정: basket이 사이트마다 **샌드박스된 자식 프로세스**(rlimit +
  120s 타임아웃)에서 `load_document → settle_lazy(20s) → 채점`을
  수행한다. 폭주 사이트는 자기 프로세스만 죽는다 — 나무위키 측정 중
  hosted 러너가 shutdown signal(143)로 죽던 문제의 가드.
- 수정 후 로컬 재측정(2026-07-25): **네이버 1,225 엘리먼트/409 텍스트
  → 읽을만함 회복**. 위키백과·티스토리·HN·MDN 읽을만함, example 빈약
  (원래 소형 페이지). GitHub 러너 재실행(run #29, workflow green)도
  동일: 네이버 1,133/400 읽을만함, 나무위키·정부24 자식 abort로 격리,
  러너 생존. 연합뉴스는 CI에서도 ConnectionReset — 환경 문제가 아니라
  서버가 이 클라이언트(TLS 지문/보안장비 추정)를 끊는 것으로 보인다.
- 나무위키·정부24 settle 중 메모리 폭주 → **gg-js VM 하드닝으로 해결
  (2026-07-25)**. 원인은 gg에 GC가 없어 단일 대형 할당·누적이 프로세스를
  abort시킨 것. 다섯 경로를 catchable RangeError로 전환:
  ① 로프 문자열 무한 배가(`s+=s`)와 `repeat`/`padStart` 거대 카운트
  → 64MB 문자열 상한, ② `split`/`Array.from(string)`/전역 regex match
  대량 원소화 → 100만 원소 상한, ③ 거대 배열 length·희소 인덱스(gov.kr)
  → 1,600만 원소 상한(SetIndex/SetProp 핫패스 포함), ④ 대형 문자열 누적
  (namu Cloudflare 챌린지) → 512MB 힙 바이트 백스톱을 dispatch 루프에
  추가. 결과: **더 이상 프로세스 abort 없음**, 러너가 9곳 완주. 나무위키
  "실패"(18노드)는 Cloudflare "Just a moment" 챌린지(5.6KB)를 받는
  것으로 실제 봇월 — OOM이 아니라 정상. 정부24 "시간초과"는 배열 폭주는
  막혔으나 스크립트 루프가 명령어 예산(400M)을 다 태워 120s 소요, 프로세스
  타임아웃이 봉쇄.
- 정부24 시간초과 재진단·해결(2026-07-29): Nuxt 루트 모듈에 있는
  **1,240개 literal dynamic import 후보를 정적 import처럼 전부 선취**하던
  모듈 그래프가 원인이었다. 동적 import를 실제 실행 시점에만 해석·fetch하도록
  고쳐 **120s 시간초과 → 5.0s, 1,154 엘리먼트/80 텍스트, 읽을만함**으로
  회복했다. 전체 9곳 재측정도 예외·시간초과 없이 완주했다.
- **후속 과제**: (a) 실행 fuel이 명령어 수 기반이라 무거운 유한 루프가
  벽시계로 오래 걸릴 수 있다 — load 내 스크립트별 벽시계 예산 검토.
  (b) 근본 해결은 GC(현재 문자열 힙·객체가 문서 수명 내내 단조 증가).
- 폼 컨트롤·overflow 스크롤 적용 후 재측정(2026-07-25): 바스켓 판정은
  그대로다(네이버·위키백과·티스토리·HN·MDN 읽을만함). 네이버·HN의 수치
  변동은 실사이트 콘텐츠 변화이며, **고정 덤프 A/B로 회귀가 아님을
  확인**했다 — 같은 입력에서 naver/HN/MDN 모두 element·text·height가
  기준(4266eea)과 동일하고, 늘어난 paint 명령은 새 입력 클리핑
  브래킷뿐이다(HN +4, MDN ±0).

## 장기 호환성 주차장

핵심 P2/P3를 지연시키지 않되, 범위에서 잃어버리지 않을 항목이다.

- [ ] inline-block 정식 formatting context와 baseline 정렬
- [x] `overflow:auto` 내부 스크롤 영역과 중첩 스크롤 입력 (2026-07-25)
      — 명시 크기를 가진 `overflow:auto|scroll` 박스가 자체 스크롤러가
      된다: 자식을 클리핑하고 노드에 보관한 스크롤 오프셋만큼 페인트
      시점에 이동시킨다(iframe 내부 스크롤과 같은 구조). 오버플로 시
      박스 안쪽에 스크롤바 표시, 히트테스트가 sticky·스크롤 오프셋을
      합성, 휠은 가장 안쪽의 아직 움직일 수 있는 스크롤러부터 소비하고
      끝에 닿으면 바깥/페이지로 넘긴다(scroll chaining). 내용 크기로
      자라는 auto 박스는 예전처럼 클리핑하지 않는다(스크롤할 수 없는
      내용을 감추지 않기 위함). 덤으로 `translate_cmds`가 중첩 clip
      브래킷의 실제 rect(clip_top/clip_bottom)를 함께 옮기도록 수정 —
      transform·iframe 경로에도 있던 잠복 버그. smoke 18종.
      후속(일부 완료): `scrollTop`/`scrollLeft`/`scrollHeight`/
      `scrollWidth` DOM 프로퍼티 연결 완료(2026-07-25) — 셸이 레이아웃
      후 실제 스크롤 상태를 VM에 밀어넣고(`set_scroll_state`,
      `set_layout_rects`와 같은 패턴), 페이지가 쓴 `el.scrollTop = n`은
      `take_scroll_writes`로 회수해 라이브 틱에서 실제 스크롤러에
      적용·클램프한다(채팅 로그 자동 스크롤 관용구가 동작). 또한 노드에
      두던 스크롤 오프셋이 **네이티브 트리 재구축마다 초기화되던 결함**을
      수정 — arena index 기준 스냅샷을 focus/hover와 같은 지점에서 복원.
      스크롤 메서드 완료(2026-07-25): `el.scrollTo/scroll/scrollBy`,
      `el.scrollIntoView()`, `window.scrollTo/scroll/scrollBy`,
      그리고 라이브 `window.scrollY`/`pageYOffset`/`scrollX`/
      `pageXOffset`. 두 가지가 설계상 중요했다.
      (1) **상대 스크롤은 위치가 아니라 델타로 전달**한다 — VM은
      레이아웃을 볼 수 없어 같은 턴에 먼저 일어난 `scrollIntoView()`가
      스크롤러를 어디로 옮겼는지 모르기 때문이다. 호스트가 실제 위치에
      대해 델타를 적용하므로 `scrollIntoView(); window.scrollBy(0,-80)`
      (sticky 헤더 보정 관용구)가 의도대로 동작한다.
      (2) 두 큐(`take_scroll_writes` / `take_scroll_into_view`)에 공유
      **시퀀스 번호**를 찍어 호스트가 **페이지가 호출한 순서 그대로**
      한 번에 재생한다(`apply_scroll_requests`). 어느 큐를 나중에
      비우느냐가 결과를 바꾸지 않는다. 또한 `window.scrollTo`는 실제
      브라우저처럼 동기적이어야 하므로 VM이 window 프로퍼티를 즉시
      갱신하고, 호스트가 아직 적용하지 않은 쓰기가 남아 있는 노드는
      `set_scroll_state`가 **덮어쓰지 않는다**(호스트의 보고가 stale).
      함께 고친 프로세스 격리 결함 2건: JSON IPC가 모든 행을 list로
      디코드해 pyo3의 tuple 추출이 거부하면서 `set_layout_rects`가
      isolated 모델에서 **첫 레이아웃마다 페이지 로드를 죽이고 있었고**,
      스크롤 시맨틱 3종이 프로토콜 allowlist에 없어 조용히 무시되고
      있었다. 이제 local/isolated 결과가 일치한다. smoke 18종 추가.
      남은 후속: 스크롤바 드래그, 포커스된 스크롤러의 키보드 스크롤,
      `scroll-behavior: smooth`, `scrollIntoView({block, inline})`.
- [x] `<select>`·체크박스·라디오의 네이티브 수준 렌더링 (2026-07-25)
      — 엔진이 위젯 페이스를 직접 그린다: 체크박스(테두리 상자 + 체크
      상태의 체크마크 2획), 라디오(원 + 선택 시 점), select(닫힌 컨트롤
      박스 + 선택된 option 라벨 + 셰브론), textarea(현재 값 다중 행,
      클리핑), submit/reset 버튼 페이스, 포커스 링. 텍스트 필드 값은
      컨트롤 박스에 클리핑되고 password는 마스킹된다. 페이지가 자체
      스타일을 준 컨트롤(`appearance:none` 관용구 = author background/
      border)은 자기 모양을 유지하고 상태 표시만 위에 그린다.
      `<select>`/`<textarea>`는 대체 컨트롤이라 자식이 흐르지 않는다
      (이전에는 모든 `<option>` 텍스트가 페이지에 나란히 렌더됐다).
      상호작용: 팝업 레이어가 없으므로 클릭/Enter/Space/방향키가 선택을
      순환하고 input·change를 발화하며 제출값이 따라간다. `<option>`
      자동 닫힘을 두 HTML 파서에 추가.
      후속: 팝업 목록 UI, `size`/`multiple` 리스트박스, optgroup 그룹핑.
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] HTTP/2 연결 및 캐시 동작 검증
- [ ] `<video>`/`<audio>`/WebGL
- [ ] HTML5 오류 복구·quirks mode·EUC-KR 등 레거시 인코딩
- [ ] 사이트 바스켓을 쇼핑몰·SPA·커뮤니티 포함 20개 이상으로 확대

## 현재 검증 기준

- Rust: 221 passed, 2 ignored (2026-07-25)
- Python smoke: 382/382 (2026-07-25, Linux + 새로 빌드한 native wheel;
  transition/@keyframes 36종 + iframe 18종 + 접근성 21종 + 폼 컨트롤 20종 + overflow 스크롤 27종 포함. native
  shell 구간은 local renderer를 명시해 top-level 스크립트의 spawn
  재import 자폭을 제거)
- Golden 렌더링 회귀: 8/8 (form-controls 픽스처 추가)
- Network gauntlet: 41/41
- CSS gauntlet: 23/23 (2026-07-25 — position-absolute-offsets는 ICB
  원점 기준으로, line-height-px는 "명시 40px == 40px 라인박스" 계약으로
  기대값을 현행 엔진 동작에 맞게 갱신; GitHub ubuntu 러너에서도 동일하게
  실패하던 2종이었다)
- Render evidence: home/demo/css/js 4종 통과

## 최근 구현 메모

현재 모듈 상태 머신은 URL별 한 번만 평가하며 리터럴·계산식 동적 import,
top-level await, import maps, JSON import attributes, 순환 의존성, Promise 실패
격리와 DCL 대기에 더해 scope-aware import live binding까지 실제 로더 경로에서
검증했다.

Proxy/Reflect는 객체와 callable target을 구분하고 `get`/`set`/`has`/
`deleteProperty`/`ownKeys`/descriptor/prototype/확장성, `apply`/`construct`,
`Proxy.revocable`을 VM 내부 연산으로 연결한다. `new`는 단일 `Construct`
바이트코드로 통합해 callable Proxy의 `apply`와 `construct` trap을 구분한다.
trap 부재 시 Reflect와 일반 문법이 같은 ordinary 연산으로 위임하며,
중복 own key·폐기된 Proxy·비확장 target 위반은 TypeError로 거절한다.

Grid template은 `[name]` line group과 `repeat()` 안의 중복 line을 track과
분리해 보존한다. `grid-column`/`grid-row`는 양수·음수 line, named line,
`span N`, 네 부분 `grid-area`를 공통 해석하며, 명시 배치를 먼저 occupancy에
예약해 앞선 auto item도 그 칸을 덮지 않는다. `position: sticky; top:*`은
Python에서 매 프레임 display list를 만들지 않고 kind 8/9 marker를 통해 Rust
rasterizer가 scroll offset과 containing-block 하한을 적용한다.

Vertical margin collapsing은 모든 adjoining margin을 `max(양수)+min(음수)`로
계산한다. 부모의 첫/마지막 normal-flow block, 중첩된 첫 자식, zero-height empty
block 체인을 같은 집합으로 전파하고, border/padding·명시 높이·overflow BFC·
float/out-of-flow·clear 경계에서는 부모-자식 collapse를 중단한다.

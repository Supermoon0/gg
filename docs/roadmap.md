# GG Browser 개발 체크리스트

기준일: 2026-07-23. 우선순위는 P0(안전·정확성)부터 P3(제품화) 순서다.
각 항목은 구현, 회귀 테스트, 실제 실행 경로 확인까지 끝나야 완료로 본다.

## 현재 진행 요약

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
- [ ] cookie/cache/socket을 network service로 이동하고 IPC gauntlet 통과
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

### 2. iframe 문서 격리 — 코어 완료 (2026-07-25, browser/frames.py)

- [x] 자식 문서의 URL/base URL, origin, cookie/storage context 분리
      (프레임마다 독립 gg-js Doc/세션, 쿠키는 자식 origin jar로만,
      allow-same-origin 없는 sandbox는 쿠키 전면 차단)
- [x] 부모-자식 event loop와 load/error lifecycle 연결
      (FrameManager.tick이 두 셸의 live tick에서 자식 이벤트 루프·애니메이션을
      구동, load/error는 부모 문서의 iframe 요소에 dispatch)
- [x] cross-origin DOM 접근 차단 — 문서 간 DOM arena를 공유하지 않으므로
      구조적으로 불가능; 드라이버는 Page.frame()으로 자식 문서 핸들 제공
- [ ] same-origin 동기 스크립팅(contentDocument/contentWindow)과
      postMessage 채널 — VM 간 브리지가 필요한 후속 작업
- [x] iframe layout/clip/scroll과 중첩 hit-test 구현
      (대체 요소 300x150 기본, width/height 속성, 프레임 내부 휠 스크롤,
      자식 링크 클릭은 프레임 내 탐색)
- [x] navigation·CSP/sandbox 최소 정책 및 회귀 테스트
      (X-Frame-Options DENY/SAMEORIGIN, CSP frame-ancestors
      'none'/'self'/*/host, sandbox allow-scripts/allow-same-origin,
      srcdoc, 중첩 depth 3 + 총 16 프레임 상한, smoke 18종)
- [ ] 프레임 자체 히스토리(뒤로/앞으로가 프레임 탐색을 되돌리기)

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
- [ ] Python smoke 전체 실행
- [ ] network/CSS/JSVM gauntlet 재실행
- [ ] home/demo/css/js render evidence 재생성
- [ ] 네이버와 사이트 바스켓 실브라우징 회귀

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
- **후속 과제**: (a) 실행 fuel이 명령어 수 기반이라 무거운 유한 루프가
  벽시계로 오래 걸릴 수 있다 — load 내 스크립트별 벽시계 예산 검토.
  (b) 근본 해결은 GC(현재 문자열 힙·객체가 문서 수명 내내 단조 증가).

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
      후속: 스크롤바 드래그, 키보드 스크롤, `scrollTop` DOM 프로퍼티.
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
- Python smoke: 373/373 (2026-07-25, Linux + 새로 빌드한 native wheel;
  transition/@keyframes 36종 + iframe 18종 + 접근성 21종 + 폼 컨트롤 20종 + overflow 스크롤 18종 포함. native
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

# GG Browser 개발 체크리스트

기준일: 2026-07-23. 우선순위는 P0(안전·정확성)부터 P3(제품화) 순서다.
각 항목은 구현, 회귀 테스트, 실제 실행 경로 확인까지 끝나야 완료로 본다.

## 현재 진행 요약

- 완료: P0 9/9, P1 8/8, P2 14/17
- 진행할 핵심: transition/@keyframes → iframe 격리 → 접근성 확대
- 제품화: P3 0/6
- 검증 부채: 현재 소스로 네이티브 wheel을 재빌드한 뒤 전체 async smoke 재실행

## 바로 진행할 작업 큐

### 1. Transition과 keyframes — 다음 작업

- [ ] CSS의 `transition-*`/`animation-*` shorthand와 longhand 정규화
- [ ] `@keyframes` 이름, `from`/`to`/백분율 frame 파싱 및 스타일 저장
- [ ] 이전 computed style과 새 style 사이에서 시작값·종료값 캡처
- [ ] color, opacity, 길이, transform의 안전한 보간기 구현
- [ ] duration/delay, iteration-count, direction, fill-mode, play-state 적용
- [ ] 단조 시계 기반 animation sampling과 rAF/timer frame 합류
- [ ] paint-only 속성과 layout 속성의 무효화 경로 분리
- [ ] tkinter와 native shell이 같은 frame scheduler를 사용하도록 연결
- [ ] transition 종료·취소 및 DOM 제거 시 animation 정리
- [ ] 단위·통합·시각 회귀 테스트 추가

완료 조건: transform/opacity transition과 2개 이상의 `@keyframes` 구간이 실제
프레임에서 움직이고, 비활성 페이지에서 timer 폭주가 없으며 두 shell의 결과가
동일해야 한다.

### 2. iframe 문서 격리

- [ ] 자식 문서의 URL/base URL, origin, cookie/storage context 분리
- [ ] 부모-자식 event loop와 load/error lifecycle 연결
- [ ] same-origin DOM 접근 허용 및 cross-origin 접근 차단
- [ ] iframe layout/clip/scroll과 중첩 hit-test 구현
- [ ] navigation·history·CSP/sandbox 최소 정책 및 회귀 테스트

### 3. 접근성 트리 확대

- [ ] 기존 implicit/explicit role snapshot에 accessible-name 계산 확대
- [ ] `aria-label`/`aria-labelledby`/`aria-describedby`와 hidden 상태 반영
- [ ] checked/selected/expanded/disabled/value 상태 노출
- [ ] label-control 관계, heading level, landmark 계층 검증
- [ ] 키보드 focus/activation과 접근성 snapshot 일관성 테스트

### 4. 릴리스 검증 게이트

- [ ] 현재 Rust/Python 소스로 release wheel 재빌드·재설치
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
- [ ] transition/@keyframes와 렌더 프레임 스케줄링
- [ ] iframe과 문서별 origin/event loop 격리
- [ ] 접근성 트리와 ARIA role/name/state 확대

## P3 — 제품화·성능·격리

- [ ] 탭, 다운로드, 북마크, 권한·인증서 오류 UI
- [ ] 브라우징 데이터 삭제와 프로필별 저장소 분리
- [ ] 페이지/네트워크 프로세스 격리와 리소스 제한
- [ ] 크래시 복구 및 세션 복원
- [ ] 프로파일 기반 GC/JIT/레이아웃 Rust 이식 여부 결정
- [ ] Windows 패키징과 CI 회귀 실행, 릴리스 서명

## 장기 호환성 주차장

핵심 P2/P3를 지연시키지 않되, 범위에서 잃어버리지 않을 항목이다.

- [ ] inline-block 정식 formatting context와 baseline 정렬
- [ ] `overflow:auto` 내부 스크롤 영역과 중첩 스크롤 입력
- [ ] `<select>`·체크박스·라디오의 네이티브 수준 렌더링
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] HTTP/2 연결 및 캐시 동작 검증
- [ ] `<video>`/`<audio>`/WebGL
- [ ] HTML5 오류 복구·quirks mode·EUC-KR 등 레거시 인코딩
- [ ] 사이트 바스켓을 쇼핑몰·SPA·커뮤니티 포함 20개 이상으로 확대

## 현재 검증 기준

- Rust: 198 passed, 2 ignored
- Python smoke: 이전 릴리스 기준 249/249; 현재 순수-Python 구간과
  grid/sticky/margin focused 검증 통과
  (이전 native wheel의 async 구간은 제한시간 초과)
- Network gauntlet: 41/41
- CSS gauntlet: 23/23
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

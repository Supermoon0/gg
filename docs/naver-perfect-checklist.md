# 네이버 "완벽 구동" 체크리스트

목표 정의 — 3단계로 나눠서 "완벽"을 측정한다:

| 단계 | 기준 | 필요한 마일스톤 |
|---|---|---|
| **A. 크롬 JS-off 동급** | 로고·둥근 검색창·아이콘이 픽셀 수준으로 같음 | M1 |
| **B. 첫 화면 완성** | 뉴스·쇼핑·피드가 실제로 그려짐 | M2 + M3 + M4 |
| **C. 사용 가능** | 스크롤·호버·검색 타이핑·클릭 이동 | M5 |
| **D. 크롬급 속도** | 재방문 1초 내 첫 화면, 스크롤 60fps | M6 |

## 실측 프로파일 (07-11, 네이버 첫 로드 = 총 1.19s)

| 구간 | 시간 | 진단 |
|---|---|---|
| 네트워크 (HTML+JS+CSS) | **619ms (52%)** | 디스크 캐시 없음 — 매번 2.3MB 재다운로드 |
| JS 실행 (gg-js) | **551ms (46%)** | 사실상 전부 컴파일. 번들이 첫 줄 `self` 미정의로 즉사 → **현재 전액 낭비** |
| HTML 파스+CSS 파스+스타일+레이아웃+페인트 | **20ms (2%)** | 엔진 코어는 이미 빠름 |

→ 성능 작업은 M6에 정리. 엔진 코어(파싱~페인트)는 병목이 아니다.

**07-11 저녁 갱신**: 즉효 3종 + M1 일부 적용 후 **재방문 로드 0.18s**
(디스크 캐시 + CSS 프리페치 오버랩). 네이버 껍데기가 **둥근 초록 알약
검색창 + 회색 placeholder**로 렌더 — 크롬 JS-off 외형에 근접.

**07-12 갱신 — 번들 스코어보드** (에러 로그 관문을 순서대로 15개 격파):

| 번들 | 크기 | 상태 |
|---|---|---|
| search.js | 283KB | ✅ 컴파일+실행 완주 |
| main.js | 763KB | ✅ 컴파일+실행 완주 |
| preload.js | 192KB | ✅ 컴파일+실행 완주 |
| polyfill.js | 254KB | ⏳ 파스 통과, `expression too deep` 1건 잔존 |
| 광고 SDK 3종 | — | 스킵 범주 (ndpsdk/NBP_CORP/apply — 외부 전역 의존) |

격파 순서: self → doc/window 리스너 → delete → ++/--멤버 → 비트연산 →
레지스터 스필 2단 → 옵셔널 호출 → Function 스텁 → **프로토타입 체인** →
메서드 추출 → Error 계층 → 숫자 메서드 → callable Object/Array →
arguments → 라벨 블록/elision/no-in 경계.

근거: 2026-07-11 실측 — 네이버 홈 HTML 215KB, main.css 754KB,
JS 번들 4개(polyfill 254KB + preload 192KB + search 283KB + main 763KB)를
전수 스캔한 기능 사용 횟수. `(×n)`이 그 횟수다.
**희소식**: 번들이 ES5로 트랜스파일되어 있어(구조 분해 0회, await 0회,
class 2회) 문법 갭이 좁고, `display:grid`·`position:sticky`도 **0회**라
아예 구현하지 않아도 된다.

---

## M0 — 완료 ✅ (2026-07-11까지)

- [x] HTTP/1.1 + TLS, keep-alive 풀, 메모리 캐시, chunked, gzip
- [x] HTML 파서 (215KB/136노드 정확 — 문서의 85%가 EAGER-DATA JSON인 것 확인)
- [x] CSS 캐스케이드 + **커스텀 프로퍼티 var()** (체인·fallback·순환가드)
- [x] **`:root` / `:where(...)`** 셀렉터 (변수 2,506개 정의 블록 파싱)
- [x] gg-js: let/const·TDZ, try/catch, `new`, **정규식 실행**, **in/instanceof**,
      removeAttribute (검색창 리빌 스크립트 완주 → 초록 테두리)
- [x] SVG path 래스터라이즈 (돋보기 아이콘, 전체 커맨드 + 타원 호, 4×4 SSAA)
- [x] 가로 스크롤 (1190px 고정폭 디자인 대응)
- [x] 비동기 이벤트 루프 (가상 시계 타이머 + fetch 서비스, settle)

현재 화면: 흰 배경 + 초록 테두리 검색창 = **크롬 JS-off와 구조 동일, 픽셀 미달**.

---

## M1 — 껍데기 픽셀 완성 (일 단위, 즉시 효과)

### 페인트
- [x] **`background-image: url()`** 로드·표시 + `background-size/-position/-repeat`
      (×488) — 07-11: 첫 레이어, url 로드 + position(px/%/키워드) +
      size(cover/contain/px/%) + repeat/타일 + 스프라이트 음수 오프셋 크롭.
      다중 레이어·gradient 레이어는 미지원
- [x] **`border-radius`** (×271) — 07-11: 균일 반경 + `50%`, 라운드 배경/테두리
      링, 코너 AA. 모서리별 개별 반경·이미지 클리핑은 미지원
- [ ] **`::before`/`::after` + `content`** (×884 — 아이콘 다수가 가상 요소로 그려짐.
      **M1 잔여 중 최대**)
- [x] `opacity` (×410) — 07-11: ≈0이면 서브트리 페인트 스킵 (부분 투명 합성은 미지원)
- [ ] `box-shadow` (×186)
- [ ] `overflow: hidden` 클리핑 (×136 — 스크린리더 텍스트 숨김의 정석 경로)
- [ ] `z-index` 쌓임 순서 (×91 — 지금은 문서 순서 페인트)
- [ ] `linear-gradient` (×36)
- [ ] `@font-face` 웹폰트 로드 (×4)
- [x] input `placeholder` 표시 — 07-11 ("검색어를 입력해 주세요." 회색 렌더,
      `input[type=hidden]` UA 룰 포함)

### 셀렉터·스타일
- [x] **속성 셀렉터 `[attr=...]`** (×636) — 07-11: 존재/`=`/`~=`/`^=`/`$=`/`*=`/`|=`
      + 따옴표/케이스 플래그, Python·Rust 미러
- [ ] `:not()` (×38), `:nth-child` (×19)
- [ ] `@media (width…)` 평가 (×5 — 지금은 블록째 스킵)

### 레이아웃
- [ ] `line-height` (×380)
- [ ] `white-space: nowrap` (×83) + `text-overflow: ellipsis` (×84)
- [ ] flex 심화: `flex-shrink/basis`, `align-items`, `justify-content` (×104)
- [ ] CSS `width/height`가 대체 요소(img·svg·input)에 적용 (지금은 HTML 속성만)
- [ ] `float` + `clear` (×25)
- [ ] inline-block 정식 배치 (지금은 근사)
- [ ] margin collapsing
- [x] ~~grid~~ (×0), ~~sticky~~ (×0) — 네이버 홈엔 없음, 스킵

---

## M2 — gg-js 언어 완주: 번들 4개(1.3MB) 컴파일+실행 통과 (주 단위)

### 첫 관문 (실측: 지금 번들이 죽는 지점)
- [x] **`self`/`globalThis` 전역 별칭** — 07-11 완료. 번들이 다음 관문으로 전진:
      현재 죽는 지점 = `ndpsdk`/`NBP_CORP` 미정의(외부 SDK 전역),
      `document.addEventListener`, too many locals, 콤마/콜론 파스, `delete`

### 파서/컴파일러
- [x] `delete` 연산자 (×85) — 07-11: Instr::Delete, fresh-shape 재구성으로
      정식 삭제(keys/in/for-in 일관, IC 자연 무효화)
- [x] `++`/`--` 멤버 타깃 — 07-11 (o.x++/o[k]--, postfix 이전 값 반환)
- [x] document/window `addEventListener` — 07-11 (document는 실등록,
      window는 Noop 수용 — M3 항목이지만 번들 관문이라 선행)
- [x] **비트 연산 7종** — 07-11 (Shl/Shr/UShr/BitAnd/BitOr/BitXor/BitNot,
      ToInt32/ToUint32 모듈로-2³² 래핑, 복합 대입 `&=` 포함)
- [x] **레지스터 스필 2단계** — 07-11/12 (Place::SpillCell/SpillUp —
      오버플로 var를 %spillN 힙 객체로, 셀 캡처로 클로저도 접근.
      임계 120: 레지스터 250은 로컬+temp 공용 예산이라 temp 여유 확보)
- [x] 콤마/콜론 파스 에러 규명·격파 — 07-12: 정체는 **라벨 블록/스위치**
      (`a:{...break a}`), **배열 elision** (`[,x]`), **no-in 경계 버그**
      (for-헤드 no_in이 함수/괄호 경계에서 리셋 안 됨 — core-js RegExp
      폴리필의 `for(var B=function(){..."dotAll"in D...};;)`)
- [x] `?.` 옵셔널 호출 — 07-11 (`a.b?.()`/`a['b']?.()` — 널가드 후
      CallThis, computed 키는 rf 전에 평가해 인자 연속성 유지)
- [x] 이항 연산 temp 재사용 — 07-12 (dst=좌측 피연산자 재사용 + tmp_top
      반납 — 체인 O(1) temp)
- [ ] **polyfill 마지막 관문: `expression too deep`** — 모듈 703개 개별 ✓,
      전부 합침 ✓, 로더 문장 개별 ✓, 합성 재현 ✓ — **전문 결합 시에만**
      재현되는 누적 케이스. 다음: alloc() 실패 지점에 소스 오프셋 로깅
- [ ] class 선언 (×2), spread 나머지 (×2)

### 객체 모델 — **M2의 최대 공사**
- [x] **프로토타입 체인 실물화** — 07-12 (보스 1페이즈): Obj에 [[Prototype]]
      슬롯, own 미스 시 체인 워크(메서드 호출 포함), 함수 `.prototype`
      lazy 생성(+constructor 역참조)·교체 가능, **`new` 실코드젠**
      (NewInstance+CallThis+SelectObj — IIFE 디슈가 폐기, 생성자의 객체
      반환 우선), instanceof 체인 탐색
- [x] **메서드 추출** — 07-12 (보스 2페이즈, core-js uncurryThis 패턴):
      `''.slice`·`(1).toString`·`[].slice`가 MethodRef 네이티브 반환 →
      call/apply 시 receiver로 디스패치 (문자열 slice/substring/charAt/
      charCodeAt/indexOf/toString(radix)/valueOf, 배열 slice/indexOf)
- [x] 에러 계층 — 07-12: Error/TypeError/RangeError/SyntaxError/
      ReferenceError를 **JS 프렐류드로** 정의 (프로토타입 체인 위에서
      instanceof 자연 동작)
- [x] **호출 가능 Object/Array** — 07-12: fn_props 정적 프로퍼티 테이블
      (Object.keys 등 + 사용자 `F.staticX`), ObjectCtor/ArrayCtor 네이티브
- [x] **arguments 객체** — 07-12: 프레임 argc 스레딩(Frame/Handler/exec),
      uses_arguments 함수는 초과 인자 보존, `[].slice.call(arguments)` 동작
- [x] Function 생성자 스텁 — 07-11 (`Function("return this")()` → window)
- [ ] getter/setter (`Object.defineProperty` — polyfill의 기본 도구.
      **polyfill 실행 단계의 다음 관문 유력**)
- [ ] `Object.getOwnPropertyDescriptor/defineProperties/create/setPrototypeOf`
- [ ] `Symbol` + iterator 프로토콜 (×1이지만 polyfill 내부에서 다수)
- [ ] `Map`/`Set`(×9)/`WeakMap`(×2), `Proxy`(×1), `Reflect.*`(×10)
- [ ] 실행 성능: 1.3MB를 JS 예산 내 실행 (IC 확대; baseline JIT은 후순위)

---

## M3 — 웹 플랫폼 API 표면 (앱 부팅, 주~월 단위)

### DOM/이벤트
- [x] `window`/`document` 레벨 addEventListener — 07-11 (document 실등록,
      window는 수용 후 무시; console error/warn/info/debug도 추가)
- [ ] 이벤트 캡처/버블 완전판, `removeEventListener`(×22),
      `dispatchEvent`(×4)/`CustomEvent`(×3)
- [ ] `classList`(×13), `dataset`, `el.style.prop =` 개별 세터
- [ ] `innerHTML` 완전판 (×14), `insertBefore/replaceChild/cloneNode` 등 트리 API
- [ ] **`getBoundingClientRect`** (×4) — JS가 레이아웃 결과를 읽는 다리 (설계 필요)
- [ ] `getComputedStyle`

### 브라우저 객체
- [ ] `location.*` (×19), `history.*` (×2), `navigator.*` (×2), `performance.*` (×3)
- [ ] **`localStorage`(×9)/`sessionStorage`(×1)** + **`document.cookie`(×4)**
      ← 네트워크 계층 쿠키 저장소와 연동 (지금 쿠키 미지원)
- [ ] `requestAnimationFrame` (×13) — 가상 시계와 프레임 루프 통합
- [ ] `IntersectionObserver`(×1)/`ResizeObserver`(×2) — 최소 스텁 + 발화
- [ ] `matchMedia` (×2), `postMessage` (×8), `scrollTo` (×11)
- [ ] canvas 2D (×9 — 크래시 방지 스텁 먼저, 실렌더 후순위)
- [ ] `fetch`/XHR 마무리 (main 번들엔 0회 — 데이터는 EAGER-DATA 인라인이라 후순위)

---

## M4 — 동적 렌더 루프 (설계 공사)

- [ ] JS 변이 → 더티 마킹 → **부분** 재스타일/재레이아웃/재페인트
      (지금은 전체 재계산 — 피드 규모에선 필수)
- [ ] rAF·타이머와 프레임 스케줄러 통합 (지금 settle은 로드 시 1회성)
- [ ] `transform` (×646 — 스프라이트 배치·이동에 광범위)
- [ ] `transition`(×98) / `@keyframes` 애니메이션(×163) — 시각 완성도
- [ ] 스크롤 리페인트 성능 (디스플레이 리스트 캐시/타일)

---

## M5 — 상호작용 완성

- [ ] 텍스트 입력 포커스/캐럿 + **IME 한글 조합** → 검색창 타이핑
- [ ] `:hover`/`:focus` 동적 재스타일 (×231/×28)
- [ ] 폼 제출 → search.naver.com 이동 (GET 쿼리 조립)
- [ ] 쿠키 세션 유지 (로그인은 범위 밖 — 별도 대공사)
- [ ] iframe (홈 셸엔 0개; 광고·로그인에서 등장 — 후순위)

---

## M6 — 성능 (실측 1.19s 기반, "크롬급 체감"까지)

체감 순 정렬. 상단 두 개가 현재 시간의 98%를 지운다.

- [x] **디스크 캐시** — 07-11: `%LOCALAPPDATA%/gg-browser/cache`, 명시적
      max-age 응답만(HTML은 신선 유지). 실측: CSS 195→13ms, JS 7종 545→52ms,
      **전체 재방문 로드 1.19s → 0.18s**
- [ ] **JS 게으른 컴파일(lazy parse)** — 함수 본문을 첫 호출 때 컴파일.
      번들의 대부분 함수는 호출되지 않으므로 551ms → 수십 ms 예상.
      gg-js 구조 공사지만 V8도 쓰는 정공법.
- [x] 파이프라인 오버랩 — 07-11: load_document가 JS 페치·실행과 병렬로
      스타일시트를 프리페치(스레드), JS가 주입한 링크만 후속 페치
- [ ] 시작 시간 — 파이썬 기동+창 생성+폰트 로드 수백 ms (프로파일 후 캐시)
- [ ] **baseline JIT** — 번들이 실제로 "돌기" 시작하면(M2 이후) 실행 시간이
      새 병목. 인터프리터 대비 3~10배 목표. (jsvm 로드맵 기존 항목)
- [ ] **레이아웃 러스트 이식** — 지금은 1ms(껍데기 136노드)라 문제없지만
      피드 수천 노드 + 호버 재레이아웃이 오면 파이썬이 병목이 됨.
      파싱/스타일 이식(242→11ms) 같은 승리가 한 번 더 남아 있는 곳.
- [ ] **GC** — 성능이라기보다 지속성: 무한 스크롤 워크로드에서 메모리 증가.
      (jsvm 로드맵 기존 항목)
- [ ] M4 부분 무효화와 합류 — 인터랙션 60fps의 전제

---

## 부록 — 이 체크리스트 밖: "모든 웹사이트"까지

이 문서는 네이버 홈 기준이다. 완주해도 웹 전체엔 다음이 남는다
(네이버 홈 사용량 0이라 뺐지만 다른 곳에선 흔한 것들):

- [ ] `display: grid` / `position: sticky` (GitHub·뉴스 사이트 도배 수준)
- [ ] **테이블 레이아웃** (옛 사이트·정부 사이트 뼈대)
- [ ] **`overflow: auto` 내부 스크롤 영역** (채팅창·사이드바)
- [ ] 폼 컨트롤 렌더링 (`<select>`·체크박스·라디오)
- [ ] 트랜스파일 안 된 모던 JS (구조 분해·async/await·제너레이터·**ES 모듈**)
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>`/`<audio>`/WebGL (유튜브·지도류 — 사실상 별개 프로젝트)
- [ ] iframe 문서 격리, CORS, 쿠키 전체 속성 (로그인·광고·임베드)
- [ ] HTML5 오류 복구 알고리즘 완전판, quirks 모드, **EUC-KR 등 레거시 인코딩**,
      RTL/양방향 텍스트
- [ ] 진행 지표: **사이트 바스켓** — 관심 사이트 10~20개를 driver로 자동 렌더해
      "읽을 만한가"를 마일스톤마다 측정 (위키백과는 이미 통과)

---

## 권장 공략 순서 (07-12 갱신)

0. ~~즉효 3종~~ ✅ / ~~M2 소품·파스 에러~~ ✅ / ~~프로토타입 체인~~ ✅
   — 번들 4개 중 3개 완주, 에러 로그 관문 15개 격파.
1. **polyfill 마지막 관문** (`expression too deep` — 전문 결합 시에만 재현):
   compiler alloc() 실패 지점에 소스 오프셋 로깅 추가가 최단 경로.
   이거 하나면 **본문 번들 전체 컴파일 완주(M2 중간 이정표)**.
2. **polyfill 실행 단계** — getter/setter(`Object.defineProperty`)가 유력한
   첫 관문, 이어서 descriptor류·Symbol·Map/Set (에러 로그가 우선순위표).
3. **M1 잔여 최대: `::before`/`::after` + content** (×884 — 화면 임팩트 최대,
   양 스타일 엔진에 가상 자식 합성 설계 필요) + box-shadow·overflow clip·z-index.
4. M3 API 표면 (getBoundingClientRect·rAF·localStorage/쿠키·observers).
5. M4 부분 무효화 + M6 lazy 컴파일 — 피드가 그려지기 시작하면 성능이 병목.
6. M5 입력/호버 — "쓸 수 있는 브라우저". M6 잔여(JIT·레이아웃 이식·GC)는
   B단계 도달 후 측정해서 순서 결정.

*갱신: 2026-07-12 (번들 스코어보드·M2 대량 체크·보스 1/2페이즈 완료 반영).
항목을 완료하면 [x]로 바꾸고 날짜를 적을 것. cargo 118/118, smoke 86종 기준.*

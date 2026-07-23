# 네이버 "완벽 구동" 체크리스트

목표 정의 — 3단계로 나눠서 "완벽"을 측정한다:

| 단계 | 기준 | 필요한 마일스톤 | 상태 (07-16) |
|---|---|---|---|
| **A. 크롬 JS-off 동급** | 로고·둥근 검색창·아이콘이 픽셀 수준으로 같음 | M1 | **사실상 달성** (M1 주요 항목 완료, 웹폰트·flex 심화 등 소품 잔여) |
| **B. 첫 화면 완성** | 뉴스·쇼핑·피드가 실제로 그려짐 | M2 + M3 + M4 | **07-23: 사실상 달성** — react가 **완전 커밋**, #root에 674 엘리먼트(뉴스스탠드·언론사 탭·쇼핑 캐러셀·이미지 44) 렌더, **JS 오류 0**. (07-20의 "커밋 못 함/88s" 프런티어 해소) |
| **C. 사용 가능** | 스크롤·호버·검색 타이핑·클릭 이동 | M5 | 스크롤·타이핑·GET 제출·**hover/focus 재스타일**(07-18) · 잔여: IME |
| **D. 크롬급 속도** | 재방문 1초 내 첫 화면, 스크롤 60fps | M6 | 재방문 0.18s ✅ · **07-23: settle 57s→~3s**(O(n²) 4곳 제거) · 60fps 미측정 |

**진도 측정 도구**: `python basket_test.py` — 대표 사이트 8개 헤드리스 렌더
스코어보드. 07-16 실측: **7/8 "읽을만함", 크래시 0** (위키백과 텍스트
3802개·HN·MDN·연합뉴스·티스토리·나무위키·정부24). JS오류: 티스토리
21→7, 연합 20→19 (07-16 모던 JS 라운드 후).

## 실측 프로파일 (07-16 갱신, 네이버 웜 로드 = 총 1.0s)

| 구간 | 시간 | 진단 |
|---|---|---|
| HTML 네트워크(캐시) | 70ms | 디스크 캐시 동작 중 |
| **JS 실행 (gg-js)** | **1,003ms (93%)** | 07-11의 551ms보다 **늘었음 — 진보의 비용** (그땐 첫 줄 즉사, 지금은 번들 4개가 실제로 깊이 실행됨). lazy 컴파일이 최대 지렛대 |
| 파스+CSS+스타일 | 77ms | 준수 |
| 레이아웃+페인트 | 4ms | 사실상 무료 |

무거운 페이지(위키백과 3,904노드): 콜드 2.0s / 웜 0.75s
(레이아웃 115ms + 페인트목록 123ms — Python치고 양호, M6 러스트 이식 후보).
**프레임레이트**: 래스터 26ms/frame(38fps 상당), 스크롤 재직렬화 포함
39ms(25fps) — 60fps 미달, M6 참조.

→ 성능 작업은 M6에 정리. 엔진 코어(파싱~페인트)는 병목이 아니다.
병목은 JS 실행 단 하나.

**07-11 저녁 갱신**: 즉효 3종 + M1 일부 적용 후 **재방문 로드 0.18s**
(디스크 캐시 + CSS 프리페치 오버랩). 네이버 껍데기가 **둥근 초록 알약
검색창 + 회색 placeholder**로 렌더 — 크롬 JS-off 외형에 근접.

**07-12 갱신 — 번들 스코어보드** (에러 로그 관문을 순서대로 15개 격파):

| 번들 | 크기 | 상태 |
|---|---|---|
| search.js | 283KB | ✅ 컴파일+실행 완주 |
| main.js | 763KB | ✅ 컴파일+실행 완주 |
| preload.js | 192KB | ✅ 컴파일+실행 완주 |
| polyfill.js | 254KB | ⏳ 실행 대공세 중(07-17): 모듈 트레일 222→457+. **웹팩 런타임이 이 번들 꼬리에 있어 polyfill 완주 = 앱 부팅 전체의 게이트**(판명). 현재 관문 .delete() |
| 광고 SDK 3종 | — | 스킵 범주 (ndpsdk/NBP_CORP/apply — 외부 전역 의존) |

격파 순서: self → doc/window 리스너 → delete → ++/--멤버 → 비트연산 →
레지스터 스필 2단 → 옵셔널 호출 → Function 스텁 → **프로토타입 체인** →
메서드 추출 → Error 계층 → 숫자 메서드 → callable Object/Array →
arguments → 라벨 블록/elision/no-in 경계.

**07-19 부팅 관문 정밀 진단** (실네트워크, 웹팩 모듈 트레일 계측 —
polyfill·preload 완주(705모듈), search는 regenerator 팩토리에서,
main(react-dom 18.3.1)은 8번째 모듈 초기화에서 사망. 5개 관문 특정
→ **같은 날 저녁 5개 전부 격파** — cargo 172/172(관문 회귀 테스트
naver_boot_gates 추가), smoke 148, 건틀릿 무회귀(jsvm은 21→22로 개선:
`[][Symbol.iterator]` 추출이 스펙 정합), basket 7/9 유지·네이버 페인트
567→641. 이전 5개 오류 전부 소멸, 새 프런티어는 아래):

1. - [x] **[S] `Object.getPrototypeOf(함수)`가 undefined** → 07-19:
   KnownCtors에 function 추가, O_GET_PROTO 함수 분기가 Function.prototype
   실물 반환(→Object.prototype→null 체인 종결). fn instanceof
   Object/Function도 참. **search 번들 regenerator 관문 소멸**
2. - [x] **[M] `new Set(배열)` 전역 파손 → react-dom 사망** → 07-19
   이음새 3개 모두: (a) 배열 인스턴스가 JS-가시 Array.prototype expando를
   봄(GetProp/GetIndex/odd-key 3경로 + 접근자 인식), (b) defineProperty
   키 ToPropertyKey(가짜 심볼 태그 보존 — 읽기 경로와 일치), (c)
   진짜 Object.prototype.toString은 Native::BrandToString — 수신자
   브랜드('[object Array]' 등) 반환, 배열 자신의 toString은 join 유지.
   **"not iterable [P-iterate]" 관문 소멸**
3. - [x] **[M] 원시값 프로퍼티 읽기가 아무 키에나 truthy 스텁** → 07-19:
   primitive_prop_read — 지원 빌트인 이름·constructor(타입 생성자 반환)·
   String/Number/Boolean.prototype expando(폴리필!)만 답하고 나머지
   undefined. **jQuery ready 관문 소멸** ($(document).ready 실행됨)
4. - [x] **[S] 최상위 `var X = X || {}` 호이스팅** → 07-19: DeclGlobal
   명령 신설 — 최상위 var 이름을 본문 실행 전 defined-undefined로
   (기존 값은 보존 — 번들 간 재선언 안전). **NBP_CORP 관문 소멸**
5. - [x] **[S] NFE 자기 이름 바인딩 + instanceof 비호출가능 RHS** →
   07-19: LoadSelf 명령(현재 클로저) — 파라미터/var 섀도잉 없을 때
   함수 스코프에 자기 이름 바인딩, 캡처 시 셀 승격 경로 통과.
   instanceof 비호출가능 RHS는 스펙대로 TypeError. **_classCallCheck
   관문 소멸**

**07-19 저녁 새 프런티어** (관문 5 격파 후 첫 재측정 — 콘솔 52→15줄,
오류 6): `.keys() is not a function` ×3, `cannot read .IS_OP of
undefined`, `Obj(#…) is not a function` ×2. react 마커 0·#container 4
유지 — 다음 라운드는 이 3종 규명부터.

**07-20 라운드 2·3 — 4개 번들 전부 완주, react-dom 엔트리 실행**:
- [x] **top-level `this` = window** (page.rs run_source) — 웹팩 UMD
  래퍼가 전역으로 넘기는 this가 undefined라 AgentDetect류 최상위
  IIFE가 `.IS_OP of undefined`로 사망하던 것 해소
- [x] **`[].keys()/values()/entries()/@@iterator` 메서드 호출**
  (CallMethod 배열 분기 + method_ref_dispatch @@iterator arm) —
  core-js es.array.iterator가 이걸로 부팅. defineProperty로 심은
  Array.prototype 메서드도 인스턴스에서 호출됨. `.keys() is not a
  function` ×3 소멸
- [x] **`arguments.callee`** (sloppy) — search 번들 jindo
  Component.extend가 재호출용으로 저장. inner 모듈 1379 사망 해소
- [x] **defer 스크립트 실행 순서** (lib.rs script_entries) — 앱 번들
  전부 `defer`인데 뒤쪽 인라인 스크립트가 정의하는 전역
  (`EAGER-DATA.GV`)을 읽음. 파서 순서 먼저 → defer 순서로 정렬.
  `.login of undefined` 소멸
- [x] **DEFAULT_FUEL 80M→400M** — 80M이 앱을 부팅 도중 끊었음
- [x] **MessageChannel 실동작** (page.rs 프렐류드) — react-dom 18
  스케줄러가 렌더 워크 루프 전체를 MessageChannel로 구동. no-op
  스텁이라 react가 커밋을 못 하던 것 → 이제 스케줄러가 settle 중 실행
- [x] **라이프사이클 이벤트 실물화** — target/currentTarget/bubbles/
  preventDefault 등 표준 프로퍼티 부여

**07-20 라운드 3b·3c — react 무한루프 격파, 리컨사일러 진입**:
- [x] **Math.clz32 신설** — react lane 순회 `while(lanes){i=31-
  clz32(lanes); lanes&=~(1<<i)}`가 clz32 부재로 최상위 비트를 못 찾아
  엉뚱한 비트를 지워 **영원히 안 끝나던 무한루프**. settle 22s→1.4s.
  GG_JS_TRACE로 workLoopSync→performUnitOfWork→beginWork 체인 국소화 후
  루프 프로토 디스어셈블로 규명 — 이번 세션 최대 단일 성과
- [x] **Object.is** — SameValue(=== + NaN==NaN + -0≠+0). react
  bailout/shallowEqual이 직접 사용. `.is()` 미구현이라 throw하던 것
- [x] **instanceof 비호출가능 RHS → false 복원** — 라운드1의 TypeError
  던지기는 _classCallCheck용이었으나 NFE self-binding(LoadSelf)이 이미
  해결. 실번들의 `x instanceof 없는생성자`(피처 디텍션)가 react 마운트
  경로에서 uncaught 유발 → 관용 false로 되돌림

**07-20 라운드 4 — react 렌더+커밋 페이즈 진입 (양파 까기 연속)**:
이름 해석 디스어셈블러(GG_JS_DUMP가 atom→프로퍼티명·const→문자열)로
각 null/undefined를 앱 소스 수준까지 역추적하며 연쇄 격파:
- [x] **fancy-regex 2차 엔진** — `regex` 크레이트가 거부하는 백레퍼런스·
  lookaround를 fancy-regex로. **date-fns 토크나이저 `/(\w)\1*|./g`가
  never-matching으로 강등돼 `.match`가 null → 앱의 `for...of null`이
  throw → react 렌더 중단**이었음. RegexRec.re를 CompiledRe{Std/Fancy/
  Never} enum으로, 7개 호출부 통일. **이번 라운드 핵심 근본원인**
- [x] **Date 세터** setUTCFullYear/Month/Date/Hours + 로컬 별칭 — date-fns
  날짜 조립. 게터·setTime만 있고 컴포넌트 세터 전무였음
- [x] **Function.prototype.toString/valueOf/hasOwnProperty** (CallMethod
  경로) — 번들이 함수 해싱/피처 디텍션으로 `fn.toString()` 호출
- [x] **ErrorEvent/PromiseRejectionEvent/MessageEvent** 전역 생성자 —
  에러 리포팅 경로가 참조
- [x] **window.dispatchEvent (WinDispatch) + EventTarget.prototype**
  실동작 — react가 `window.dispatchEvent(errorEvent)`로 에러 보고
- [x] **document.createElementNS** — react가 모든 SVG 노드를 이걸로 생성
  (네이버 홈은 SVG 아이콘 다수). 미지원이라 SVG stateNode가 undefined →
  **커밋 페이즈에서 `.classList of undefined`**. 네임스페이스 무시,
  로컬명(arg1)으로 생성

**07-20 라운드 4 현 프런티어**: react가 **렌더 완주 + 커밋 페이즈 진입**
(settle 워크 1.4s→88s = react가 실제 파이버 트리를 대량 처리 중).
현재 사망점: 커밋 경로의 클래스 토글 헬퍼(mi=3310)가 **undefined DOM
노드**를 받아 `node.classList.add/remove` throw — react/앱이 실브라우저엔
있는 노드를 참조하는데 우리 DOM엔 없어 undefined(id/ref 불일치). 별개로
잔존: 앱 헬퍼 `.length of undefined`(jQuery.each류, mi≈2758). react
미커밋(마커 0·#root 2·#container 4)이나 페이지 그레이스풀 디그러데이션
유지(검색창+리더 헤드라인, 무크래시). **판단: react 완전 커밋은
"이 노드가 왜 undefined인가"의 긴 꼬리(노드별 whack-a-mole, 빌드당
~4분) + 렌더 88s의 실용성 문제로 다세션 리서치 규모. 무한루프·정규식
같은 근본원인은 이번에 다 잡음.** cargo 174/174, smoke 148, basket 7/9,
건틀릿 무회귀.
진단 도구: `GG_JS_TRACE=1`(nullish/budget 오류에 프레임 체인 mi:pi+
mi/pi/ip, 사망 리스너), `GG_JS_DUMP=<dir>`(사망 프로토+상위 콜러 4단
**이름 해석** 바이트코드 덤프) — env 미설정 시 0비용.

후속: 동적 주입 `<script src>` 미실행(ndp-core/ntm — pump 프로토콜에
script 종 추가 [M]), 정규식 lookaround/backref 31건 never-matching 강등
(fancy-regex 2차 엔진 [M], 서로게이트 클래스 강등은 무해 판명).

**★ 07-23 프런티어 해소 — react 완전 커밋 + 렌더 (07-20의 벽 돌파)**:
07-20에 "다세션 리서치 규모"로 판정했던 두 objection이 모두 사라짐 —
(1) 렌더 88s → **~3s**, (2) 커밋 경로 undefined-노드 크래시 → **JS 오류 0**.
실측(실네트워크, 4.4→3.2s): react가 **#root에 674 엘리먼트 완전 커밋**
— 뉴스스탠드(언론사편집·엔터·스포츠·게임·경제 탭)·쇼핑 캐러셀(도서
저자/출판사)·이미지 44개·GNB. 크래시·언캐치 0. **B단계(첫 화면 완성)
사실상 달성.** 이 세션에서 격파한 근본원인:
- [x] **인터프리터 O(n²) 4곳 제거 (settle 57s→~3s)** — settle 전체가 한
  pump 라운드에 몰리고 힙이 커질수록 명령/초 20배 붕괴. `intern()`
  선형스캔→해시맵, charAt/charCodeAt 매호출 UTF-16 재구성→문자열당
  유닛 메모, 문자열 `.length` 매읽기 재카운트→길이 메모,
  method_ref_dispatch 배열 수신자 선(先)클론→무클론 패스. (상세: M6)
  **88s→~3s가 "커밋은 하는데 느려서 비실용" objection을 제거한 결정타.**
- [x] **파서 갭 (스크립트 통째 사망 유발)** — class 필드명 get/set/static
  오인, async 제너레이터 `async function*`/객체 async 메서드, escape/
  unescape 전역. yna·모던 번들 일반.
- [x] **문서 박스 = 뷰포트** — DocumentLayout이 UA `body{margin:8px}`
  위에 (13,18) 인셋을 중복 적용해 전 페이지가 크롬 대비 (13,18) 밀리고
  26px 좁았음. 원점 (0,0)·풀폭으로 → `top:-30px` sr-only 스킵링크가
  화면 밖 클립(크롬 정합). smoke 143(인셋 하드코딩 4개 정정).
현 잔여(롱테일, 저순위): 콘텐츠 영역 산발 오버랩 소수(캐러셀 엣지케이스,
서빙 콘텐츠 따라 4~36), 동적 `<script src>` 미실행, 정규식 강등.
**판단: A·B·D 사실상 달성. "완벽"의 남은 격차는 근본원인이 아니라
시각 디테일·롱테일.**

**★ 07-23 뷰포트-lazy 콘텐츠 로드 (step 프리미티브 + settle_lazy)**:
헤드리스 렌더가 앱 셸+EAGER-DATA 섹션(뉴스스탠드·피드)만 그리고 데이터
기반 섹션(쇼핑 피드·푸터·위젯)은 비어 있던 문제를 규명·해소:
- **근본원인**: 섹션 로드 이펙트가 settle 중 실행되는데 그 시점엔 레이아웃이
  없어 `getBoundingClientRect`가 0 → "화면 밖" 판정 → 로드 포기. React
  이펙트는 재실행 안 되므로 사후 rect 푸시로 복구 불가. 그리고 `pump()`은
  한 번에 모든 타이머를 발화해 commit↔effect 사이에 레이아웃을 못 끼움.
  (MessageChannel 스케줄러가 `setTimeout(0)` 매크로태스크로 도는 것 확인 —
  타이머 하나씩 발화하면 끼울 수 있음.)
- **step 프리미티브** (vm.rs `pump_step` + Doc.step/now_ms): 스케줄러 슬라이스
  **한 개**(타이머 1개+마이크로태스크)만 발화하고 반환. 호스트가 슬라이스
  사이에 실제 rect를 push → 다음 슬라이스의 이펙트가 실 geometry를 봄.
- **settle_lazy** (native.py): step 드라이브 + 레이아웃 인터리브. Naver의
  lazy 섹션은 단일 `/nvhaproxy/v2/pc/lazy`(142KB) 뒤에 배치돼 있고, 이걸
  발화·해소하니 **리치 피드(패션/쇼핑 썸네일 다수)+전체 푸터** 렌더. 레이아웃
  코얼레싱(프레임 단위 스로틀)으로 **27s→~7s**. cargo 189/smoke 143 무회귀
  (settle_lazy 옵트인).
- **잔여(진짜 롱테일)**: 우측 사이드바 위젯(날씨/증시/캘린더/VIBE) — 데이터는
  `/lazy`에 있고(PC-WEATHER/PC-STOCK/PC-CALENDAR) 컬럼(w420)도 있으나 위젯
  컴포넌트가 **데이터를 받고도 빈 채로 렌더**(캔버스 차트 스텁 + 컴포넌트별
  이슈). 위젯별 whack-a-mole + 네이버 서빙 콘텐츠 편차 큼. 언론사 풀그리드
  (6/24)·상단 쇼핑 캐러셀도 유사 잔여.

**시각 격차의 정체(07-19)**: 네이버 서빙 HTML은 `<img>` 0개의 앱 셸 —
로고·아이콘·썸네일은 전부 JS 부팅 후 생성. @font-face 4종 모두 ttf
폴백 보유(woff2 갭 안 물림)·사용 요소 0. 그라디언트/그림자 근사도 서빙
셸에선 매칭 0. **즉 "완벽 렌더" ≈ 위 관문 격파가 거의 전부**. 부팅 후
과제: 모서리별 radius(196 box-shadow 중 blur 156)·실그라디언트·부분
opacity 합성. 별건 발견: line-height가 크롬보다 ×1.164 헐거움(factor를
font_px 기준으로 만들고 ascent+descent에 곱함 — layout.py [S]),
gradient+url 다중 레이어에서 url 레이어 소실(draw.py parse_background
[S/M]), 오프셋 없는 absolute 박스 레이아웃 탈락(layout.py [S/M]),
basket 이미지 열은 하니스가 이미지를 아예 안 싣는 아티팩트([S]).

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

## M1 — 껍데기 픽셀 완성 ✅ 사실상 완료 (07-16)

주요 항목 전부 완료 — 잔여는 소품(웹폰트·flex 심화·float 등)뿐.
바스켓 검증으로 네이버 밖 일반화 확인됨.

### 페인트
- [x] **`background-image: url()`** 로드·표시 + `background-size/-position/-repeat`
      (×488) — 07-11: 첫 레이어, url 로드 + position(px/%/키워드) +
      size(cover/contain/px/%) + repeat/타일 + 스프라이트 음수 오프셋 크롭.
      다중 레이어·gradient 레이어는 미지원
- [x] **`border-radius`** (×271) — 07-11: 균일 반경 + `50%`, 라운드 배경/테두리
      링, 코너 AA. 모서리별 개별 반경·이미지 클리핑은 미지원
- [x] **`::before`/`::after` + `content`** (×884) — 07-16: 셀렉터
      pseudo 마크(::/: 레거시 포함) → Rust 스타일 엔진이 가상 자식 합성
      (content 문자열/none·normal, 재스타일 중복 가드, var() 적용) →
      레이아웃이 크기+배경 가상 아이콘을 인라인 이미지 박스로(스프라이트
      크롭 페인트), 텍스트 content는 일반 흐름. 네이버에서 합성·스프라이트
      로드 확인. Python 폴백 엔진은 미러 안 함(네이티브 전용 — 문서화)
- [x] `opacity` (×410) — 07-11: ≈0이면 서브트리 페인트 스킵 (부분 투명 합성은 미지원)
- [x] **`box-shadow`** (×186) — 07-16: 첫 레이어를 flat 오프셋 rect로
      근사(dx/dy/색; blur·spread·inset 무시). radius 따라감
- [x] **`overflow: hidden` 클리핑** (×136) — 07-16: 디스플레이 리스트에
      클립 push/pop 명령(kind 6/7, 뷰포트 컬링에도 살아남는 균형 쌍),
      Rust 래스터라이저에 클립 스택 필드(blend·fill_rect 존중).
      paint_after() 훅으로 자식 이후 pop. overflow-x/y·clip·scroll 포함
- [x] **`z-index` 쌓임 순서** (×91) — 07-16: paint_tree가 컨테이너별로
      자식 서브트리를 (z-index, 문서순서) 안정 정렬(전역 아닌 컨테이너별 =
      각 positioned+z-index가 스태킹 컨텍스트를 이루는 근사). positioned
      박스의 정수 z-index만 참여, 나머지는 문서 순서. 음수-z는 부모 배경
      뒤로는 못 감(근사)
- [x] **`linear-gradient`** (×36) — 07-16: 첫 색 스톱을 단색 배경으로
      근사(gradient_color — #hex/rgb/named 추출). 실제 그라디언트 미지원
- [x] **`@font-face` 웹폰트 로드** (×4) — 07-18: CSS 소스에서
      디스크립터 추출(`browser/webfonts.py` — family/src/weight/style,
      data: URL의 `;base64` 포함) → 로더블 소스(ttf/otf/data:)만 페치
      (request_raw, 페이지 URL 기준 resolve) → FontStore 동적 등록
      (`add_font` — 정적 테이블보다 우선, 볼드/이탤릭 변형 매칭,
      memo만 무효화해 기존 variant id 유지) → 폰트 캐시 클리어 후
      첫 레이아웃. 미지 패밀리도 등록돼 있으면 통과(has_family).
      E2E: file:// 웹폰트가 p에 실적용·실측 폭 차이 확인.
      갭: **woff/woff2 미지원**(fontdue에 인플레이터 없음 — 실사이트
      대부분이 woff2라 로컬 검증 필요, 스킵은 무해), CSS 파일 기준
      상대경로(지금은 페이지 기준), unicode-range
- [x] input `placeholder` 표시 — 07-11 ("검색어를 입력해 주세요." 회색 렌더,
      `input[type=hidden]` UA 룰 포함)

### 셀렉터·스타일
- [x] **속성 셀렉터 `[attr=...]`** (×636) — 07-11: 존재/`=`/`~=`/`^=`/`$=`/`*=`/`|=`
      + 따옴표/케이스 플래그, Python·Rust 미러
- [x] **`:not()`** (×38), **`:nth-child`** (×19), `:first-child`/
      `:last-child` — 07-16: Python·Rust 양 엔진 미러(nth는 odd/even/N,
      an+b는 룰 거부). 클래스급 명시도
- [x] **`@media (min/max-width)` 평가** (×5) — 07-16: 뷰포트 폭(기본
      데스크톱 1280px, compute_styles에 옵션 배관)으로 조건 판정 —
      매칭 시 내부 룰 언랩, 아니면 블록 스킵. `screen`/`all`/`and`/콤마·
      px/em 지원, 미지원 피처는 룰 유지. Python·Rust 양 엔진 미러

### 레이아웃
- [x] **`line-height`** (×380) — 07-16: normal(1.25 유지)·unitless·px·
      em/rem·% → 줄 박스 높이 배율. LineLayout 베이스라인/높이에 반영.
      **07-19 실증 결함 발견**: 블록 요소(p/div) 직속 텍스트에 무시됨 —
      Rust export의 Text 상속 화이트리스트에 line-height가 빠져 있고
      LineLayout이 Text 노드에서 읽는 구조. 인라인(span) 직접 지정만
      동작(46.56 vs 23.28px 실측, validation/css_gauntlet.py). 수정 필요
- [x] **`white-space: nowrap`** (×83) — 07-16: word()에서 줄바꿈 억제 +
      **`text-overflow: ellipsis`** (×84) — 단일 라인 오버플로를 잘라 "…"
      (비상속 속성이라 요소 조상에서 읽음; measure 이분탐색 절단)
- [x] **flex 심화** (×104) — 07-18: `justify-content`(center/flex-end/
      space-between/space-around/space-evenly — 행별 잔여 공간 분배,
      auto 마진이 흡수했으면 무동작), `align-items`/`align-self`
      (center/flex-end — 교차축은 배치 후 서브트리 시프트, 재레이아웃
      없음; stretch 크기 늘림은 미지원 → **07-23: align stretch 지원**
      (auto 높이 항목이 행 교차크기로 늘어남, 8660f7c)), `flex-shrink`
      (nowrap 단일 행 오버플로를 shrink×크기 비례로 반납, min-content
      바닥 없음 → **07-23: iterative 해석 + min-content 바닥 + max 클램프**
      (e81bc4e), **`gap`/`flex-direction:column` grow/justify/align**
      (dd0322f·4f2d3aa)),
      `flex-basis` + **`flex` 축약형**(1 / 0 0 200px / none — 양 엔진
      미러, `flex:1`은 스펙대로 basis 0). 덤 버그 수정: `"wrap" in
      "nowrap"`이 참이라 **모든 flex 컨테이너가 랩 모드였음** — 이제
      정확 매칭(스펙 기본 nowrap + shrink; 기존에 우연히 랩에 의존한
      렌더는 달라질 수 있음 — 바스켓 재검증 필요)
- [x] **CSS `width/height`가 대체 요소에 적용** — 07-18: img/svg의
      인라인 이미지 박스가 CSS 크기를 HTML 속성보다 우선 사용(% 폭은
      컨테이닝 블록 기준, 한쪽만 지정 시 고유 비율 유지; % 높이는
      기준 없음 → 속성/비율 폴백). input은 블록 박스 모델이라 기존에
      이미 적용됨
- [x] **`float` + `clear` v1** (×25) — 07-18: 블록 모드를 증분 배치로
      전환(앞선 float가 등록돼야 뒤 형제가 회피 가능). ~~**폭 명시된
      float만 참여**(auto 폭은 일반 흐름 폴백 — v1 게이트)~~ → **07-23:
      auto 폭 float도 shrink-to-fit로 참여, 플로팅 요소는 block-level로
      승격(`<p><img float>text</p>` 이미지가 실제 float, 45315df)**:
      좌/우 가장자리에 흐름 하단 y로 배치, 같은 y의 기존 float 뒤로
      스택. 자동 폭 in-flow 블록은 상단이 겹치는 float만큼 x 시프트+
      폭 축소(라인박스 단위가 아니라 블록 통째 회피 — 근사).
      `clear`: left/right/both가 해당 float 바닥 아래로 강하.
      컨테이너 높이는 float 바닥 포함(클리어픽스형 봉쇄).
      갭: 인라인 흐름 안의 float, float 아래로 텍스트 재확장,
      margin 있는 float의 스택 x 근사
- [ ] inline-block 정식 배치 (지금은 근사)
- [x] **margin collapsing** — 07-23: 인접 블록 형제의 세로 마진 병합
      `max(mb,mt,0)+min(mb,mt,0)` (CSS 2.1 §8.3.1). 문단 32px→16px 간격,
      example.com 높이 587→555 (efc6152)
- [x] **grid** — 07-23: `display:grid` 전체 구현 — 트랙(fr/minmax/repeat/
      rem/%), `grid-template-areas` named-area 배치, gap, column/row span.
      네이버 홈엔 없지만 위키백과 Vector 3열 셸·카드 그리드가 실제 열로
      배치됨 (0012311). **sticky**는 여전히 흐름 폴백(안전 근사),
      `position:fixed`는 뷰포트 기준으로 분리 (0603521)

---

## M2 — gg-js 언어 완주: 번들 4개(1.3MB) 컴파일+실행 통과 — 95% (07-16)

문법 대형 관문은 전부 격파(ES2020 대부분: 클래스 상속·async/await 전 위치·
spread/rest·구조분해 전 형태·옵셔널 체이닝). 남은 건 싱글턴 롱테일:
제너레이터 `function*`, private 필드(`#x` 추정), expression too deep 1건
(tmp=250 고갈), 런타임 "yet" 부류(엔진 bail → 캐치 가능한 JS TypeError로
전환하는 캠페인이 systemic fix — polyfill anObject 장기전과 연결).

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
- [x] **`expression too deep` 격파 — M2 중간 이정표: 번들 4개 전체 컴파일
      완주!** — 07-12: 범인은 Seq(콤마 시퀀스) temp 누수(비마지막 항 슬롯
      눌러앉음 — core-js 엔트리의 수백 항 `e(id),e(id),...`). 체크포인트
      반납으로 수정. + Function.prototype.**bind**(Bound 클로저) +
      **uncurry 인프라**(FP.apply/call/bind 추출·재적용·직접형·관용)
- [x] **class 선언·extends·super·static·필드·get/set** — 07-16: 전 클래스
      IIFE 디슈가(프로토타입 메서드 + Object.create 체인). super는 파서
      문맥 재작성(sup.call/apply). 인스턴스 부착 방식은 상속 비호환이라 폐기
- [x] **spread/rest 전체** — 07-16: rest 파라미터(arguments.slice 디슈가),
      객체 리터럴 spread(Object.assign 디슈가), 객체 패턴 rest(복사+delete).
      배열/호출 spread는 기존 [].concat. + RegExp 생성자
- [x] **await 표현식 위치** — 07-16: 정규화 패스(중첩 await → `var __awN=`
      승격) + chain_async 확장(If분기 IIFE, 루프는 재귀 프로미스 체인).
      return/break 낀 분기·루프만 기존 에러 유지
- [x] **try/catch 안의 await** — 07-16: rejection→catch 체인
      (`P.resolve(IIFE).catch(handler).then(fin,fin)`). return은 마지막
      문장 위치면 허용(값 전파). If 암에도 동일 개선
- [x] **소품 문법 5종** — 07-16: 객체 리터럴 get/set, for-of 구조분해
      헤드, 구조분해 대입 표현식, `{a=1}` shorthand 디폴트, `f?.(...a)`.
      + Array.splice

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
- [x] **getter/setter + 디스크립터 API (보스 3페이즈)** — 07-15/16:
      defineProperty/getOwnPropertyDescriptor(객체·함수)/create/
      get·setPrototypeOf, 접근자 사이드테이블(IC 공존, 프로토 접근자를
      인스턴스 this로), 추출 배열/문자열 빌트인 확장, 정규식 폴백,
      typed-array 스텁 11종, nullish 쓰기 관용.
      **polyfill: 엔진 관문 소진 — 남은 건 JS 레벨 TypeError 2종**
      (bound 생성자 new 시맨틱, anObject 감지 — 메모리에 수정안 기록)
- [x] **`Object.defineProperties`/`getOwnPropertyNames`** — 07-18:
      defineProperties는 defineProperty와 적용 로직 공유(define_one_prop
      추출), getOwnPropertyNames는 인덱스 키 + shape 프로퍼티 +
      **접근자 사이드테이블 키**(Object.keys가 못 보는 accessor-only 키
      포함), 함수 타깃은 정적 프로퍼티 + prototype
- [x] **`Symbol`(페이크)** — 07-16: 문자열 기반(프렐류드 JS) — 유일값·
      well-known 6종·for/keyFor. 갭: typeof가 'string'
- [x] **이터레이터 프로토콜 (for-of)** — 07-16: IterMaterialize 명령 —
      배열/문자열 무비용 통과, Map/Set 직접, @@iterator 보유 객체는
      프로토콜 드레인(10만 회 예산). 제너레이터·커스텀 이터러블·
      `for ([k,v] of map)` 전부 동작. 갭: spread([...set])는 concat
      디슈가라 미적용
- [x] **`Map`/`Set`(×9)/`WeakMap`(×2)/`WeakSet`** — 07-16: Rust 네이티브
      (map_data/set_data 사이드테이블 + 인스턴스 인덱스 담은 Native 변형).
      get/set/has/delete/clear/forEach(do_native에 mods 스레딩)/keys/
      values/entries. size는 변이마다 갱신되는 데이터 prop. 배열 시드·
      체이닝·객체 키 identity. keys/values/entries는 배열 반환(이터레이터
      대신 — for-of 즉시 동작)
- [ ] `Proxy`(×1), `Reflect.*`(×10)
- [x] **제너레이터 `function*`/`yield`** — 07-16: 파서 자기완결 상태머신
      디슈가(switch 세그먼트 + 로컬을 외부 스코프 호이스팅 = 클로저 셀로
      지속, sent 값은 세그먼트 시작 대입). 문장 레벨 yield·객체/클래스
      `*gen()`·next/return/throw·@@iterator. **루프/조건 안 yield와
      `yield*`는 명시 에러**(조용한 오답 금지 원칙). VM 무수술
- [x] **private 필드 `#x`** — 07-18: 렉서가 `#`+식별자를 '#x' 이름의
      Ident로 — 필드/메서드가 '#x' 키 프로퍼티로 디슈가되고 일반 멤버
      문법으로는 그 이름을 쓸 수 없어 구성상 프라이빗. 인스턴스별
      상태 분리·프라이빗 메서드 검증. 갭: `#x in obj` 브랜드 체크
- [x] **런타임 yet → JS TypeError 전환 캠페인** — 07-16: VmError.kind
      (TypeError/ReferenceError 분류) + exception_value가 프렐류드
      프로토타입 체인에 연결(instanceof·name·toString 실물). 동작화:
      odd-key 인덱싱·음수/소수 인덱스·문자열/객체 propertyIsEnumerable·
      원시 인덱싱. 덤: `var u;` 호이스팅 버그 수정, `new C(...args)`
      (_construct 디슈가), 메서드 shorthand 3종(문자열/숫자/computed 키)
- [ ] 실행 성능: 1.3MB를 JS 예산 내 실행 (IC 확대; baseline JIT은 후순위)

---

## M3 — 웹 플랫폼 API 표면 (앱 부팅, 주~월 단위)

### DOM/이벤트
- [x] `window`/`document` 레벨 addEventListener — 07-11 (document 실등록,
      window는 수용 후 무시; console error/warn/info/debug도 추가)
- [x] **이벤트 버블·`removeEventListener`(×22)·`dispatchEvent`(×4)/
      `CustomEvent`(×3)** — 07-16: Event/CustomEvent 프렐류드 생성자 +
      dispatchEvent 네이티브(리스너를 this=노드로 호출, bubbles 워크,
      stopPropagation/preventDefault→반환값), removeEventListener 실구현.
      캡처 단계는 미지원(문서화)
- [x] **`classList`(×13)** — 07-16: add/remove/contains/toggle
      (node id를 담은 Native::ClassList 변형이 class 속성 실조작 —
      className과 일관)
- [x] **`dataset`·`el.style.prop =` 개별 세터** — 07-16: 프록시 객체
      사이드테이블(style_nodes/dataset_nodes) — Get/SetProp 훅이
      camelCase↔kebab 변환해 인라인 style/data-* 속성으로 라우팅.
      cssText 포함
- [x] **트리 API** — 07-16: insertBefore/removeChild/replaceChild/
      cloneNode(deep)/contains + parentNode/children/childNodes/
      firstChild/tagName/nodeType 게터. innerHTML get/set은 기존 동작
- [x] **`getBoundingClientRect` 실측** (×4) — 07-18: 셸이 레이아웃
      직후 주요 박스(Block/Image)의 (x,y,w,h)를 `Doc.set_layout_rects`
      로 Rust VM에 푸시(St.layout_rects) → 이후 이벤트 핸들러의 gBCR이
      실제 지오메트리 반환(E2E: 클릭 핸들러가 200×40 @21 읽음).
      문서좌표 근사(스크롤 미반영 — 뷰포트 상대는 후속), 첫 레이아웃
      전엔 기존 제로렉트. **덤 버그 수정: addEventListener 핸들러의
      this가 undefined였음** → click/라이프사이클 디스패치가 this=노드
      (window 센티널은 실제 window 객체 — 가짜 dom_node면 아레나 밖
      인덱싱 패닉)로 호출
- [x] `getComputedStyle` — 스텁(el.style 프록시 반환, 프렐류드) 확인 07-18

### 브라우저 객체
- [x] **`location.*`(×19), `history.*`(×2), `navigator.*`(×2),
      `performance.*`(×3), `screen`** — 07-16: location은 실제 페이지
      URL로 채움(PageVm.set_page_url ← lib.rs 바인딩 ← native.py
      page_url 파라미터 ← browser/shell 호출부). performance.now는
      가상 시계 연동. history/screen은 정적+no-op
- [x] **`localStorage`(×9)/`sessionStorage`(×1)** — 07-16: 인메모리
      key-value(Native::Storage, getItem/setItem/removeItem/clear/key).
      디스크 영속화는 후순위. + getBoundingClientRect 제로렉트 스텁
      (레이아웃이 JS 이후라 실측 불가 — 크래시 방지) + focus/blur/
      scrollIntoView 등 no-op 6종
- [x] **`document.cookie`(×4)** — 07-16: 인메모리 저장소 왕복(St.cookies,
      속성 무시·업서트). 네트워크 계층 연동(요청에 실어 보내기)은 후속
- [x] **`requestAnimationFrame`(×13)** — 07-16: 16ms 가상 타이머
      (Native::Raf, 콜백에 타임스탬프). cancelAF/requestIdleCallback 포함.
      프레임 "루프"(M4 렌더 통합)는 별도
- [x] `IntersectionObserver`(×1)/`ResizeObserver`(×2) — 스텁 + 발화
      구현돼 있음 확인 07-18 (프렐류드: IO는 "모두 가시"로 즉시 발화 —
      lazy 콘텐츠가 즉시 로드됨. MutationObserver·customElements·
      AbortController 포함)
- [x] `matchMedia` (×2), `postMessage` (×8), `scrollTo` (×11) —
      구현돼 있음 확인 07-18 (matchMedia는 matches:false 객체,
      postMessage/scrollTo는 window noop)
- [x] **canvas 2D 크래시 방지 스텁** (×9) — 07-18: `getContext('2d')`가
      스텁 컨텍스트 반환 — 드로잉 호출 28종 noop 수용, measureText/
      getImageData/createImageData는 제로 메트릭·빈 데이터 객체,
      그라디언트/패턴은 addColorStop noop 객체, canvas 역참조·
      fillStyle 등 상태 프로퍼티 보유. webgl 등 비-2d는 null(정직한
      피처 디텍션), `toDataURL()`은 `"data:,"`. 실렌더는 후순위
- [x] `fetch`/XHR — fetch는 가상 시계 서비스 네이티브, XHR은 fetch 위에
      랩(GET, onload/onreadystatechange) — 확인 07-18. main 번들엔 0회

---

### B단계 지름길 (정공법과 병행 가능)
- [x] **EAGER-DATA 리더 모드** — 07-16 완료: `browser/reader.py` —
      `window["EAGER-DATA"][key]` JSON에서 (title, url) 채굴(범용 hunt,
      광고 키 스킵) → `</body>` 직전에 self-styled 섹션 주입(파스 전
      HTML 소스 단계 = 양 엔진·레이아웃이 일반 마크업으로 처리).
      **네이버 paint 10→292 명령, 텍스트 1→271**: 연합뉴스 실시간
      헤드라인 10건 + 관심사 피드 + 언론사 246곳이 화면에 렌더.
      타 사이트 no-op·광고 스킵·실패 무해(inject는 예외 시 원본 반환).
      리더 모드는 추후 AI 리더 모드의 토대(확장 구상 참조)

## M4 — 동적 렌더 루프 (v1 가동, 07-17)

- [x] **라이브 이벤트 루프 v1** — 07-17: `pump_bounded`(dt 안에 due인
      타이머/rAF만 발화, 시계를 dt만큼 전진 — 로드 시 settle의
      fast-forward와 대비되는 실시간 페이싱) + `Doc.tick(dt)`/
      `dom_version()`(Document.version — 6개 변이 메서드가 bump) +
      browser.py `_live_tick`(80ms tkinter after 루프: tick→fetch
      서비스→버전 변화 시 refresh+relayout, 세대 가드로 네비게이션 중단,
      12초 무변화 시 500ms 백오프 — 정지 아님, dt는 실경과시간).
      demo_live.html로 실검증(초시계·rAF 카운터·지연 문장·리스트 성장)
- [x] **부분 재스타일/재레이아웃 v1** — 07-18: `Doc.restyle_diff` —
      Rust가 재스타일 후 이전 계산 스타일과 diff해 피해 등급을 보고
      (0 무변화 / 1 페인트-온리: 노드별 패치 반환 / 2 지오메트리 /
      3 구조 — pseudo 증감). **페인트-온리면(hover/focus의 대부분)
      기존 Python 트리에 스타일만 제자리 패치하고 리페인트만** —
      export·트리 재구축·레이아웃 전부 생략. 지오메트리/구조는
      재-export(스타일 재계산은 생략)+재레이아웃. 페인트-온리 판정은
      화이트리스트(color·background·border-color·box-shadow·opacity·
      radius·transform 등 — 모르는 속성은 보수적으로 지오메트리).
      3,506노드 실측: hover당 114→53ms(2.2×; diff+패치 14 + 리페인트
      34). 잔여: 리페인트 자체의 부분화(디스플레이 리스트 캐시 —
      M6 스크롤 60fps와 합류), 라이브 틱의 구조 변이 경로는 여전히
      전체 재구축
- [ ] winit 셸(shell.py --native)에도 라이브 루프 배선 (현재 tkinter만)
- [x] **`transform`** (×646) — 07-18: translate/translateX/translateY/
      translate3d/matrix(e,f)의 이동 성분을 페인트 오프셋으로 적용
      (px·% — %는 스펙대로 자기 border box 기준, 다중 함수 합성,
      중첩 조상과도 합성). scale(0)/matrix(0,..,0,..)은 서브트리 숨김.
      명령 클래스가 전부 left/top/right/bottom 좌표라 translate_cmds
      일괄 이동 — tkinter·native 양 렌더 경로 공통. 레이아웃 비영향
      (paint-only, 스펙 일치). 미지원: 회전·비영 스케일 렌더,
      인라인 요소 transform, 히트테스트 반영(클릭 좌표는 원위치)
- [ ] `transition`(×98) / `@keyframes` 애니메이션(×163) — 시각 완성도
- [ ] 스크롤 리페인트 성능 (디스플레이 리스트 캐시/타일)

---

## M5 — 상호작용 완성

- [x] **텍스트 입력 포커스/캐럿/타이핑** — 07-18: 클릭 히트테스트 →
      input 포커스(is_focused) → `<Key>`로 타이핑/백스페이스(값은
      value 속성에, **Rust DOM에도 set_attr 미러 — 페이지 JS가 입력값
      읽음**), 캐럿은 값 끝에 DrawLine 페인트, 리페인트는 재레이아웃
      없이 디스플레이 리스트만 재생성. Esc/빈 곳 클릭 언포커스,
      트리 재구축 시 포커스 해제. UA 시트에 `input{height:1.5em}`
      (맨 input 높이 0 → 클릭 불가 버그 수정). 잔여: **IME 한글
      조합**(tkinter 캔버스 IME — 로컬 윈도우에서 검증 필요),
      캐럿 이동/선택, native(winit) 셸 배선
- [x] **`:hover`/`:focus` 동적 재스타일** (×231/×28) — 07-18:
      Python·Rust 양 CSS 파서가 :hover/:focus 셀렉터 수용(클래스급
      명시도), 매칭은 상태 기반 — Python은 node.is_hovered/is_focused
      마크, Rust는 Document.hover_chain(요소+조상)/focused를
      셸이 set_hover/set_focus로 세팅. on_motion(30ms 스로틀)이
      hover 요소 변화 시에만, 시트에 :hover 룰이 있을 때만 재스타일
      (없는 사이트는 무비용). 조상-hover 자손 룰(`.m:hover .sub`)
      동작. **재스타일이 트리를 재구축해도 포커스·hover를 Rust 인덱스로
      리맵** — 라이브 틱 중 타이핑도 이제 살아남음. 잔여: 전체
      재스타일이라 네이버 규모에선 M4 부분 무효화와 합류 필요
- [x] **폼 제출 (GET 쿼리 조립)** — 07-18: `browser/forms.py` —
      Enter → 조상 <form> 탐색, input 필드 직렬화(name 있는 것만,
      submit/button류 제외, checkbox/radio는 checked만, 한글
      percent-인코딩), action의 기존 쿼리 대체 후 resolve·이동.
      POST는 미지원 안내. file: 스킴이 쿼리를 경로에 섞던 버그 수정
- [x] **쿠키 세션 유지** — 07-21 `document.cookie` ↔ 네트워크 자 브리지
      (코덱스 5f3c8af) + 07-23 **속성 인식 자**(35448e3): Set-Cookie 속성
      파싱, Domain 서브도메인 공유(.naver.com → nid/www; cross-site Domain
      거부), Path 스코핑, Expires/Max-Age 만료(=0 삭제), Secure(https 한정),
      HttpOnly(document.cookie 숨김·요청엔 전송). 12/12 유닛. (로그인 자체는
      여전히 범위 밖 — 서버 인증 플로우)
- [ ] iframe (홈 셸엔 0개; 광고·로그인에서 등장 — 후순위)

---

## M6 — 성능 (실측 1.19s 기반, "크롬급 체감"까지)

체감 순 정렬. 상단 두 개가 현재 시간의 98%를 지운다.

- [x] **디스크 캐시** — 07-11: `%LOCALAPPDATA%/gg-browser/cache`, 명시적
      max-age 응답만(HTML은 신선 유지). 실측: CSS 195→13ms, JS 7종 545→52ms,
      **전체 재방문 로드 1.19s → 0.18s**
- [x] **JS 게으른 컴파일(lazy compile)** — 07-18: 함수 리터럴의 본문
      코드젠을 첫 호출까지 미룸. 지연 시점에 free_vars로 자유변수를
      선해석해(중간 함수 업밸류 스레딩·스필 객체 라우팅 포함) 캡처가
      실물인 스텁 proto를 만들고, VM 호출 경로(ensure_compiled)가 첫
      호출에 본문을 별도 모듈로 컴파일·로드한 뒤 클로저 레코드를
      패치(재호출은 다이렉트). 중첩 함수는 재귀 지연 = 호출 안 되는
      함수는 스텁조차 안 생김. ModStore(RefCell+Rc)로 실행 중 모듈
      추가. 합성 웹팩형 605KB 실측(release): **컴파일 55→14ms(-74%),
      protos 7,501→1,501개**, 웜 실행 저하 없음(0.5ms). 부수 효과:
      콜드 함수의 본문 오류가 스크립트 전체를 못 죽이고 호출 시
      catch 가능한 SyntaxError가 됨 — 실제 JS 시맨틱에 더 가까움.
      **+ lazy parse도 완료(07-18 오후)**: 24토큰 이상의 순수 함수
      본문(async/제너레이터/파라미터 프롤로그/super 재작성 제외)은
      AST도 안 만들고 토큰 범위만 기록 — 스킵 중 자유변수 후보를
      토큰 스캔으로 과대근사 수집(`.`/`?.` 뒤 프로퍼티명만 제외,
      템플릿 `${}` 구멍은 원문 단어 스캔). 첫 호출에 파스+컴파일.
      최종 실측(605KB): **프런트엔드 108ms(eager) → 30ms(-72%)**
      (lex 12.5 + parse 11.5 + 스텁 6). 웜 실행 저하 없음.
      네이버 웜 로드 1,003ms 재실측은 로컬에서 (이 세션은 egress
      제한).
- [x] **인터프리터 O(n²) 핫패스 제거 (네이버 settle ~57s → ~5s)** —
      07-23: 네이버 클라이언트 렌더를 페이즈·오퍼코드·수신자별로 격리
      프로파일(임시 계측 후 제거)한 결과, settle 전체가 **한 pump
      라운드**에 몰려 있고 그 라운드의 명령/초 처리량이 힙이 커질수록
      20배 붕괴 = 디스패치 처리량이 아니라 몇 개의 핫패스에 숨은
      O(n²)였다. 네 곳을 고쳐 제거:
      (a) `intern()` — 문자열 아레나를 선형 스캔으로 디듑 → `for..in`/
      Object.keys 키 인터닝이 키당 O(n), 빌드 전체 O(n²). content→index
      해시맵(flat_index)로 O(1). 인터닝된 Flat은 불변이라 무효화 없음.
      (b) charAt/charCodeAt — 매 호출(직접형 + core-js uncurried
      `fn.apply` 형)마다 전체 UTF-16 유닛 벡터를 재구성 → 문자 스캔이
      O(n²). 문자열당 단일 유닛 메모(units_cache)를 str_char_read로
      경유해 순차 스캔 O(n) 상각. 40만자 스캔 12.4s → 0.07s(175×).
      (c) 문자열 `.length` — 읽을 때마다 UTF-16 유닛 O(n) 재카운트 →
      스캐너의 `while (i<s.length)`가 O(n²). 문자열당 길이 메모
      (ulen_cache/str_u16_len).
      (d) method_ref_dispatch 배열 수신자 — 호출마다 수신자 배열 전체를
      선(先)클론 → `push.apply(acc, chunk)`가 O(n²). 제자리/스캔 연산
      (push/pop/shift/unshift/reverse/indexOf/lastIndexOf/at) 무(無)클론
      패스 추가, slice는 요청 범위만 클론.
      전부 표준 준수 문자열/배열 시맨틱(네이버 전용 아님). cargo 189,
      smoke 143(네트워크 게이트된 google 리다이렉트만 실패=환경 기준선).
      → line 619 baseline JIT 항목이 예견한 "실행이 새 병목" 지점에서
      먼저 알고리즘 병목을 제거한 것. JIT은 여전히 후순위.
- [x] **스크롤 60fps (native 셸 기준)** — 07-18: 디스플레이 리스트를
      **Rust에 상주**(set_display_list — 페인트 변화 시에만 문서좌표·
      기기픽셀로 1회 직렬화), 스크롤 프레임은 오프셋만 전달
      (render_frame/render_frame_raw) — 뷰포트 컬링(클립 브래킷은
      생존)·시프트·래스터 전부 Rust 안. 3.5k노드/4.2k cmd 페이지
      실측(리눅스 컨테이너): **native 셸 경로 2.0ms/frame(≈488fps)**
      — 07-16의 재직렬화 13ms 항목 자체가 소멸. tkinter 셸은
      rust 2.2ms + tk PhotoImage 스왑 25.5ms = 잔여 병목이 tk 고유
      한계(탈 tkinter 방향 그대로). 윈도우 실기 재실측 필요.
      + **리눅스 폰트 폴백**(DejaVu/WQY 테이블, 로드된 테이블 기준
      변형·한글 폴백 체인) — headless CI에서 네이티브 렌더 경로가
      처음으로 실검증 가능해짐(P5 headless Linux 빌드의 전초)
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

- [x] **`display: grid`** — 07-23: named-area·fr/minmax/repeat·gap·span
      전체 구현(0012311). `position: sticky`는 흐름 폴백(근사),
      `position:fixed`는 뷰포트 분리(0603521)
- [x] **테이블 레이아웃** — 07-23: `display:table/-row/-cell`+`<table>`
      오토 컬럼 알고리즘(min/max-content, colspan/rowspan, 행/셀 배경).
      HN 겹침 9→0 (f1ca219)
- [~] **`overflow: auto` 내부 스크롤 영역** — 07-23: 클립 대상에서
      **의도적으로 제외**(비스크롤 풀페이지 렌더에서 과소계산 높이로
      클립하면 읽을 내용을 가림). hidden/clip/scroll은 클립
- [~] 폼 컨트롤 렌더링 — 07-23: `<input>`/`<textarea>` 기본 폭·
      checkbox/radio 크기, %폭 인라인블록(97ee65a). `<select>`는 잔여
- [x] ~~트랜스파일 안 된 모던 JS~~ — 07-16: 구조 분해(선언·대입·for-of 헤드)·
      async/await(전 위치)·클래스 상속·spread/rest **완료**. 잔여:
      제너레이터·**ES 모듈**(import/export)
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>`/`<audio>`/WebGL (유튜브·지도류 — 사실상 별개 프로젝트)
- [~] iframe 문서 격리, CORS, 쿠키 — 07-21: **`document.cookie` ↔
      네트워크 쿠키 자 브리지**(세션 쿠키 왕복: 응답 Set-Cookie 캡처 →
      요청 Cookie 헤더 재생 → 스크립트가 읽고 쓴 값 반영, 5f3c8af).
      호스트별 저장(도메인 간 누출 없음). 잔여: 전체 속성(Path/Domain/
      Secure/Expires), iframe 격리, CORS
- [ ] HTML5 오류 복구 알고리즘 완전판, quirks 모드, **EUC-KR 등 레거시 인코딩**,
      RTL/양방향 텍스트
- [x] 진행 지표: **사이트 바스켓** — 07-16: `E:\gg\basket_test.py` 상설화
      (노드/스타일/페인트/텍스트/JS오류/시간 스코어보드). 8개 중 7개
      "읽을만함", 크래시 0. 정부24 저페인트는 mega-menu display:none
      정상 처리로 판명. 개선 여지: 이미지 미로드(스크립트 한계)
- [ ] 바스켓 **20개로 확장** (쇼핑몰·SPA·커뮤니티 추가) — "실용적 완벽"
      (= 매일 쓰는 사이트 20개가 잘 뜨고·빠르고·입력됨)의 측정판.
      B+C+D 단계의 결승선 지표

---

## 확장 구상 (주차장 — 집부터 짓고 나서)

07-16 논의. 엔진(B단계)이 우선이라 보관만:

- **AI 통합 — 방향 합의됨**: 크로미엄 스킨들의 사이드바 모방이 아니라
  **엔진 수준 통합**. 우리는 파서~레이아웃 전 계층을 소유하므로:
  1. 요약/질문 사이드 패널 (레이아웃 트리 텍스트 추출은 이미 있음,
     Python 셸이라 2~3일 규모. 첫 단계 후보)
  2. **그레이스풀 디그러데이션 AI** — 렌더 실패/빈약 감지 → DOM+인라인
     JSON을 AI에 넘겨 페이지 재구성(리더 모드). 크로미엄 스킨은 렌더가
     안 깨져서 만들 이유가 없는, 우리만의 기능. "완벽히 그리거나,
     완벽히 읽어주거나". EAGER-DATA 리더 모드(M3 지름길)가 토대
  3. 에러 로그 AI 분류 → 체크리스트 갱신 자동화 (개발 루프 도구화)
  - 주의: API 비용·지연·프라이버시(페이지 외부 전송) — 로컬 모델
    (Ollama) 옵션을 열어두는 설계로

---

## 권장 공략 순서 (07-16 갱신)

0. ~~즉효 3종 / M2 파스 관문 21개 / 프로토타입 체인 / 보스 3페이즈
   (getter·setter·디스크립터)~~ ✅ — 번들 4개 컴파일 완주.
1. ~~M1 스윕~~ ✅ — ::before/after·셀렉터·gradient·overflow clip·
   box-shadow·타이포·@media·z-index. **A단계 사실상 달성.**
2. ~~사이트 바스켓 검증~~ ✅ — basket_test.py 상설화, 7/8 읽을만함.
3. ~~모던 JS 대격파~~ ✅ — 클래스 상속·async/await 전 위치·spread/rest·
   구조분해·소품 문법. 티스토리 JS오류 21→7.
4. **런타임 yet → JS TypeError 전환 캠페인** ← 다음 추천. 여러 사이트
   오류 일괄 해소 + try/catch 감싼 번들 전진 + polyfill anObject
   장기전과 같은 계열이라 일석삼조.
5. M3 계속: `document.cookie`(네트워크 쿠키 저장소부터)·rAF·
   location/navigator 채우기·classList — 앱 부팅 표면 확장.
   (병행 옵션: **EAGER-DATA 리더 모드** — 며칠 투자로 네이버 본문
   텍스트가 "보이게" 되는 지름길. 사기 진작 + 추후 AI 리더 모드 토대)
6. M4 부분 무효화 + M6 lazy 컴파일 — 피드가 그려지기 시작하면 성능이 병목.
7. M5 입력/호버 — "쓸 수 있는 브라우저". 제너레이터·ES 모듈은 필요 사이트가
   나타날 때. M6 잔여(JIT·레이아웃 이식·GC)는 B단계 도달 후 측정해서 결정.

*갱신: 2026-07-18 (오전: canvas 2D 스텁·Object.defineProperties/
getOwnPropertyNames 추가, M3 스텁류 실태 반영 — Observer/matchMedia/
postMessage/scrollTo/getComputedStyle/XHR은 코드에 이미 있었는데 문서만
미갱신이었음. + tkinter 폴백 폰트에 .size 부재 버그 수정.
오후: **M6 lazy 컴파일 가동** — 지연 배분 실측용 프로파일 하니스
`cargo test --release -- --ignored profile_phases --nocapture` 상설화.
저녁: lazy parse + M4 transform — smoke 111종).
항목을 완료하면 [x]로 바꾸고 날짜를 적을 것. cargo 156/156, smoke 141종
(엔진 파트; 실네트워크 관문은 egress 제한 환경에서 측정 불가) 기준.
검증 체인: cargo test → maturin build → pip 재설치 → smoke_test.py →
basket_test.py (네이버 단건은 scratchpad diag 스크립트).

07-19 전면 실증(docs/validation-2026-07-19.md): cargo 171/171, smoke
149종 전수(실네트워크 2건 env-skip), lazy 컴파일 protos 7501→1501 정확
재현, 신규 상설 건틀릿 validation/ 4종 — HTTP/1.1 스택 로컬 서버 실증
14/14(keep-alive 접속 1개·디스크 캐시 계층·리다이렉트 캡), gg-js 언어
30종 node v22 대조(CLAIM 20/20, 조용한 오답 0), CSS 실좌표 22/23
(line-height 결함 ↑), 네이티브 래스터 PNG 4장. egress 게이트의 실체는
소켓 차단이 아니라 허용목록 인터셉터의 text/plain 안내문(그래서 basket
예외 0건·전원 "빈약")으로 규명. 오후 egress 허용 후 net.py에 환경 프록시
지원(CONNECT 터널) 추가 → smoke 148/149(잔여 1건은 https 전용 환경의
plain-http 한계), basket 실측 7/9 읽을만함·크래시 0(나무위키는 Cloudflare
봇월), 네이버 실렌더 PNG(546텍스트·웜 3.2s) 확보.*

---

## 07-20 — **네이버 React 커밋 달성** (B단계 실질 진입)

네이버 홈 React(react-dom 18 `createRoot`, CSR)가 gg-js에서 **렌더+커밋을
완주**한다. 빈 `#root`(정적 0노드) → 앱 컴포넌트 트리 **63~99노드** 커밋:
`Layout-module__column_left/right`, `#newsstand`(ContentHeaderView),
`#shopping`, `#feed`(FeedView), `MobileButtonView`(＂모바일 버전으로 보기＂)
— 전부 네이버 실제 CSS-모듈 컴포넌트 출력. 번들 7종 **에러 0**, 언캐치드
예외 0.

**언블록한 엔진 갭 3종** (전부 회귀 테스트 + cargo 177/177):
1. `getOwnPropertyDescriptor(배열,"length"/인덱스)` → `undefined` 반환.
   배열 length/인덱스는 shape 밖에 저장돼 슬롯 조회가 놓쳤음. core-js의
   배열 length 세터가 매 변경 전 `.writable`을 읽어 undefined면
   "Cannot set read only .length" throw → React 커밋 중단. writable
   데이터 디스크립터 합성으로 해소.
2. 배열 이터레이터가 `thisArg`(2번째 인자)를 무시. forEach/map/filter/
   some/every/find/findIndex가 콜백 `this`를 thisArg에 바인딩하도록 수정
   (일반 디스패치 + uncurried 추출 경로 양쪽). 네이버가 싣는 W3C
   IntersectionObserver 폴리필이 `forEach(fn, this)`로 `this._rootContains
   Target`을 읽어서, thisArg 없으면 undefined→throw→React 비동기 작업 중단.
3. `setInterval` 로드-세틀 무한 전진. pump가 quiescence까지 fast-forward
   하는데 인터벌은 quiesce하지 않아 매 pump 콜에서 예산(200k)까지 발화
   (IntersectionObserver 폴 인터벌 → 세틀당 ~50s 낭비). 인터벌은 pump 콜당
   1회만 발화·등록 유지(clearInterval 정상)·has_pending_work에서 제외.

**성능 관찰(측정)**: 페치 7종(feed/news JSON) 해소 후, 한 번의 setTimeout(0)
콜백(=React 렌더+커밋 1틱, 가상시계 동결로 shouldYield 미발동→비분할)이
**~78s / 명령어 13.8M**(≈175K명령어/s, 라운드1의 6.6M/s 대비 40× 저하).
샘플링 프로파일러(임시)로 핫스팟 규명: 상위 2함수가 시간의 ~46% —
(a) 네이버 앱 번들에 실린 **JS 재귀하강 JSON 파서**(≈350KB 페치 데이터를
문자 단위 파싱; 우리 네이티브 JSON.parse가 아니라 앱 자체 파서),
(b) `fn.apply(this, arguments)` 래퍼(≈35만 회 호출). 단일 이차식 버그가
아니라 **어린 인터프리터의 분산된 호출/할당/조회 오버헤드 × 수백만 연산**.
→ 근본 해결은 M6(JIT·인터프리터 스루풋) 영역. 렌더 자체는 **정확·완주**함.

**측정 아티팩트 규명**: 이전 라운드의 "릴리스 빌드에서 DOM 게터 소실"은
**grep 오판**이었다. 릴리스 LLVM이 짧은 문자열-리터럴 `match`를 인라인
정수 즉치 비교로 컴파일해 리터럴 바이트가 .rodata에 연속 ASCII로 안 남음
→ grep 불검출. 런타임 검증 결과 tagName/nodeType/classList/firstChild/
childNodes/parentNode/documentElement/getElementById/activeElement **전부
정상 동작**. DOM 리드 표면은 멀쩡했다.

**추가 수정 (같은 날, 시각 렌더 중 발견)**:
4. `String.prototype.concat` 가변인자. 직접형 `str.concat(a,b,c)`이 첫
   인자만 접합("a".concat("b","c","d")→"ab"), 추출형 `s.concat.call(...)`은
   미지원 에러. 둘 다 recv+전체 인자 접합으로 수정(jindo/core-js 문자열
   헬퍼가 반복 호출). 네이버 렌더 로그의 반복 에러 소멸 + 콘텐츠 증가.

**시각 실증(PNG, xvfb + 네이티브 래스터, cp312)**: 네이버 홈이
**692 페인트 커맨드·545 텍스트·문서높이 5779px**로 그려짐. 확인된 실렌더:
초록 검색 알약, **2026 북중미 월드컵 NAVER 특별 로고**(트로피+축구공
그래픽), 우측 **로그인 박스**(NAVER 로그인·아이디/비밀번호 찾기·회원가입),
**웹툰/웹소설 추천 카테고리 메뉴**(웹툰홈·요일별웹툰·베스트도전·웹소설홈·
시리즈에디션·시리즈홈·만화 — React 렌더), 하단 Partners/Developers 내비,
웨일·N 로고. 언캐치드 예외 0. (뉴스스탠드 카테고리 라벨 가로 겹침 = CSS
포지셔닝 잔여, 레이아웃 과제.)

**설치 함정 기록**: 무버전 `pip`로 cp312 wheel을 --force-reinstall해도
.so가 갱신 안 되는 경우 있음(PEP 668/버전매칭 스킵). `python3.12 -m pip
install --break-system-packages --force-reinstall`로 명시 설치 필요.
초판 PNG는 이 함정으로 옛(수정 전) 엔진이 쓰여 리더모드 콘텐츠였음 → 정정.

*상태: A 달성·B 실질 진입(첫 화면 React 컴포넌트 트리 + 카테고리/로그인/
푸터 실렌더, 5779px). 잔여 B는 피드/뉴스 카드 데이터 심화 + 가로 레이아웃
겹침(M1/CSS) + 성능(M6, 앱 자체 JSON 파서 ~78s).*

**07-20 오후 — 가로 겹침의 진짜 원인은 flex 콘텐츠 사이징이었음(수정)**.
뉴스스탠드/추천카테고리 탭 6개가 한 x(=63)에 완전 포개진 건 CSS 포지셔닝이
아니라 **flexbox main-size 버그**였다. 진단(레이아웃 박스 실좌표 덤프): 탭
내비는 `<ul flex>`(6 `<li>`)를 감싼 `<div flex:1 0 0>`인데, 형제
`<a flex:0 0 auto>`(basis:auto)가 우리 엔진의 "legacy 균등분배"로 행 전체
폭(790px)을 삼켜 `flex:1` 내비가 0px로 붕괴 → 탭 전원 x=63 스택.
**수정**: flex 아이템마다 확정 base = flex-basis(길이) > width >
**max-content**(초광폭 제약 하 서브레이아웃으로 실측, 레이아웃 패스당
메모이즈). free는 flex-grow로 배분, overflow는 flex-shrink×base로 회수.
grow 선언이 하나도 없을 때만 auto-basis 아이템이 잔여를 균등분배(순진한
등폭 컬럼 레이아웃 + 기존 smoke 기대치 보존). 결과: **탭이 가로로 흐름**
(겹침 0), Naver 레이아웃+페인트 ~20ms 유지, css_gauntlet 22/23 불변,
flex smoke 6종 + 신규 `flex:1 0 0` vs 콘텐츠-형제 회귀 전부 통과.
잔여 탭 폴리시(탭 간 여백)는 CSS-모듈 패딩 캐스케이드 심화 영역.

**07-20 저녁 — 피드 lazy 렌더 파이프라인 심층 추적 + IntersectionObserver 언블록**.
네이버 피드/뉴스 블록은 IntersectionObserver로 "화면에 보일 때"만 렌더되는데,
관찰자가 영구히 "아무것도 안 보임"으로 판정해 142KB 페치 데이터가 있어도
카드가 안 그려졌다. 폴리필 내부 메서드를 래핑해 실패 지점을 정확히 규명:
`_rootIsInDom→true`, `_getRootRect→w=0 h=0`(뷰포트 붕괴), `_rootContainsTarget
→false`(트리 미포함 판정). 두 DOM 원시 결함이 원인이었고 둘 다 수정:
1. **뷰포트 메트릭**: `documentElement/body.clientWidth/clientHeight`(+ window
   .innerWidth/innerHeight 등)가 undefined → 폴리필 root rect 붕괴. 1280×5000
   레이아웃 뷰포트 보고(헤드리스 전체 세틀이 아래-폴드 lazy 블록도 보게 tall).
   offsetWidth/Height·scrollWidth/Height·clientTop/Left·offsetTop/Left도 추가.
2. **documentElement.parentNode**: null 반환 → 폴리필 containsDeep가 parentNode를
   document까지 못 걸어올라감 → `_rootContainsTarget=false`. 실 DOM대로
   `<html>.parentNode = document`(parentElement는 null 유지)로 수정.
수정 후 `_getRootRect→1280×5000`, `_rootContainsTarget→true` 확인. 지오메트리
피드백 루프(layout→set_layout_rects→re-settle)를 헤드리스 렌더에 넣으니 노드
610→701, **하단 정책 푸터(회사소개·인재채용·이용약관·개인정보처리방침…) 신규 렌더**.
회귀: document_element_parent_is_document(parentNode/parentElement/containsDeep/
clientWidth), cargo 179/179.

**잔여(피드 카드)의 정직한 정체**: (a) 관찰 대상이 인라인 `<a class=link_headline>`
인데 지오메트리 푸시가 BlockLayout/ImageLayout만 수집 → 인라인 요소는 rect 0
(파이프라인 한계, 부분 우회 검증함). (b) `/pc/lazy` 응답 자체가 미인증 기본값이라
item 배열 대부분 비어있음(blocks-with-items=1). (c) lazy가 다층 캐스케이드(관찰→
렌더→새 관찰자→…). 즉 단일 바운드 버그가 아니라 파이프라인 확장 + 인증 데이터 +
다층 lazy의 합. 우리 JSON.parse·fetch→json→콜백 체인은 142KB에서 완벽 동작 확인.

**07-20 밤 — 완성: 뉴스/피드가 화면에 실제로 그려짐 (오클루더 제거)**.
남은 벽은 데이터가 아니라 **가시성**이었다. `EAGER-DATA`의 실제 연합뉴스
헤드라인 13건이 이미 페인트되고 있었는데(NEWS_IN_PAINT=13), `position:absolute;
height:100%` 장식 오버레이가 전체 페이지를 덮어 눈에 안 보였다. 원인은 절대
요소의 % 높이가 컨테이닝 블록(=문서)에 대해 풀리는데, 헤드리스 전체-페이지
렌더에서 문서 높이가 수천 px라 오버레이가 그만큼 부풀어 뉴스 위를 페인트한 것.
`DocumentLayout._layout_positioned`에서 out-of-flow 레이아웃 동안 문서의
definite_height를 null로 가려 그 %가 auto(콘텐츠 높이)로 폴백하게 수정. in-flow
콘텐츠는 이미 배치돼 영향 없고, 각 절대 박스는 여전히 자식에게 자기 definite
height를 준다. 결과: **오버레이 소멸, 주요 뉴스(연합뉴스) 헤드라인 + 관심사 피드가
화면에 그대로 렌더** — NAVER 로고·검색창·로그인 패널·뉴스스탠드 탭·카테고리 탭·
공지/Partners/Developers·웨일 브라우저·정책 푸터(© NAVER Corp.)까지 완결된 홈.
회귀: smoke 레이아웃/flex/absolute 전 항목 통과(% height auto·vh auto·중첩 절대
포함), redirect 네트워크 테스트만 환경 이슈로 실패(레이아웃 무관). height=3878,
cmds=510, text=373. **네이버 홈이 GG 엔진에서 완전한 페이지로 렌더된다.**

**07-20 심야 — 정공법 완수: 뉴스 피드가 네이버 자체 React DOM으로 렌더 (리더 모드 졸업)**.
사용자가 "완벽" + "정공법(JS 엔진 심화)"을 택함. 리더 모드(EAGER-DATA 텍스트
주입) 대신 네이버 자신의 컴포넌트 트리가 피드를 그리게 만드는 게 목표. 정밀 진단
결과 피드가 안 뜨는 건 CSS 그리드가 아니라 JS 엔진 결함이었다: React가 피드
컴포넌트를 렌더 중 예외로 서브트리를 버림. bytecode 트레이스(GG_JS_TRACE에
module:proto 태깅 추가)로 `AutoRolling` 컴포넌트까지 좁힘 — `e.children`가 빈
배열이라 `child.key`에서 크래시. 원인은 데이터 셀렉터가 EAGER-DATA blocks→
materials→items 변환에 쓰는 빌트인들이 엔진에 없어 변환이 실패→빈 children.
**추가한 엔진 표면**: `Array.prototype.at/flat/flatMap/findLast/findLastIndex`
(직접호출+추출형), `String.prototype.at`, `Object.fromEntries`, `structuredClone`,
`Image` 생성자, `new`가 DOM-노드 반환 생성자를 존중(SelectObj), JS 정규식 번역
(`\uXXXX`→`\u{XXXX}`·클래스 내 리터럴 `[`·`[^]`/`[]`·서로게이트 범위). 이어서
피드가 뜨자 `AutoRolling` 헤드라인 티커(3초마다 새 setTimeout 재장전)가 세틀
fast-forward를 무한 재렌더로 몰아넣음 — **250ms 가상시간 지평선** 도입:
지평선 밖(애니메이션/폴링) 타이머는 프레임 페이스로 미루고 첫 페인트 프레임에서
수렴, has_pending_work도 먼 미래 원샷을 무시. **결과: reader OFF에서 연합뉴스/
관심사 피드가 썸네일 이미지 다단 카드 그리드로 네이티브 렌더**(NEWS_IN_PAINT
4~10/20·각 롤러가 현재 헤드라인 1건씩 실제 네이버처럼 순환, imgs 53~57,
뉴스스탠드 언론사 로고 6곳+1/4 페이지네이션). 리더 모드는 GG_READER=1 옵트인으로
강등(주입 시 lazy 관찰자를 화면 밖으로 밀어 네이티브 카드를 못 뜨게 함).
회귀: cargo 184/184, 헤드리스 smoke 143 PASS(네트워크 redirect만 환경 이슈).
커밋: d66a952(regex/Image/SelectObj), d8ed33f(array/object 메서드+세틀 지평선),
+ reader 게이팅. **네이버 홈이 GG 엔진에서 진짜 자기 DOM으로 완전 렌더된다.**

**07-20 심야2 — 활성 탭 검증: 버그 아님, 서버 데이터 충실 반영 확인**.
"매 로드마다 다른 카테고리 탭(추천/책방/웹툰/패션뷰티)이 활성"이 결함인지
정공법으로 소스까지 추적. `ContentHeaderView`의 활성 탭은 하드코딩이 아니라
`window["EAGER-DATA"]["PC-FEED-WRAPPER"].blocks[0]["@code"]`에서 파생
(`s=Ul.find(tab=>tab.blockCode===blocks[0]["@code"])`). 즉 네이버가 서버에서
내려준 피드의 첫 블록 카테고리가 활성 탭이 됨. naver.com을 연속 fetch하니
body 길이(234021→231382→212546)와 blocks[0] 코드(PC-FEED-CULTURE→
PC-FEED-BEAUTY→…)가 매번 달라짐 — **네이버 서버가 요청마다 피드 리드
카테고리를 로테이션**. 엔진은 `EAGER-DATA` 전역을 HTML에서 재정렬 없이 정확히
읽어 그 탭을 활성화(engine blocks[0]==원본 HTML 확인). **결론: 활성 탭은
네이버 서버 데이터를 충실히 반영한 것이지 렌더 결함이 아니며, "추천 고정"은
실증(엔진이 사이트를 있는 그대로 구동)을 훼손함.** Math.random도 정상 동작
(0.677… 반환) — 콘텐츠 매회 변동은 네이버 자신의 랜덤화. 실제 크롬도 동일.

---

## 07-21~23 — **레이아웃 표준 일반화 ("전체가 돌아가도록") + 쿠키 브리지**

지시: *"레이아웃이 네이버 한정적으로 하지말고 전체가 돌아가도록 해줘."*
`browser/layout.py`를 네이버 튜닝이 아니라 **CSS 표준을 따르는 범용 엔진**으로
일반화. 감사 도구 `scripts/render_audit.py`(사이트별 겹침/오프스크린/붕괴/JS오류
JSON 1줄) + 10개 basket으로 각 변경을 실측 게이트.

### 레이아웃 커밋 20개 (전부 smoke 143 불변 · 회귀 게이트 통과)
- **테이블/인라인/@media** (f1ca219): CSS 테이블 오토 컬럼(min/max-content,
  colspan/rowspan, 행·셀 배경); 인라인 자식만 있는 블록은 인라인 포매팅
  (`<div>a <a>b</a> c</div>` 3줄→1줄); `@media` 멀티라인 조건 파싱(줄바꿈된
  `screen\nand (max-width:750px)`가 데스크톱에 모바일 스타일 누출하던 것 차단,
  Rust+Python 양 엔진 + 유닛테스트)
- **absolute 컨테이닝 블록** (940c4bb): 가장 가까운 positioned 조상 기준으로
  오프셋/％크기 해석(문서 기준 → 표준). 드롭다운/오버레이가 한 좌표에 뭉치던
  최대 결함 해소 — tistory 28→0, yna 236→16
- **마진 병합** (efc6152), **min/max-height + flex gap** (dd0322f),
  **CJK 줄바꿈 + word-break/overflow-wrap** (d92bb56), **폼 컨트롤 기본폭 +
  %폭 인라인블록** (97ee65a), **white-space pre-wrap/pre-line** (b233139)
- **CSS Grid** (0012311): 트랙 fr/minmax/repeat/rem, `grid-template-areas`
  named-area 배치, gap, span. 위키백과 Vector 3열 셸이 실제 열로 배치
- **vertical-align** (b0bbc11), **position:fixed 뷰포트 고정** (0603521),
  **box-sizing:content-box 명시 존중** (a115eeb — 기본 border-box는 유지),
  **flex align stretch** (8660f7c), **인라인 요소 배경** (27ebba7),
  **aspect-ratio** (f6efe7c)
- **z-index 음수 페인트 순서** (ef575ea), **인접 인라인 팬텀스페이스**
  (f813cf3 — `$<b>5</b>`→"$5"), **object-fit/object-position** (e5bdb37),
  **iterative flex min/max 클램핑 + min-content 바닥** (e81bc4e),
  **진짜 flex-direction:column** grow/justify/align (4f2d3aa),
  **auto-width float + float→block-level** (45315df)

### 실측 (render_audit.py, 10개 basket)
**8/10 완전 클린**: example·HN·cern·motherfucking·gnu·danluu·rfc2616·tistory
= 겹침 0. **HN 9→0, tistory 28→0, yna 236→9~12.** 위키백과는 Grid+float로
사이드바·figure가 구조적으로 올바르게 배치(잔여 겹침은 항상 열린 Vector
드롭다운 내비 = 인터랙션 상태 한계, 레이아웃 버그 아님). MDN 메가메뉴도
hover 상태 미모델 — 정적 렌더의 근본 한계.

### 의도적으로 남긴 3가지 (미구현 아님)
- **per-line float intrusion**: 블록 단위 회피가 smoke로 고정된 설계라 유지
  (문단이 float 옆으로 이동은 됨; 라인 단위로 바꾸면 float 테스트 회귀)
- **box-sizing 기본값**: border-box 유지(코드베이스의 의도적 "web reality"
  + `width is border-box` 테스트). 명시적 content-box는 처리
- **object-fit:cover 소스-rect 크롭**: 드로우 레이어에 소스 rect 없어 clip으로
  근사(시각 결과 동일)

### 쿠키 브리지 + JS 엔진 심화 (07-21, 코덱스 5f3c8af — 리뷰·검증 완료)
- **`document.cookie` ↔ 네트워크 쿠키 자**: 응답 Set-Cookie 캡처 → 요청
  Cookie 헤더 재생 → 스크립트 시드/폴드 왕복. 호스트별 저장(도메인 간 누출
  없음). 잔여: 전체 속성(Path/Domain/Secure/Expires)
- 함께 실린 JS 엔진 기능(lodash `_.template` 실행용, 커밋 메시지엔 미기재):
  **`with`문**, **`Function` 생성자**, **`String.replace(함수 콜백)`**,
  정규식 캡처. 전부 유닛테스트 포함
- **검증**: cargo **189/189**, smoke 143(레이아웃 전수, google redirect만
  환경 게이트), 레이아웃 basket 불변(공존 확인), 쿠키 왕복 실동작 확인

*상태: 레이아웃이 네이버 전용이 아니라 표준 기반 범용 엔진으로 일반화됨.
워크플로우 감사 랭킹 1–20 전부 + Grid 구현. 8/10 사이트 클린.*

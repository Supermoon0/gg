# 네이버 "완벽 구동" 체크리스트

목표 정의 — 3단계로 나눠서 "완벽"을 측정한다:

| 단계 | 기준 | 필요한 마일스톤 | 상태 (07-16) |
|---|---|---|---|
| **A. 크롬 JS-off 동급** | 로고·둥근 검색창·아이콘이 픽셀 수준으로 같음 | M1 | **사실상 달성** (M1 주요 항목 완료, 웹폰트·flex 심화 등 소품 잔여) |
| **B. 첫 화면 완성** | 뉴스·쇼핑·피드가 실제로 그려짐 | M2 + M3 + M4 | M2 95% (문법 완료, 런타임 롱테일) · M3 스텁 표면 완료(07-18, 잔여: getBoundingClientRect 실측) · M4 라이브 루프 v1 + **부분 무효화 v1**(07-18) |
| **C. 사용 가능** | 스크롤·호버·검색 타이핑·클릭 이동 | M5 | 스크롤·타이핑·GET 제출·**hover/focus 재스타일**(07-18) · 잔여: IME |
| **D. 크롬급 속도** | 재방문 1초 내 첫 화면, 스크롤 60fps | M6 | 재방문 0.18s ✅ · 60fps 미측정 |

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
      em/rem·% → 줄 박스 높이 배율. LineLayout 베이스라인/높이에 반영
- [x] **`white-space: nowrap`** (×83) — 07-16: word()에서 줄바꿈 억제 +
      **`text-overflow: ellipsis`** (×84) — 단일 라인 오버플로를 잘라 "…"
      (비상속 속성이라 요소 조상에서 읽음; measure 이분탐색 절단)
- [x] **flex 심화** (×104) — 07-18: `justify-content`(center/flex-end/
      space-between/space-around/space-evenly — 행별 잔여 공간 분배,
      auto 마진이 흡수했으면 무동작), `align-items`/`align-self`
      (center/flex-end — 교차축은 배치 후 서브트리 시프트, 재레이아웃
      없음; stretch 크기 늘림은 미지원), `flex-shrink`(nowrap 단일
      행 오버플로를 shrink×크기 비례로 반납, min-content 바닥 없음),
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
      전환(앞선 float가 등록돼야 뒤 형제가 회피 가능). **폭 명시된
      float만 참여**(auto 폭은 일반 흐름 폴백 — v1 게이트):
      좌/우 가장자리에 흐름 하단 y로 배치, 같은 y의 기존 float 뒤로
      스택. 자동 폭 in-flow 블록은 상단이 겹치는 float만큼 x 시프트+
      폭 축소(라인박스 단위가 아니라 블록 통째 회피 — 근사).
      `clear`: left/right/both가 해당 float 바닥 아래로 강하.
      컨테이너 높이는 float 바닥 포함(클리어픽스형 봉쇄).
      갭: 인라인 흐름 안의 float, float 아래로 텍스트 재확장,
      margin 있는 float의 스택 x 근사
- [x] **inline-block 정식 배치** — 07-19: 폭 명시 박스가 라인의 원자
      박스로(InlineBlockLayout), auto 폭은 인라인 폴백(float v1 게이트)
- [x] **margin collapsing** — 07-19: 인접 형제 붕괴(양수 max/음수
      min/혼합 합). 부모-자식 붕괴는 잔여
- [x] ~~grid~~ (×0), ~~sticky~~ (×0) — 네이버 홈엔 없음, 스킵

---

## M2 — gg-js 언어 완주: 번들 4개(1.3MB) 컴파일+실행 통과 — 95% (07-16)

문법 대형 관문은 전부 격파(ES2020 대부분: 클래스 상속·async/await 전 위치·
spread/rest·구조분해 전 형태·옵셔널 체이닝). 남은 건 싱글턴 롱테일:
제너레이터 `function*`, private 필드(`#x` 추정), ~~expression too deep~~
(**07-19 u16 레지스터 파일로 영구 소멸** — react-dom 131KB 컴파일 실증), 런타임 "yet" 부류(엔진 bail → 캐치 가능한 JS TypeError로
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
      07-19: 셸이 스크롤을 푸시(set_scroll)해 뷰포트 상대 좌표(스펙 일치), 첫 레이아웃
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
- [x] **쿠키 세션 유지 v1** — 07-19: net.py 쿠키 자(Set-Cookie 수집,
      동일 호스트 Cookie 헤더) + document.cookie 양방향 동기화.
      로그인(보안 속성 완전판)은 여전히 범위 밖
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

- [x] ~~`display: grid`~~ **v1** — 07-19: template-columns(px/fr/auto/
      repeat)+gap, 행 우선 배치 · `position: sticky`는 흐름 렌더만(핀 잔여)
- [x] **테이블 레이아웃 v1** — 07-19: 균등 컬럼+colspan, 행 그룹.
      자동 컬럼 폭·rowspan 잔여
- [x] **`overflow: auto`** — 07-19: hidden과 동일 클립(내부 스크롤 잔여)
- [ ] 폼 컨트롤 렌더링 (`<select>`·체크박스·라디오)
- [x] ~~트랜스파일 안 된 모던 JS~~ — 07-16: 구조 분해(선언·대입·for-of 헤드)·
      async/await(전 위치)·클래스 상속·spread/rest **완료**. 07-19:
      **ES 모듈 v1**(정적 링커 — browser/esmodules.py) 추가. 잔여:
      루프 안 yield, dynamic import()
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>`/`<audio>`/WebGL (유튜브·지도류 — 사실상 별개 프로젝트)
- [ ] iframe 문서 격리, CORS, 쿠키 전체 속성 (로그인·광고·임베드)
- [ ] HTML5 오류 복구 알고리즘 완전판, quirks 모드, ~~EUC-KR~~(07-19:
      meta 스니핑 + 파이썬 코덱), RTL/양방향 텍스트
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

*갱신: 2026-07-19 — u16 레지스터(React 18 UMD 부팅·리렌더 E2E),
ES 모듈 v1 링커, 테이블/grid/inline-block/margin collapsing v1,
overflow:auto 클립, 쿠키 자+동기화, gBCR 뷰포트 상대, EUC-KR 스니핑.
cargo 174 · smoke 170(GG_SKIP_NET=1+xvfb). 이전 갱신: 2026-07-18 (오전: canvas 2D 스텁·Object.defineProperties/
getOwnPropertyNames 추가, M3 스텁류 실태 반영 — Observer/matchMedia/
postMessage/scrollTo/getComputedStyle/XHR은 코드에 이미 있었는데 문서만
미갱신이었음. + tkinter 폴백 폰트에 .size 부재 버그 수정.
오후: **M6 lazy 컴파일 가동** — 지연 배분 실측용 프로파일 하니스
`cargo test --release -- --ignored profile_phases --nocapture` 상설화.
저녁: lazy parse + M4 transform — smoke 111종).
항목을 완료하면 [x]로 바꾸고 날짜를 적을 것. cargo 156/156, smoke 141종
(엔진 파트; 실네트워크 관문은 egress 제한 환경에서 측정 불가) 기준.
검증 체인: cargo test → maturin build → pip 재설치 → smoke_test.py →
basket_test.py (네이버 단건은 scratchpad diag 스크립트).*

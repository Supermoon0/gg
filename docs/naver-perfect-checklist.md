# 네이버 "완벽 구동" 체크리스트

목표 정의 — 3단계로 나눠서 "완벽"을 측정한다:

| 단계 | 기준 | 필요한 마일스톤 | 상태 (07-16) |
|---|---|---|---|
| **A. 크롬 JS-off 동급** | 로고·둥근 검색창·아이콘이 픽셀 수준으로 같음 | M1 | **사실상 달성** (M1 주요 항목 완료, 웹폰트·flex 심화 등 소품 잔여) |
| **B. 첫 화면 완성** | 뉴스·쇼핑·피드가 실제로 그려짐 | M2 + M3 + M4 | M2 95% (문법 완료, 런타임 롱테일) · M3 스텁 표면 완료(07-18, 잔여: getBoundingClientRect 실측) · M4 라이브 루프 v1 |
| **C. 사용 가능** | 스크롤·호버·검색 타이핑·클릭 이동 | M5 | 스크롤만 (가로/세로) |
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
- [ ] `@font-face` 웹폰트 로드 (×4)
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
- [ ] flex 심화: `flex-shrink/basis`, `align-items`, `justify-content` (×104)
- [ ] CSS `width/height`가 대체 요소(img·svg·input)에 적용 (지금은 HTML 속성만)
- [ ] `float` + `clear` (×25)
- [ ] inline-block 정식 배치 (지금은 근사)
- [ ] margin collapsing
- [x] ~~grid~~ (×0), ~~sticky~~ (×0) — 네이버 홈엔 없음, 스킵

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
- [ ] private 필드 `#x` (연합 "bad class member name: Assign" ×2 추정)
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
- [ ] **`getBoundingClientRect`** (×4) — JS가 레이아웃 결과를 읽는 다리
      (설계 필요; 현재는 제로렉트 스텁 — 크래시 방지)
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
- [ ] **부분** 재스타일/재레이아웃/재페인트 (지금은 변이 시 전체 재계산 —
      껍데기급에선 충분, 피드 규모에서 필요해지면)
- [ ] winit 셸(shell.py --native)에도 라이브 루프 배선 (현재 tkinter만)
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
      번들의 대부분 함수는 호출되지 않으므로 큰 폭 단축 예상.
      07-16 실측으로 시급성 상승: JS 실행이 웜 로드의 **93%(1,003ms)**.
      gg-js 구조 공사지만 V8도 쓰는 정공법.
- [ ] **스크롤 60fps** — 07-16 실측: 재직렬화 13ms + 래스터 26ms = 25fps.
      ① 스크롤은 재직렬화 없이 래스터 단에서 오프셋 처리(=13ms 제거),
      ② 더티 타일/디스플레이 리스트 캐시(M4와 합류)로 60fps 사정권
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
- [x] ~~트랜스파일 안 된 모던 JS~~ — 07-16: 구조 분해(선언·대입·for-of 헤드)·
      async/await(전 위치)·클래스 상속·spread/rest **완료**. 잔여:
      제너레이터·**ES 모듈**(import/export)
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>`/`<audio>`/WebGL (유튜브·지도류 — 사실상 별개 프로젝트)
- [ ] iframe 문서 격리, CORS, 쿠키 전체 속성 (로그인·광고·임베드)
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

*갱신: 2026-07-18 (canvas 2D 스텁·Object.defineProperties/
getOwnPropertyNames 추가, M3 스텁류 실태 반영 — Observer/matchMedia/
postMessage/scrollTo/getComputedStyle/XHR은 코드에 이미 있었는데 문서만
미갱신이었음. + tkinter 폴백 폰트에 .size 부재 버그 수정 — line-height가
NativeFont에만 있던 속성을 읽어 폴백 경로에서 크래시).
항목을 완료하면 [x]로 바꾸고 날짜를 적을 것. cargo 148/148, smoke 108종
(엔진 파트; 실네트워크 관문은 egress 제한 환경에서 측정 불가) 기준.
검증 체인: cargo test → maturin build → pip 재설치 → smoke_test.py →
basket_test.py (네이버 단건은 scratchpad diag 스크립트).*

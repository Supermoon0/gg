# GG Browser — 처음부터 완성까지

기준일 2026-07-30. 자체 작성 코드 76,671줄 (ggcore/jsvm 34,928 ·
ggcore/html5 8,247 · browser 21,858 · validation 4,571).

[roadmap.md](roadmap.md)는 기능 단위의 상세 이력이다. 이 문서는 그
위에서 **전체 궤적 하나**를 본다: 어디서 시작했고, 지금 어디고, 무엇을
남기고 있으며, "완성"이 무슨 뜻인지.

원칙 하나만 지킨다. **모든 항목은 숫자로 닫는다.** "동작한다"는 완료가
아니다. 외부 채점표의 점수, 게이트의 exit code, 또는 재현 가능한 측정치가
있어야 완료다.

---

## 0. 지금 서 있는 자리

| 축 | 점수 | 게이트 |
|---|---|---|
| HTML 파서 | **1796/1796 (100%)** | html5lib-tests, cargo test |
| JS 엔진 | **27,379/102,655 (26.67%)** | Test262, baseline regression=0 |
| CSS (script) | **4,344/24,862 (17.47%)** | WPT css testharness |
| CSS (render) | **5,178/17,263 (29.99%)** | WPT css reftest, 픽셀 비교 |
| CSS (자체) | 23/23 | css_gauntlet |
| JS 계약 | 28/31 | jsvm_gauntlet (3건은 문서화된 GAP) |
| 내장 계약 | 9/9 | conformance |
| Rust 단위 | 281 | cargo test |

두 숫자의 격차가 이 프로젝트의 현재 모양이다. **파서는 끝났고 엔진은
4분의 1이다.** roadmap.md의 "P0 9/9, P1 8/8, P2 17/17 완료"는 *기능*
축의 이야기이고, 정합성 축은 별개로 26.67%다. 완성은 두 축이 다 닫혀야
한다.

---

## 1단계 — 기반 (완료)

- [x] Rust(ggcore) + Python(browser) 하이브리드 구조와 pyo3/maturin 빌드
- [x] 외부 JS 엔진(Boa) 제거, 자체 gg-js 단일 경로
- [x] 레지스터 바이트코드 VM, NaN 박싱, 실행 fuel
- [x] Windows/Linux CI, 실패 시 exit code를 내는 게이트 체계
- [x] 실사이트 바스켓 정기 실행

## 2단계 — 렌더링 파이프라인 (구조는 완료, 정합성은 17.47%)

- [x] HTML5 명세 파서 — 토크나이저, 23개 insertion mode, adoption
      agency, foreign content, fragment parsing, quirks
      → **1796/1796**, 엔진 DOM과 파스 트리가 바이트 동일함을 별도 검증
- [x] CSS 파싱·캐스케이드·상속, 미디어 쿼리, 변수, 의사 요소
- [x] 레이아웃 — 블록/인라인, flex, `position`, `overflow:auto` 스크롤러,
      transform, sticky
- [x] 페인트, 히트테스트, 폼 컨트롤 네이티브 렌더
- [x] iframe 문서 격리, 접근성 트리, transition/keyframes
- [x] 스타일 엔진 성능 — 시트 메모이제이션, ancestor bloom filter,
      매치 버퍼 재사용 → naver restyle 80.8ms → 11.3ms
- [x] paint-first 로딩 → naver 최초 페인트 666ms → 68.6ms

## 3단계 — 엔진 정합성 (진행 중, **26.67%**)

여기가 완성까지 남은 일의 대부분이다. 남은 실패를 실측 히스토그램
순서로 둔다. 괄호 안은 현재 실패 케이스 수.

### 3-A. 없는 언어 기능 — 각각이 독립적으로 큰 덩어리

- [ ] **BigInt** (3,195 + BigInt64Array 1,625 = 4,820)
      임의 정밀도 정수, 리터럴 `123n`, 연산자 오버로드, 타입드 배열 2종
- [ ] **Temporal** (9,706) — 날짜/시간 신규 API. 단일 최대 덩어리지만
      아직 어느 브라우저도 안정 출시하지 않았다. **우선순위 낮음**
- [ ] **Intl** (2,234) — 국제화. ICU 없이는 사실상 불가, 범위 재검토 필요
- [ ] **async iteration** (2,976) — `for await`, async generator,
      `Symbol.asyncIterator`
- [ ] **`yield*` 위임** (1,382)
- [ ] **eval** (1,537) — 간접 eval은 쉬움, 직접 eval은 스코프 접근 필요
- [ ] **modules** (843) — `import`/`export` 정적 링크
- [ ] **Iterator 헬퍼** (870)
- [ ] **SharedArrayBuffer / Atomics** — 스레드가 없으면 의미 제한적

### 3-B. 정확성 — 기능은 있으나 명세와 다름

- [ ] **negative syntax 4,654건** — 거부해야 할 코드를 받아들인다.
      class body 검증, dynamic import 위치, 정규식 문법, block-scope
      재선언. 파서가 관대한 만큼 하나하나 좁혀야 한다
- [ ] **TypeError를 던져야 하는데 안 던짐** (2,289)
- [ ] **`unexpected character`** (1,881) — private field `#x`의 남은 형태,
      ZWNJ/ZWJ 식별자
- [ ] **`for-in` 구조분해 타깃** (903)
- [ ] **구조분해 대입형 중첩 기본값** (~165) — `[[a] = [1]] = x`.
      바인딩형은 완료, 대입형은 desugar를 늦춰야 해서 미착수
- [ ] **접근자 열거 순서 interleaving** — 접근자가 데이터 프로퍼티 뒤로
      정렬된다. `{x, get a(){}, y}`가 `x,y,a` (정답 `x,a,y`).
      shape 슬롯을 접근자에도 줘야 한다
- [ ] **`$262` 호스트 객체** (905) — 러너 쪽. `createRealm`,
      `detachArrayBuffer`, `evalScript`

### 3-C. 견고성

- [x] 스크립트가 프로세스를 죽이는 할당 경로 차단 (72PB 요청 → RangeError)
- [x] 정규식 리터럴 재컴파일 제거 (**3,090배**), 정규식 아레나 무한 증가 차단
- [x] `exec`의 `lastIndex` — `while((m=re.exec(s)))` 무한 루프 해소
- [x] `for...in` 순서 결정론 (HashMap 시드 누출 2곳)
- [x] SVG/배경 이미지 캐시 (+362MB → +9MB / 40 라운드)
- [x] `el.style`/`el.dataset` 프록시 메모이제이션
- [ ] **`String/replace-math.js` 행(hang)** — abort가 아니라 멈춤.
      할당 가드와 다른 모양이라 별도 조사 필요
- [ ] **GC 없음** — `heap_bytes`가 `saturating_add`만 한다. 장시간
      실행 페이지의 근본 한계. task-scoped 세대별 GC 검토

### 3-D. 이미 닫은 것 (이번 아크)

- [x] 템플릿 `${}` 스캐너 (`unterminated string` 5,622 → 8)
- [x] 중첩 패턴 기본값 (`found Punct(Assign)` 5,780 → 461)
- [x] throw 이름 붙이기 (`[object Object]` 28,071 → 1,142)
- [x] 함수 `name`/`length` own property
- [x] 에러 서브클래스 `constructor` (`but got a Error` 3,386 → 456)
- [x] 식별자 유니코드 이스케이프 + 예약어 규칙 (위치별 3분기)
- [x] `JSON.stringify` 정수 키 손실
- [x] 엔진 내부 장부 비열거화 (Date/RegExp/arguments/Math)
- [x] `Object.create`를 `defineProperties`와 같은 경로로
- [x] `Date.parse` 형식 1개 → 4개 + 유효성 검사
- [x] **타입드 배열 9종 + DataView를 실제 바이트 위에** (66 → 867)

## 3.5단계 — CSS 정합성 (진행 중, **17.47%**)

점수판: `validation/wpt_css_conformance.py`. WPT의 testharness 기반
7,293 파일 / 24,862 서브테스트. 나머지 16,368개는 레퍼런스 렌더와
픽셀 비교가 필요한 reftest라 아직 채점 불가.

이 축은 2026-07-30에 처음 측정했다. 그전까지 "CSS가 된다"는 자체
작성 23개 케이스를 뜻했고, 완성도를 물었을 때 직접 만든 탐침이
100% → 44%로 두 번 다른 답을 냈다. 탐침은 자기가 재는 것만 잰다.

### 배관 (완료) — 여기까지가 CSS가 아니었다

- [x] **CSSOM** — `el.style`에 메서드가 없어 parsing 계열 1,000개가
      첫 줄에서 죽었다. getPropertyValue/setProperty/removeProperty/
      getPropertyPriority/item + `CSS` 네임스페이스. 3.93% → 20.55%
- [x] **`<body onload>`** — 1,122 파일이 이 속성으로 시작하는데
      무반응이었다. 서브테스트 10,870 → 18,437 (침묵은 0%가 아니라
      미측정)
- [x] **`getComputedStyle`** — 인라인 style 속성을 그대로 돌려주고
      있었다. 이제 캐스케이드 + 인라인 오버레이
- [x] **`split`의 캡처 그룹** — testharness가 모든 단언 메시지를
      캡처 정규식 split으로 만든다. 캡처를 버려서 11,201건이 전부
      `"expected "`에서 잘렸다. JS 엔진 버그를 CSS 점수판이 찾았다
- [x] **헤드리스 레이아웃** — 드라이버가 레이아웃을 아예 안 돌려서
      모든 요소가 0×0이었다. 기하 단언 9,097건 중 9,090건이 0과
      비교 중. 레이아웃 → rect 푸시 → 그 다음 스크립트 순서로 수정
- [x] **`document.fonts`** — `.ready` of undefined가 페이지 코드보다
      먼저 던져서 파일 전체가 침묵했다. 18,437 → 24,862

### 여기부터가 CSS 본체 (미착수)

- [ ] **레이아웃 정확도 8,857건** — got≠0인 실제 오차. grid alignment,
      flex, sizing
- [ ] **`got==0` 4,803건** — abspos의 writing-mode/auto-position 변형,
      `width: stretch` (548), `aspect-ratio` (348)
- [ ] **값 검증** — `e.style.x = '잘못된값'`이 설정된다. 거부하려면
      프로퍼티별 문법표가 필요하다
- [ ] **`initial` 해석** — 키워드를 문자열로 저장할 뿐 초기값으로
      풀지 않는다. 프로퍼티별 초기값표가 필요하다
- [ ] **정규 직렬화** — `red`가 `rgb(255, 0, 0)`으로 읽혀야 한다
- [x] **상속 프로퍼티 8개 → 20개** — `line-height`·`letter-spacing`·
      `text-transform`·`list-style-type` 등 12개가 상속되지 않았다.
      `body { line-height: 2 }`가 자식에게 안 갔다
- [x] **`text-decoration` 전파** — 상속이 아니라 in-flow 후손 전파라는
      별개 규칙. 텍스트 노드에서 읽고 있어서 `<a>`조차 밑줄이 없었다
- [ ] **미구현 프로퍼티** — 이번 아크에서 outline · list-style-type ·
      aspect-ratio · opacity · background-clip · linear-gradient ·
      `font` 단축 · `ch`/`ex`/`min()`/`max()`/`clamp()` ·
      `transform: scale`/`transform-origin` · `text-align-last` ·
      `white-space: break-spaces`가 닫혔다. 남은 것: order,
      writing-mode, column-count, filter, clip-path, 회전/스큐
      transform, CSS 카운터
      *주의: 이 목록은 두 번 틀렸다. 프로퍼티마다 조건을 지어 확인할 것 —
      배경 없는 div는 rect를 안 그리고 짧은 텍스트는 줄바꿈이 없다*
- [x] **reftest 픽셀 하네스** — `validation/wpt_css_reftest.py`.
      16,929 파일 / 17,263 비교. 저장된 이미지가 아니라 *같은 엔진의 두
      렌더*를 비교하므로 골든 파일도 폰트 일치도 필요 없다

#### 궤적: 28.25% → 27.47% → 29.99%

부풀린 숫자에서 정직한 숫자로 내려간 다음, 엔진 수정으로 올라갔다.

#### 하네스가 먼저 틀렸다 (28.25% → 27.47%)

첫 측정의 28.25%는 **부풀려진 숫자였다.** `fetch`가 `{}`를 돌려주고
있어서 외부 스타일시트도 이미지도 배경도 테스트와 레퍼런스 양쪽 모두에서
빠졌고, 아무것도 안 그린 두 페이지는 당연히 일치했다. 리소스를 실제로
읽게 하자 점수가 **내려갔다**. 엔진은 그대로인데 숫자가 내려간 것이
정직해진 결과다.

- [x] **로컬 리소스 로딩** — corpus 루트를 `/`로 보는 root-relative 해석,
      corpus 밖으로 나가는 경로 거부, 페이지마다 이미지 스토어 초기화
      (배경 캐시 키가 raw CSS url이라 디렉터리가 다르면 충돌)
- [x] **root-relative 레퍼런스** — `<link rel=match href="/css/...">`를
      테스트 디렉터리 기준으로 풀고 있어서 120건 이상이 렌더도 안 해보고
      "corpus 밖"으로 실패했다
- [x] **`<meta name=fuzzy>`** — 808개 파일이 "이 비교는 안티에일리어싱
      때문에 이만큼 어긋난다"고 파일 안에 적어두는데 하네스가 무시하고
      완전 일치를 요구했다. 저자가 이미 면제한 이유로 떨어뜨리고 있었다
- [x] **Ahem 폰트** — corpus의 3,444개 파일이 `font-family: Ahem`을
      쓴다. 모든 글리프가 em 정사각형이라 레이아웃을 픽셀 단위로 단언할
      수 있는 폰트다. 없으면 테스트와 레퍼런스가 둘 다 대체 폰트로
      떨어져서 대체 폰트를 재게 된다. 등록해도 css-text/white-space는
      38/120 그대로였고, **그게 유용한 결과다** — 그 실패들은 폰트가
      아니라 엔진이다

#### 그 다음 엔진 쪽 (이번 아크)

- [x] **외부 SVG 디코드** — 래스터 디코더가 `image` 크레이트라 SVG가
      닿으면 던졌고 요소가 아무것도 안 그렸다. 인라인 `<svg>`가 쓰던
      경로로 라우팅. 그 과정에서 퍼센트 도형 기하(`width="100%"`)가 0으로
      파싱되던 것과, viewBox(좌표계)를 intrinsic size로 착각하던 것
      두 가지가 드러났다. background-size 3/80 → 50/251
- [x] **`.xht` 레퍼런스의 CDATA** — `<![CDATA[ ... ]]>`로 감싼
      스타일시트가 통째로 버려지고 있었다. corpus의 2,437개 파일이 .xht
      레퍼런스를 가리킨다. css-flexbox 20/80 → 30/80
- [x] **오프셋 없는 `position:absolute`** — 아예 안 그려졌다. static
      position은 이미 계산돼 있었고 휴리스틱 하나가 건너뛰고 있었다
- [x] **linear-gradient 실제 렌더** — 첫 색 스톱 단색 근사에서 진짜
      그라디언트로. 각도 4단위, `to <corner>`(박스 비율에 따라 기울어짐),
      퍼센트/길이 스톱, 하드 스톱, rgba 알파. 0/83 → 65/83
- [x] **grid 트랙 사이징** — auto/min-content/max-content/minmax/
      fit-content가 전부 1fr이었다. `auto 1fr`이 50/50으로 나왔다.
      MDN 문서 높이 22,412px → 4,439px, 텍스트 조각 3,947 → 1,939
      (같은 5,747자 — 열이 좁아 단어가 글자 단위로 쪼개지고 있었다)
- [x] **`repeat(auto-fill, ...)`** — 패턴을 정확히 1번 반복해서 모든
      반응형 카드 그리드가 1열이 됐다
- [x] **text-align 7값 중 3값만 알았다** — start/end/match-parent/
      justify가 전부 left로 떨어졌다. `start`가 초기값이다. 17/121 → 63/121
- [x] **`text-align-last`** — 없었다. mismatch 실패 48건이 이거 하나
- [x] **`white-space: break-spaces`** + CSS Text 4 롱핸드
      (`white-space-collapse` / `text-wrap-mode`)
- [x] **`background-clip`** — 없었고, 배경 박스가 패딩 박스였다.
      초기값은 border-box(테두리 *아래*로 칠한다)
- [x] **`-webkit-line-clamp`의 트리거** — 이름만 써도 클램프했다.
      레거시 박스(`display:-webkit-box`) 안에서만 적용된다
- [x] **`transform: scale` + `transform-origin`** — translate만 읽고
      나머지를 버리고 있었다. 회전/스큐는 rect를 quad로 만들어서 이
      디스플레이 리스트에 명령이 없다 — 여전히 미구현
- [x] **`aspect-ratio`** — 파서만 있고 읽는 곳이 없었다. width가 있고
      height가 auto면 비율로 높이를 준다. min-height의 초기값 `auto`는
      비율 박스에서 "콘텐츠 기반 최소"라서, 비율이 콘텐츠를 자르려 하면
      콘텐츠가 이긴다 (17/80 → 29/80, 넓힌 샘플 44/120 → 50/120)
- [x] **`ch`·`ex`·`min()`·`max()`·`clamp()`** — 전부 None(=auto)이었다.
      `max-width: min(100%, 1200px)`가 아무것도 안 좁혔다. ch는 요소
      자신의 폰트에서 "0"을 재서 푼다 (Ahem에서 1em, 기본 폰트에서
      0.64em — 0.5em 근사는 2배까지 틀렸다)
- [x] **`outline`** — 없었다. 모든 키보드 포커스 링이 안 보였다
- [x] **리스트 마커** — `<ol>`이 점을 그렸다. decimal/alpha/roman/
      square/circle + `<ol start>`·`reversed`·`<li value>` + 모르는
      @counter-style 이름은 decimal로 폴백
- [x] **SVG 슈퍼샘플** — intrinsic 크기로 래스터화한 뒤 확대하면
      색 경계에 블렌딩 띠가 생긴다. 크게 굽고 샘플러가 내려오게
      (10/80 → 16/80)
- [x] **`font` 단축 속성** — `font: 25px/1 Ahem`이 통째로 무시되고
      있었다. css-text의 Ahem 테스트 전체가 16px 기본 폰트로
      렌더되고 있었다. white-space 5/60 → 27/60
- [x] **`opacity`** — `opacity: 0.05` 미만 컬링 한 곳에서만 읽히고
      그 위는 전부 불투명하게 그렸다. 래스터라이저가 opacity를 들고,
      디스플레이 리스트가 페이드된 서브트리를 push/pop(kind 11/12)로
      감싼다. 중첩은 CSS대로 곱해진다 (0.5 안의 0.5 = 0.25).
      *주의: 그룹 합성이 아니라 명령별 알파다. 페이드된 서브트리 안에서
      자식끼리 겹치면 아래가 비친다 — 진짜 그룹 opacity는 서브트리
      전용 오프스크린 버퍼가 필요하다*
- [ ] **break-spaces의 선행 브레이크** — 보존된 공백 시퀀스의 *첫 글자
      앞*에도 브레이크 기회가 있다. white-space 실패 73건
- [ ] **회전·스큐 transform** — 폴리곤 필과 회전 글리프 래스터가 필요
- [ ] **mismatch인데 동일하게 렌더** — 달라야 하는데 같다. 남은 큰 덩어리는
      font-palette, scrollbar-color, shaping, 회전 transform
- [ ] **fragmentation (css-break/*)** — 560건. 화면 브라우저에는 낮은 가치
- [ ] **css-ui 위젯 렌더 802건 / grid-lanes(Grid L3 masonry) 806건**
- [ ] **CSS 카운터** — `counter-increment`/`counters()`/`@counter-style`.
      `<ol reversed>`의 시작값이 counter-increment에 의존하는 케이스가
      여기 걸려 있다 (css-lists 7건)
- [ ] **CJK 줄바꿈 (UAX 14)** — css-text/i18n 158건이 전부 여기

## 4단계 — 웹 플랫폼 (부분)

있음: Proxy · Reflect · Symbol · Map/Set/WeakMap · Promise ·
FinalizationRegistry · structuredClone · fetch · Worker ·
MutationObserver · IntersectionObserver · ResizeObserver ·
customElements · localStorage · crypto · URL · TextEncoder ·
AbortController

- [ ] **WebAssembly** — 없음
- [ ] **indexedDB** — 없음
- [ ] **`<video>`/`<audio>`/WebGL** — 없음
- [ ] **HTTP/2 · 캐시 의미론** 검증
- [ ] **Service Worker**
- [ ] **레거시 인코딩** (EUC-KR 등) 과 인코딩 스니핑
- [ ] **텍스트 셰이핑** — 복합 문자, 양방향, 리거처
- [ ] 자체 정규식 엔진 (현재 `regex` + `fancy-regex` 크레이트)

## 5단계 — 제품화 (미착수)

- [ ] 탭, 다운로드, 북마크, 권한·인증서 오류 UI
- [ ] 브라우징 데이터 삭제, 프로필별 저장소 분리
- [ ] 페이지/네트워크 프로세스 격리와 리소스 제한
- [ ] 크래시 복구, 세션 복원
- [ ] shared-memory frame triple buffer와 chrome composite
- [ ] Windows 패키징, 릴리스 서명

---

## 완성의 정의

"브라우저가 돌아간다"는 완성이 아니다. 아래 다섯 개가 동시에 참일 때
1.0이라 부른다.

1. **HTML 파서 100%** — 이미 참 (1796/1796). 유지가 조건
2. **Test262 90% 이상** — Temporal/Intl 제외 기준. 현재 26.67%
3. **실사이트 바스켓 20개 이상**이 Chrome 렌더와 1:1 대조에서 합격
4. **모든 게이트 exit 0**, baseline regression 0이 CI에서 상시 유지
5. **장시간 실행 페이지가 메모리 상한 안에서 안정** — GC가 전제

가장 먼 것은 2번이다. 그리고 2번은 3-A의 큰 덩어리 몇 개(BigInt,
async iteration, eval, modules)와 3-B의 negative syntax 4,654건이
좌우한다. Temporal 9,706건은 제외 기준을 두는 편이 정직하다 — 어느
브라우저도 아직 안정 출시하지 않았다.

---

## 작업 순서 (다음에 무엇을)

가치/노력으로 정렬한다. 위에서부터.

1. **negative syntax 4,654** — 새 기능이 아니라 좁히기. 파서를 이미
   아는 사람이 제일 빨리 벤다
2. **async iteration 2,976** — 실제 페이지가 쓴다. `for await`는
   fetch 스트리밍의 기본형
3. **BigInt 4,820** — 자체 완결적이고 경계가 뚜렷하다
4. **eval 1,537** — 간접 eval만으로도 절반 이상
5. **modules 843** — 번들되지 않은 현대 페이지의 전제
6. **GC** — 점수는 안 오르지만 5번 완성 조건의 전제
7. Temporal / Intl — 범위 결정 먼저

---

## 이 프로젝트에서 반복 확인된 것

작업 방식에 관한 것이라 체크리스트에 남긴다.

**점수판이 먼저 옳아야 한다. 그리고 옳아지면 점수는 내려갈 수 있다.**
reftest 하네스는 첫 측정에서 리소스를 하나도 안 읽고 있었다. 테스트도
레퍼런스도 똑같이 빈 페이지로 렌더되니 통과였다. 실제로 읽게 만들자
28.25% → 27.47%. 엔진은 그 사이에 나아지기만 했다. **부풀린 숫자를
지키느니 내려가는 편이 낫다** — 내려간 숫자만이 다음 작업을 옳은 곳으로
보낸다. 이번 아크의 background-size 192건 "렌더링 버그"는 전부 하네스가
`support/*.svg`를 안 연 것이었다.

**게이트가 도는 동안 파일을 고치면 게이트가 죽는다.** 워커가 매 청크마다
소스를 다시 import하기 때문에, 편집 중간 상태를 읽은 청크가 NameError로
2,692건을 실패시켰다. 전체 실행이 무효가 됐다. 이후로는 `git worktree`로
커밋 시점 스냅샷을 떠서 거기서 게이트를 돌리고, 메인 트리는 계속 고친다.
(휠은 공유되므로 게이트 중 `pip install`은 여전히 금지다.)

**"동일하게 렌더된다"는 실패는 진단이다.** `rel=mismatch`인데 두 페이지가
똑같이 그려졌다는 건 그 프로퍼티가 아무 일도 안 한다는 뜻이다. 미구현
기능을 찾는 가장 싼 질의였다 — text-align-last 48건이 그렇게 나왔다.

**외부 채점표만이 잡는 버그가 있다.** 이번 아크에서 게이트가 다섯 번
regression을 잡았고, 그중 셋은 자신 있게 커밋한 변경이었다. 단위
테스트 280개가 잡은 것은 한 번도 없다. 특히 두 가지는 원리적으로 못
잡는다:

- **HashMap 시드가 `for...in` 순서로 새던 버그** — 프로세스마다 답이
  달라서, 통과하는 실행과 실패하는 실행이 섞인다
- **수정과 그 수정의 테스트가 함께 사라진 머지** — 없어진 테스트는
  실패를 보고하지 못한다. 스위트는 green을 유지한 채 동작만 뒤로 갔고,
  전체 코퍼스 게이트 하나만 알아챘다

그래서: **머지 후에는 `cargo test`가 아니라 게이트를 돌린다.**

**점수가 안 움직여도 옳은 수정이 있다.** 템플릿 스캐너 수정은
`unterminated string`을 5,622 → 8로 줄였는데 점수는 그대로였다 —
파싱이 뚫린 테스트들이 그 다음 없는 기능에 걸렸기 때문이다. 반대로
정규식 컴파일 캐시는 점수를 거의 못 올렸지만 **3,090배**였다. 점수는
하나의 축일 뿐이다.

**진단이 먼저다.** `[object Object]` 벽을 걷어낸 수정은 점수를 0.00
올렸지만, 그걸 걷어내기 전에는 코퍼스의 4분의 1이 아무 정보도 내지
않았다. 걷어내니 실패 목록이 읽혔고 거기서 3,386건짜리 원인이 보였다.

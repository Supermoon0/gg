# GG Browser

밑바닥부터 만든 하이브리드(Python + Rust) 웹 브라우저 엔진.

- **Rust (`ggcore`)**: HTML 파싱, CSS 파싱, 스타일 계산, **자체 JavaScript 엔진(gg-js)**,
  이미지 디코딩(PNG/JPEG/GIF/WebP), SVG 래스터라이즈, **텍스트 셰이핑·글꼴 래스터화·
  프레임 렌더링**(윈도우 TTF 직접 로드, 글리프 단위 한글 폴백, @font-face 웹폰트),
  네이티브 창 백엔드(winit + softbuffer), Rust 상주 디스플레이 리스트 스크롤
- **Python**: 네트워크, 레이아웃, 브라우저 로직, 라이브 틱 루프
- 러스트 모듈이 없으면 순수 파이썬 + tkinter 캔버스 경로로 자동 폴백

## 실행

```
python main.py                        # tkinter 셸, 홈 화면(about:home)
python main.py https://naver.com      # tkinter 셸
python main.py --native               # 러스트(winit) 창 셸
python smoke_test.py                  # 헤드리스 파이프라인 테스트 (249종)
python basket_test.py                 # 대표 사이트 렌더 측정 (9곳)
cd ggcore && cargo test --lib --no-default-features  # 198 통과 + 진단용 2종 ignored
```

native shell은 renderer child process를 기본 사용한다. 디버그/테스트에서 로컬
renderer를 강제하려면 `GG_PROCESS_MODEL=local`을 설정한다. renderer child는
시작 시 OS 샌드박스를 스스로 적용한다(POSIX setrlimit CPU/메모리/파일 +
no_new_privs, Windows Job object; `GG_RENDERER_CPU_S`/`GG_RENDERER_MEMORY_MB`
로 조정, `GG_RENDERER_SANDBOX=0`으로 해제). Headless API는 명시적으로
선택할 수 있다.

```python
from browser.driver import Page

page = Page(process_model="isolated")
page.goto("https://example.com")
```

## 자동 검증

`.github/workflows/ci.yml`은 모든 push와 pull request에서 Windows/Linux
Rust 테스트, 네이티브 wheel 빌드, Python smoke, network/CSS/JSVM gauntlet,
시각 렌더 증거 생성을 실행한다. PR 검증은 재현성을 위해 외부 인터넷 테스트를
건너뛰며, 실제 사이트 9곳은 `live site basket` workflow가 매주 화·금요일
오전 3시(KST)와 수동 실행 시 측정한다.

로컬에서 외부 사이트 접속을 제외하고 smoke를 실행하려면:

```powershell
$env:GG_SKIP_LIVE_NETWORK="1"
python smoke_test.py
```

## JavaScript — 자체 엔진 gg-js

레지스터 바이트코드 VM + NaN 박싱 + 인라인 캐시. 함수 본문은 **첫 호출까지
파싱·컴파일을 지연**한다(lazy parse/compile). 브라우저와 헤드리스 드라이버는
모두 gg-js를 유일한 JavaScript 런타임으로 사용한다.
[연구 노트](docs/jsvm-research.md) · [네이버 완벽 구동 체크리스트](docs/naver-perfect-checklist.md)

**언어**: 프로토타입 체인 실물화(`new`·`instanceof`·상속), 클래스(extends/super/
static/필드/접근자/`#private`), 제너레이터·이터레이터 프로토콜, async/await(전 위치),
구조 분해·spread/rest·옵셔널 체이닝·`??`, getter/setter·프로퍼티 디스크립터,
Map/Set/WeakMap/WeakSet, Symbol(문자열 페이크 + 폴리필 공존), 정규식(리터럴·
생성자·인스턴스 프로퍼티·추출 exec/test), **ToPrimitive/ToPropertyKey 시맨틱**
(valueOf/toString 실호출), 레이블 문, 비트 연산, `arguments`, bind/call/apply
uncurry 전 패턴, 배열·문자열·array-like의 메서드 추출(core-js `uncurryThis` 호환).
`Proxy`는 객체·함수 target, `get`/`set`/`has`/`deleteProperty`/`ownKeys`/
descriptor/prototype/확장성 trap, `apply`/`construct`, revocation을 VM 내부 연산에
연결한다. `Reflect` 13개 메서드는 같은 내부 연산을 공유해 trap의 기본 위임과
불변조건 검사를 보존한다.

**웹 플랫폼**: DOM 트리 조작 전반(생성·삽입·복제·형제 탐색·expando·attributes
컬렉션·앵커 URL 분해), 이벤트(add/removeEventListener·dispatchEvent·버블링·
attachEvent 레거시), location/navigator/history/performance, 타이머·rAF·마이크로태스크,
fetch + XMLHttpRequest, localStorage/sessionStorage, document.cookie, classList,
el.style/dataset 프록시, MutationObserver·IntersectionObserver·matchMedia 등
플랫폼 스텁 레이어, 라이프사이클(readyState·DOMContentLoaded·load).
parser-blocking/`async`/`defer`/module 스크립트의 fetch·실행·라이프사이클 순서를
분리하고, 동적 `<script>`의 기본 async, `async=false`, load/error 이벤트를 지원한다.
ES module은 상대 URL 해석, 병렬 fetch와 URL별 단일 평가 캐시, 정적 default/named/
namespace import, named/default/star re-export, `import.meta.url`, 순환 그래프와
namespace live export를 지원한다. `import()`은 리터럴과 런타임 계산식을 모두
지원하며 계산식은 실행 시점에 URL/import map을 해석하고 fetch한다. Promise 오류
전파, 동시 요청의 fetch·평가 캐시, top-level await의 의존성/DCL 대기,
exact·prefix·scoped import maps도 같은 상태 머신을 쓴다. default·named·namespace
import 읽기는 scope-aware getter로 연결되어 함수·구조분해·화살표 매개변수,
catch·블록·for 선언과 객체/클래스 메서드의 shadowing을 보존한다. 객체 축약 속성과
템플릿 표현식에서도 live 값을 읽으며 import 쓰기와 모듈 범위 재선언은 거절한다.
정적 `with { type: "json" }`과 동적
`import(url, { with: { type: "json" } })`은 JSON을 단일 default export 모듈로
검증·평가하며, 지원하지 않는 속성·타입과 잘못된 JSON은 모듈 오류 또는 Promise
`TypeError`로 정산한다. 문서별 런타임 그래프는 약한 참조로 수명 관리된다.

**실전 검증**: 네이버 번들 파이프라인에서 polyfill(core-js)·preload(jQuery)·
웹팩 런타임과 react-dom 18 `createRoot`가 끝까지 실행된다. 빈 `#root`에서 실제
React 컴포넌트 트리를 렌더·커밋하고, 뉴스·관심사 피드·로그인 패널·푸터까지
네이버 자체 DOM으로 그린다. 번들 7종의 언캐치드 예외는 0이다.

## 네트워크

소켓 위에 직접 구현한 HTTP/1.1: 호스트별 커넥션 풀(keep-alive), 메모리·디스크
개인 캐시(`no-store`/`private`/`no-cache`, `Vary`, ETag/Last-Modified 조건부
재검증과 304 병합), GET/POST body, 301/302/303/307/308 리다이렉트 규칙,
chunked, gzip, 취소 토큰과 전체 요청 timeout, `file:`/`about:`/`data:` 스킴.
쿠키는 Domain/Path/Secure/HttpOnly/SameSite/Expires/Max-Age와 보안 접두사를
적용하며, JS 쓰기는 속성을 보존한 채 네트워크 jar로 동기화된다. 스타일시트는
스크립트 실행과 병렬로 프리페치된다. fetch/XHR에는 origin 비교, CORS 응답 검증,
preflight, credentials 모드, mixed-content 차단을 적용한다. 공식 Public Suffix
List의 exact/wildcard/exception/PRIVATE 규칙과 IDNA 정규화로 Domain supercookie를
막고, SameSite는 스킴과 eTLD+1을 함께 비교한다.

PSL 스냅샷은 `browser/data/public_suffix_list.dat`에 포함된다. 공식 목록으로
갱신하려면 `python scripts/update_psl.py`를 실행한다.

## 렌더링 파이프라인

```
URL ──► net.py ──► html_parser.py ──► style.py ──► layout.py ──► draw.py ──► 화면
        네트워크      DOM 트리        스타일 계산     박스 트리    디스플레이 리스트
                          ▲
                     css_parser.py (UA + <style> + <link>)
```

파싱·스타일은 Python/Rust 양 엔진 미러(노드 단위 동일 결과 검증), 레이아웃은
Python, 페인트는 Rust 소프트웨어 래스터라이저. 골든 렌더링 회귀 스위트
(픽스처 7종, diff 가능한 디스플레이 리스트) 포함.

**CSS**: 셀렉터(복합·자손·속성 7종·`:root`/`:where`/`:not`/`:nth-child`/
`:first`/`:last-child`·`:hover`/`:focus` 동적 재스타일), 커스텀 프로퍼티(`var()`
체인·fallback·순환 방지), `@media` (min/max-width), `@font-face` 웹폰트,
`::before`/`::after` + content, background-image(스프라이트 크롭·타일),
border-radius, box-shadow, gradient 색 폴백, overflow:hidden 클리핑, z-index
쌓임(컨테이너별 정렬), transform(translate/scale), opacity 게이트, line-height,
white-space:nowrap, text-overflow:ellipsis, **transition + @keyframes 애니메이션**
(shorthand/longhand 정규화, 색·길이·transform 보간, iteration/direction/
fill-mode/play-state, paint-only와 layout 무효화 분리 — 두 셸이 같은
`AnimationEngine`을 frame tick에서 샘플링)

**iframe**: 프레임마다 완전히 분리된 자식 문서(자체 URL/base·origin·쿠키
jar·storage·gg-js 이벤트 루프·스타일·애니메이션). 부모는 자식의 페인트
출력만 임베드하고(300x150 대체 요소 기본, clip, 프레임 내부 휠 스크롤,
중첩 hit-test·프레임 내 링크 탐색), load/error를 iframe 요소에 발화한다.
X-Frame-Options·CSP frame-ancestors·sandbox(allow-scripts/allow-same-origin)·
srcdoc, 중첩 depth 3 상한. 헤드리스는 `page.frame("#id")`로 자식 문서를
조회한다.

**레이아웃**: 블록/인라인, 박스 모델, position(absolute/fixed/relative/**sticky**),
플렉스박스(justify/align/shrink/basis 포함), **float + clear**, table의
rowspan/colspan, grid 고정/fr track·named area·named line·양축 span과 충돌 없는
자동 배치, 대체 요소 CSS 사이징, 이미지·SVG 인라인 배치. sticky subtree는
상주 display list의 push/pop 경계로 보존해 Rust가 스크롤마다 위치를 계산하며
hit-test도 같은 containing-block 제한 offset을 사용한다. Vertical margin은
인접 형제뿐 아니라 부모–첫/마지막 자식, 중첩·빈 블록 체인과 양수/음수 혼합까지
collapse하며 border·padding·overflow formatting context에서는 차단한다.

**동적**: 라이브 틱 루프(타이머/rAF 발화 → DOM 변이 감지 → 부분 무효화 v1 →
재렌더), Tab/Shift+Tab 순차 포커스와 Enter/Space 기본 동작, 텍스트 입력·캐럿,
링크·버튼·체크박스·라디오 키보드 활성화, GET/POST 폼 제출
(URL-encoded·text/plain·multipart 파일), 외부 `form=` owner, submit/reset/invalid
이벤트와 required/type/pattern/min/max 길이·수치 검증, 실측 getBoundingClientRect

**폼 컨트롤**: 엔진이 위젯 페이스를 직접 그린다 — 체크박스(체크마크),
라디오(점), `<select>`(선택된 option 라벨 + 셰브론), textarea(현재 값,
클리핑), 버튼 페이스, 포커스 링. 페이지가 직접 스타일을 준 컨트롤
(`appearance:none` 관용구)은 자기 모양을 유지하고 상태 표시만 덧그린다.
팝업 레이어가 없으므로 select는 클릭/Enter/Space/방향키로 선택을 순환하며
input·change를 발화한다.

**스크롤 영역**: 명시 크기의 `overflow:auto|scroll` 박스는 자체 스크롤러다 —
자식 클리핑, 페인트 시점 오프셋, 박스 내부 스크롤바, sticky와 합성되는
히트테스트, 그리고 안쪽 스크롤러가 끝에 닿으면 바깥·페이지로 넘어가는
scroll chaining. 페이지 JS도 같은 스크롤러를 읽고 움직인다 —
`scrollTop`/`scrollLeft`/`scrollHeight`/`scrollWidth`,
`el.scrollTo`/`scrollBy`/`scrollIntoView()`,
`window.scrollTo`/`scrollBy`와 라이브 `window.scrollY`. 엔진이 스크롤을
소유하므로 요청은 큐에 쌓였다가 호스트가 **페이지가 호출한 순서대로**
재생하고, 상대 스크롤(`scrollBy`)은 위치가 아니라 델타로 전달되어 같은
턴의 `scrollIntoView()` 결과 위에 정확히 얹힌다.

**크롬**: 주소창, 히스토리, 세로/가로 스크롤(네이티브 셸은 Rust 상주 디스플레이
리스트로 오프셋 전용 프레임), 링크 히트 테스트, EAGER-DATA 리더 모드(네이버
헤드라인·피드 추출 렌더). 상위 탐색은 UI 스레드 밖에서 실행되며 새 탐색·중지로
기존 요청을 취소한다. 뒤로/앞으로는 저장한 문서와 폼·스크롤 상태를 복원한다.

**접근성**: 역할/이름/상태의 계층 접근성 트리(`browser/accessibility.py`) —
WAI-ARIA accessible-name 서브셋(aria-label/labelledby, label[for]·감싸는
label, alt, caption/legend, placeholder, title), checked/selected/expanded/
disabled/value 등 상태, landmark·heading 아웃라인, aria-hidden 서브트리
프루닝. Rust `snapshot()`은 같은 역할 맵·이름 우선순위의 flat 고속 경로이고,
헤드리스는 `page.ax_tree()`로 전체 트리를 읽는다.

## 성능

위키백과 「Web browser engine」 기준: **엔진 전체(네트워크 제외, 첫 로드)
504 ms → 약 50 ms** (파싱+스타일 26ms · 레이아웃 11ms · 프레임 4-6ms).
재방문은 디스크 캐시로 네이버 1.19s → 0.18s. 프로파일 하니스:
`cargo test --release -- --ignored profile_phases --nocapture`

바스켓(9곳: 네이버·위키백과·나무위키·연합뉴스·티스토리·HN·MDN·정부24·example)
8/9 "읽을만함", 크래시 0.

호환성 추세는 [`validation/conformance.py`](validation/conformance.py)가
동일한 JSON 형식으로 기록한다. PR CI에서는 gg-js/DOM 내장 계약 probe 9개를
회귀 gate로 실행하고, 주기 workflow에서는 고정 SHA의 Test262·WPT 정적 하위
집합을 별도 점수로 측정한다. 내장 probe 통과 수를 공식 suite 통과율로 해석하지
않는다. 실행 방식과 어댑터 경계는 [conformance scorecard](docs/conformance.md)에
정리되어 있다. 최초 고정 기준선은 내장 9/9, Test262 37/120, 정적 WPT 0/2다.

## 아직 없는 것 (다음 단계 후보)

상세 우선순위와 완료 조건은 [개발 로드맵](docs/roadmap.md)에 체크리스트로 관리한다.

- baseline JIT, GC, 레이아웃 러스트 이식 (React 실측 병목을 기준으로 결정)
- HTTP/2, `<video>`/WebGL, 로그인 호환성, transition/animation DOM 이벤트
- iframe same-origin 스크립팅(contentWindow/postMessage), 프레임 히스토리
- [browser/renderer/network 프로세스 격리와 IPC](docs/process-isolation-ipc.md)
- process-neutral local seam: `browser/renderer_session.py`, `browser/network_backend.py`

## 네이티브 코어 빌드

```
pip install maturin
cd ggcore
maturin build --release -o dist
pip install --force-reinstall dist\ggcore-*.whl
```

검증 체인: `cargo test --lib --no-default-features` → 휠 빌드·설치 →
`python smoke_test.py` → `python validation/net_gauntlet.py` →
`python validation/jsvm_gauntlet.py` → `python validation/conformance.py` →
`python validation/css_gauntlet.py` → `python validation/render_evidence.py` →
`python basket_test.py`

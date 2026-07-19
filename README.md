# GG Browser

밑바닥부터 만든 하이브리드(Python + Rust) 웹 브라우저 엔진.
목표는 **전체 웹** — 티어별 판정 기준은 [로드맵](docs/roadmap.md) 참조
(T1 문서 웹 ✅ · T2 앱 웹 진행 중 · T3 세션 · T4 미디어 · T5 플랫폼).

- **Rust (`ggcore`)**: HTML 파싱, CSS 파싱, 스타일 계산, **자체 JavaScript 엔진(gg-js)**,
  이미지 디코딩(PNG/JPEG/GIF/WebP), SVG 래스터라이즈, **텍스트 셰이핑·글꼴 래스터화·
  프레임 렌더링**(윈도우 TTF 직접 로드, 글리프 단위 한글 폴백, @font-face 웹폰트 — woff2 포함),
  네이티브 창 백엔드(winit + softbuffer), Rust 상주 디스플레이 리스트 스크롤
- **Python**: 네트워크, 레이아웃, 브라우저 로직, 라이브 틱 루프
- 러스트 모듈이 없으면 순수 파이썬 + tkinter 캔버스 경로로 자동 폴백

## 실행

```
python main.py                        # tkinter 셸, 홈 화면(about:home)
python main.py https://naver.com      # tkinter 셸
python main.py --native               # 러스트(winit) 창 셸
python smoke_test.py                  # 헤드리스 파이프라인 테스트 (178종)
GG_SKIP_NET=1 xvfb-run python smoke_test.py   # egress 제한/헤드리스 CI
python basket_test.py                 # 대표 사이트 렌더 측정 (9곳)
cd ggcore && cargo test --lib         # 엔진 단위 테스트 (176종)
```

## JavaScript — 자체 엔진 gg-js

레지스터 바이트코드 VM + NaN 박싱 + 인라인 캐시. 함수 본문은 **첫 호출까지
파싱·컴파일을 지연**한다(lazy parse/compile). async 지원 빌드에서는 브라우저가
자동으로 gg-js 경로를 사용한다(`GGJS=1`, Boa는 폴백).
[연구 노트](docs/jsvm-research.md) · [네이버 완벽 구동 체크리스트](docs/naver-perfect-checklist.md) · [완벽 구동 설계](docs/naver-perfect-design.md)

**언어**: 프로토타입 체인 실물화(`new`·`instanceof`·상속), 클래스(extends/super/
static/필드/접근자/`#private`), 제너레이터·이터레이터 프로토콜, async/await(전 위치),
구조 분해·spread/rest·옵셔널 체이닝·`??`, getter/setter·프로퍼티 디스크립터,
Map/Set/WeakMap/WeakSet, Symbol(문자열 페이크 + 폴리필 공존), 정규식(리터럴·
생성자·인스턴스 프로퍼티·추출 exec/test), **ToPrimitive/ToPropertyKey 시맨틱**
(valueOf/toString 실호출), 레이블 문, 비트 연산, `arguments`, bind/call/apply
uncurry 전 패턴, 배열·문자열·array-like의 메서드 추출(core-js `uncurryThis` 호환).

**웹 플랫폼**: DOM 트리 조작 전반(생성·삽입·복제·형제 탐색·expando·attributes
컬렉션·앵커 URL 분해), 이벤트(add/removeEventListener·dispatchEvent·버블링·
attachEvent 레거시), location/navigator/history/performance, 타이머·rAF·마이크로태스크,
fetch + XMLHttpRequest, localStorage/sessionStorage, document.cookie, classList,
el.style/dataset 프록시, MutationObserver·IntersectionObserver·matchMedia 등
플랫폼 스텁 레이어, 라이프사이클(readyState·DOMContentLoaded·load).

**실전 검증**: 네이버 번들 파이프라인에서 **웹팩 런타임 구동 성공** —
polyfill(core-js)·preload(jQuery) 번들이 끝까지 실행되고 앱이 리스너·타이머를
등록하며 동적 스크립트를 주입하는 단계까지 도달. **React 18 UMD 실번들
(production.min)은 부팅 완료**: u16 레지스터 파일이 마지막 컴파일 관문
("expression too deep")을 제거 — ReactDOM.render/createRoot 마운트,
useState/useEffect, 클릭→setState→리렌더까지 E2E 검증. **ES 모듈 v1**:
`<script type=module>`을 정적 링커가 실행 전에 클래식 스크립트로 링크.
**동적 주입 스크립트 채널**(주입 → 페치 → 실행 → onload 체이닝)과
**합성 네이버 픽스처**(EAGER-DATA + 3단 주입 + React 피드)가 부팅 체인을
헤드리스로 상시 검증한다. 네이버 원본 번들 재검증은 로컬 네트워크 필요.

## 네트워크

소켓 위에 직접 구현한 HTTP/1.1: 호스트별 커넥션 풀(keep-alive), 메모리 캐시,
디스크 캐시(명시적 max-age), 리다이렉트, chunked, gzip, `file:`/`about:`/`data:` 스킴,
**세션 쿠키 자**(Set-Cookie 수집 → 동일 호스트 재전송, document.cookie 양방향
동기화), **문자셋 스니핑**(Content-Type + `<meta charset>` — EUC-KR 레거시 사이트).
스타일시트는 스크립트 실행과 병렬로 프리페치된다.

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
white-space:nowrap, text-overflow:ellipsis

**레이아웃**: 블록/인라인, 박스 모델, position(absolute/fixed/relative),
플렉스박스(justify/align/shrink/basis 포함), **float + clear**,
**테이블 v1**(균등 컬럼·colspan), **grid v1**(px/fr/auto/repeat + gap),
**inline-block**(원자 라인 박스), **형제 margin collapsing**,
overflow:auto 클립, 대체 요소 CSS 사이징, 이미지·SVG 인라인 배치

**동적**: 라이브 틱 루프(타이머/rAF 발화 → DOM 변이 감지 → 부분 무효화 v1 →
재렌더), 텍스트 입력 포커스·타이핑·캐럿, GET 폼 제출, 실측
getBoundingClientRect(뷰포트 상대 — 스크롤 반영)

**크롬**: 주소창, 히스토리, 세로/가로 스크롤(네이티브 셸은 Rust 상주 디스플레이
리스트로 오프셋 전용 프레임), 링크 히트 테스트, EAGER-DATA 리더 모드(네이버
헤드라인·피드 추출 렌더)

## 성능

위키백과 「Web browser engine」 기준: **엔진 전체(네트워크 제외, 첫 로드)
504 ms → 약 50 ms** (파싱+스타일 26ms · 레이아웃 11ms · 프레임 4-6ms).
재방문은 디스크 캐시로 네이버 1.19s → 0.18s. 프로파일 하니스:
`cargo test --release -- --ignored profile_phases --nocapture`

바스켓(9곳: 네이버·위키백과·나무위키·연합뉴스·티스토리·HN·MDN·정부24·example)
8/9 "읽을만함", 크래시 0.

## 아직 없는 것 (다음 단계 후보)

전체 우선순위별 목록: [통합 TODO 체크리스트](docs/todo.md)

- 네이버 원본 main 번들 로컬 재검증 (React 자체는 부팅 확인 — 07-19)
- `position: sticky` 스크롤 핀, 테이블 자동 컬럼 폭, grid span 배치
- Proxy/Reflect(의도적 보류 — 반쪽 스텁은 폴리필 오판 유발), dynamic import()
- baseline JIT, GC, 레이아웃 러스트 이식 (부팅 후 측정하며 결정)
- transition/@keyframes, iframe, HTTP/2, `<video>`/WebGL, 로그인(쿠키 보안 속성 완전판)

## 네이티브 코어 빌드

```
pip install maturin
cd ggcore
maturin build --release -o dist
pip install --force-reinstall dist\ggcore-*.whl
```

검증 체인: `cargo test --lib` → 휠 빌드·설치 → `python smoke_test.py` →
`python basket_test.py`

# GG Browser

밑바닥부터 만든 하이브리드(Python + Rust) 웹 브라우저 엔진.

- **Rust (`ggcore`)**: HTML 파싱, CSS 파싱, 스타일 계산, **JavaScript 실행(Boa 엔진 + DOM 바인딩)**,
  이미지 디코딩(PNG/JPEG/GIF/WebP), **텍스트 셰이핑·글꼴 래스터화·프레임 렌더링**
  (윈도우 TTF를 직접 로드하고 글리프 단위 한글 폴백 — Segoe UI → 맑은 고딕 → Segoe UI Symbol),
  네이티브 창 백엔드(winit + softbuffer)
- **Python**: 네트워크, 레이아웃, 브라우저 로직
- 러스트 모듈이 없으면 순수 파이썬 + tkinter 캔버스 경로로 자동 폴백 (JS/이미지 제외 동일 동작)

## 실행

```
python main.py                        # tkinter 셸, 홈 화면(about:home)
python main.py https://example.com    # tkinter 셸
python main.py --native               # 러스트(winit) 창 셸 — 탈 tkinter
python smoke_test.py                  # 헤드리스 파이프라인 테스트
```

데모 페이지: `about:demo` (렌더링·이미지), `about:js` (JavaScript + onclick 이벤트)

## JavaScript

`<script>`(인라인·외부)를 Boa 엔진으로 실행한다. 지원 DOM API:
`document.getElementById / querySelector / querySelectorAll / getElementsByTagName /
createElement / title / body`, 요소의 `textContent / innerHTML / getAttribute /
setAttribute / removeAttribute / appendChild / remove / addEventListener / id / className`,
`console.log`, `alert`. 클릭 이벤트는 조상으로 **버블링**하며 onclick 속성과
addEventListener 핸들러를 모두 실행한 뒤 재스타일 → 재레이아웃 → 재렌더한다.
JS 컨텍스트는 페이지 단위로 유지되어 이벤트 핸들러가 페이지 스크립트의 상태를 공유한다.
querySelector는 엔진의 러스트 CSS 셀렉터 매처를 그대로 재사용한다.

`GGJS=1`이면 Boa 대신 **자체 제작 JS 엔진(gg-js)** 으로 실행한다
(레지스터 바이트코드 VM + NaN 박싱 + 인라인 캐시, DOM 노드를 VM 네이티브 값으로
취급 — [연구 노트](docs/jsvm-research.md)). `let`/`const` 블록 스코프·TDZ·
per-iteration 클로저 캡처, `try`/`catch`/`finally`/`throw`(엔진 에러도
`{name, message}` 형태로 catch 가능), `a[i]()` computed 멤버 호출(this 유지),
정규식 실행(리터럴 + `.test`/`.exec`, String `match`/`replace`/`split`/`search`),
`in`(own-property 검사)·`instanceof`(내장 생성자는 identity 매칭, 사용자 함수는
false — 프로토타입 체인이 없는 엔진의 정의된 근사)까지 지원한다.

## 네트워크

소켓 위에 직접 구현한 HTTP/1.1: **호스트별 커넥션 풀(keep-alive)** — 같은 호스트
재요청 시 TCP+TLS 핸드셰이크 생략(위키백과 재로드 359ms → 87ms), **메모리 캐시** —
`Cache-Control: max-age` 존중, no-store/no-cache 제외 (example.com 재방문 0.01ms),
**디스크 캐시** — 명시적 max-age 응답만 저장(재시작 생존; 네이버 JS/CSS 2.3MB
재방문 620ms → 50ms), 리다이렉트, chunked, gzip,
`file:` / `about:` / `data:` 스킴. 스타일시트는 스크립트 실행과 **병렬로
프리페치**된다(파이프라인 오버랩).

## 네이티브 코어 빌드 (선택)

```
pip install maturin
cd ggcore
maturin build --release -o dist
pip install dist\ggcore-*.whl
```

빌드 후 브라우저 상태 표시줄에 "완료 (rust)"가 표시되면 네이티브 경로가 활성화된 것.

## 렌더링 파이프라인

실제 브라우저 엔진(Blink/WebKit)과 같은 단계로 구성되어 있습니다.

```
URL ──► net.py ──► html_parser.py ──► style.py ──► layout.py ──► draw.py ──► 화면
        네트워크      DOM 트리        스타일 계산     박스 트리     디스플레이
                          ▲                                        리스트
                     css_parser.py
                     (UA 스타일시트 + <style> + <link rel=stylesheet>)
```

| 모듈 | 역할 |
|---|---|
| [net.py](browser/net.py) | HTTP/1.1, HTTPS(TLS), 리다이렉트, chunked, gzip, `file:`/`about:` |
| [html_parser.py](browser/html_parser.py) | 토크나이저 + 트리 빌더 — 주석, 엔티티, 암묵적 태그, 자동 닫힘 |
| [css_parser.py](browser/css_parser.py) | 태그/클래스/ID/복합/자손 셀렉터, 명시도, 에러 복구 |
| [style.py](browser/style.py) | 캐스케이드(UA → 페이지 → style 속성), 상속, em/% 해석 |
| [layout.py](browser/layout.py) | `Document → Block → Line → Text` 박스 트리, 줄바꿈, 정렬 |
| [draw.py](browser/draw.py) | 디스플레이 리스트(페인트 명령) |
| [browser.py](browser/browser.py) | tkinter 셸 — 주소창, 히스토리, 스크롤, 링크 히트 테스트 |
| [shell.py](browser/shell.py) | 네이티브(winit) 셸 — 크롬까지 자체 래스터라이저로 그림 |
| [native.py](browser/native.py) | 러스트 코어 연결부 (파싱+JS+스타일 → 파이썬 트리) |
| [textengine.py](browser/textengine.py) | 러스트 텍스트 엔진/래스터라이저 연결부 |
| [pages.py](browser/pages.py) | `about:home`, `about:demo`, `about:js`, 에러 페이지 |
| [ggcore/src](ggcore/src) | 러스트 코어: html/css/style/js/fonts/raster/window |

## 지원 기능

- HTML: 엔티티(`&amp;`), 주석, `<script>`/`<style>` 원시 텍스트, `<p>`/`<li>` 자동 닫힘,
  생략된 `<html>`/`<head>`/`<body>` 삽입
- CSS: 셀렉터 명시도 캐스케이드, 상속, `margin`/`padding`/`border` 축약형,
  `background-color`, `text-align`, `text-decoration`, `white-space: pre`,
  글꼴 크기/굵기/기울임, px/em/rem/% 단위,
  **커스텀 프로퍼티**(`--x: ...` 상속 + `var(--x, fallback)` 치환 — 체인·순환 방지,
  해석 실패 시 상속값/초기값 폴백), `:root`·`:where(...)` 셀렉터
  (미지원 대안은 룰이 아니라 그 대안만 버림 — 네이버처럼 `:where(:root,:host)`에
  변수 2천 개를 정의하는 사이트가 동작),
  **속성 셀렉터** `[attr]`/`=`/`~=`/`^=`/`$=`/`*=`/`|=`,
  **`background-image`**(url 로드 + position/size(cover·contain·px·%)/repeat —
  스프라이트 시트의 음수 오프셋 크롭·타일링을 러스트에서 렌더),
  **`border-radius`**(균일 반경 + `50%` — 라운드 배경/테두리 링, AA),
  `opacity`(≈0이면 서브트리 페인트 스킵), input `placeholder` 표시
- 레이아웃: 블록/인라인 배치, 단어 단위 줄바꿈, 베이스라인 정렬, 목록 불릿, `<hr>`,
  **박스 모델**(`width`/`max-width`/`min-width`/`height`, `margin: auto` 중앙 정렬,
  패딩·테두리 렌더링), **`position: absolute/fixed/relative`**(out-of-flow 배치),
  **플렉스박스**(`display: flex`, 행 배치, `flex-wrap`, 남은 공간 분배), 이미지 인라인 배치
- 크롬: 주소창(Ctrl+L), 뒤로/앞으로(Alt+←/→), 새로고침(Ctrl+R), 휠/키보드 스크롤,
  링크 클릭·호버 상태 표시줄, 방문 기록,
  **가로 스크롤**(네이티브 셸 — 콘텐츠가 창보다 넓으면 휠 dx·←/→ 키, 하단 바)
- SVG: 인라인 `<svg>`의 `<path>`를 러스트에서 래스터라이즈(전체 path 커맨드 +
  타원 호, nonzero 채우기, 4×4 슈퍼샘플링 AA) 후 이미지처럼 배치 —
  fill은 계산 스타일 → 프레젠테이션 속성 순으로 해석 (네이버 검색 아이콘)

## 성능

위키백과 「Web browser engine」 문서 기준 (HTML 130KB + CSS 216KB, DOM 1,661노드, CSS 규칙 539개):

| 단계 | 순수 Python | Rust 하이브리드 | 기법 |
|---|---|---|---|
| HTML+CSS 파싱+스타일 계산 | 130 ms | **26 ms** | 러스트 이식 + 셀렉터 해시 버킷 인덱스 |
| 레이아웃 (첫 로드) | 242 ms | **11 ms** | 러스트 텍스트 측정 (Tcl 왕복 제거) + 글리프 캐시 |
| 레이아웃 (웜) | 6 ms | **5.5 ms** | 단어 폭 메모이제이션 |
| 프레임 렌더링 | 캔버스 위임 | **4-6 ms** (+표시 12 ms) | 러스트 소프트웨어 래스터라이저, AA 글리프 블렌딩 |
| 스타일시트 다운로드 | 337 ms | 동일 | 6-스레드 병렬 |

**엔진 전체 (네트워크 제외, 첫 로드): 504 ms → 약 50 ms.**
파이썬/러스트 두 경로는 같은 페이지에서 노드 단위로 동일한 DOM·계산 스타일을 생성한다
(위키백과 1,661노드 비교 검증 통과).

## 아직 없는 것 (다음 단계 후보)

- 테이블 레이아웃, 플로트(`float`), `flex-shrink`/`flex-basis`/열 정렬 등 본격 플렉스박스,
  마진 상쇄(margin collapsing)
- JS(gg-js): 구조 분해, 클래스, `async`/`await`, 정규식 실행, getter/setter,
  computed 객체 키, `?.`/`??`, 라벨 문
- 쿠키, 디스크 캐시, HTTP/2

(이미지 `<img>`, 병렬 리소스 로딩, keep-alive 커넥션 풀, 메모리 캐시, JavaScript 실행,
플렉스박스 기본형·`position`은 **이미 구현되어 있다** — 위 "지원 기능"·"성능" 절 참고.)

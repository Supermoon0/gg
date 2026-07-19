# 실증 보고서 — 2026-07-19

리눅스 CI 컨테이너(외부 사이트 egress 차단)에서 README·체크리스트의 주장을
소스에서부터 빌드해 전수 실측한 결과. 검증 하니스는 `validation/`에 상설화.

## 요약 스코어보드

| 검증 | 결과 | 비고 |
|---|---|---|
| `cargo test --lib` | **171/171 통과** (ignored 2 = 진단·프로파일) | README 주장과 정확히 일치 |
| 릴리스 휠 빌드 | 성공 (cp311 + cp312, maturin 1.14) | 5m11s / 증분 2m10s |
| `smoke_test.py` | **엔진 파트 141/141 통과** → 실네트워크 2건 env-skip 시 **147 PASS + 2 SKIP = 149종 전수** | 드라이버 5종·about:home 포함, [FAIL] 0 |
| `basket_test.py` | 측정 불가 (아래 네트워크 게이트) | 하니스 자체는 9행 전부 무크래시 완주 |
| 프로파일 하니스 | lazy 컴파일 주장 **정밀 재현** | protos 7501→1501 **정확 일치**, 프런트엔드 63.4→28.8ms |
| CSS/레이아웃 독립 건틀릿 (신규 23종) | **22/23** | 실좌표 검증. 낙방 1건 = line-height 버그(하단) |
| gg-js 언어 건틀릿 (신규 30종, node v22 대조) | **CLAIM 20/20 전원 일치** | 조용한 오답 0건, 편차는 전부 문서화된 것 |
| HTTP/1.1 스택 로컬 실증 (신규 14종) | **14/14** | keep-alive·gzip·chunked·리다이렉트·디스크 캐시를 서버측 카운터로 증명 |
| 네이티브 래스터 시각 증거 | PNG 4장 생성 | about:home·라이브 데모·CSS 쇼케이스·JS 전용 페이지 |

## 환경과 네트워크 게이트

- Python 3.12(tkinter, xvfb) + Rust 1.94. 검증 체인: cargo test → maturin
  build → pip 재설치 → smoke → 건틀릿.
- **egress의 실체 규명**: 소켓 연결 자체는 성공하지만(naver 443 접속 0.01s)
  HTTP 허용목록 인터셉터가 모든 요청에 `x-deny-reason: host_not_allowed`
  텍스트 100바이트를 응답한다. 예외가 아니라 정상 응답이라 basket은
  9곳 전부 그 안내문(16단어)을 충실히 렌더하고 "빈약" 판정 — 실사이트
  점수(8/9)는 이 환경에서 원리적으로 측정 불가. smoke의 실네트워크 2건
  (example.com 페치, google 리다이렉트)도 같은 게이트로 죽는 것 확인
  (그 앞까지 [FAIL] 0).

## 프로파일 실측 (release)

605KB 합성 웹팩 번들, ignored 테스트 2종(polyfill_diag·profile_phases) 모두 통과:

| 항목 | 주장 (07-18) | 실측 (07-19 컨테이너) |
|---|---|---|
| lazy 프런트엔드 | ~30ms | **28.8 / 25.6ms** (2회) |
| eager 프런트엔드 | 108ms | 63.4ms (더 빠른 하드웨어, 비율 2.2×로 방향 일치) |
| protos lazy/eager | 1501 / 7501 | **1501 / 7501 정확 일치** |
| 웜 실행 저하 | 없음 | 0.51ms vs 0.31ms — 무해 수준 |

(위키백과 ~50ms·스크롤 2.0ms/frame은 파이썬 셸 경유 측정치라 러스트
하니스 범위 밖 — 기존 07-18 실측 기록 유지.)

## HTTP/1.1 스택 — 처음으로 네트워크 주장 실증 (`validation/net_gauntlet.py`)

로컬 http.server 2대(적중·접속 카운터)로 14/14:

- keep-alive 풀: 같은 호스트 GET 2회 = **TCP 접속 1개** (서버측 카운트)
- gzip 73B→2112B 투명 해제, chunked(확장·트레일러 포함) 재조립
- 301→302→200 체인 최종 URL 전파, 자기 리다이렉트는 **8+1회에서
  RuntimeError** (행 없음)
- max-age 캐시: 3회 페치에도 서버 적중 1 — 메모리 캐시 비운 뒤에도 1
  (**디스크 계층 단독 증명**, 리눅스는 `/tmp/gg-browser/cache`),
  `no_cache=True`로 2가 됨
- 64KiB 헤더·대소문자 섞인 content-length·콜론 없는 쓰레기 줄 무사 통과
- POST 미지원 확인(문서와 일치), file:/data:/about: 스킴 동작

## gg-js 언어 건틀릿 (`validation/jsvm_gauntlet.py`, node v22 대조)

README 언어 주장 20개 스니펫 전원 node와 문자열 단위 일치: 프로토타입
체인·클래스(#private·super·static 포함)·제너레이터(문장 레벨)·async/await
(10개 위치)·구조 분해·spread/rest·옵셔널 체이닝·??·디스크립터 API·
Map/Set/WeakMap/WeakSet·Symbol 페이크·정규식·ToPrimitive(부작용 카운터로
실호출 증명)·라벨·비트연산(ToInt32 래핑 15식)·arguments·uncurry·배열
메서드·에러 계층. **lazy parse 시맨틱도 재현**: 24토큰 이상 본문의 문법
오류가 로드는 통과하고 첫 호출에서 catch 가능한 SyntaxError (경계값 포함).

경계 탐침(GAP 케이스)이 고정한 한계 — 전부 시끄러운 에러거나 문서화된
페이크, **조용한 오답 0건**:

- 루프/조건 안 `yield`·for-of 본문 안 `await`은 명시 거부 (문서화됨)
- 중첩 구조 분해 디폴트(`{z:{w}={w:9}}`)·파라미터 내부 디폴트는 파스 에러
- 디스크립터 속성 미강제: `writable:false`여도 덮어써짐 (기능은 사이드
  테이블, 속성 강제는 없음)
- `exec` 결과에 `.index`/`.input` 없음, `lastIndex` 미갱신
- string-hint ToPrimitive가 valueOf 우선 (`String(o)`는 toString,
  `${o}`는 valueOf — 스펙 편차)
- `typeof [][Symbol.iterator]`가 'undefined' (빌트인 프로토타입엔 페이크
  심볼 키 부재)

## CSS/레이아웃 독립 건틀릿 (`validation/css_gauntlet.py`) — 22/23

smoke의 단언을 재사용하지 않고 실좌표로 재검증: 속성 셀렉터 7형 전부,
:not/:nth-child/:first/:last-child, 명시도·캐스케이드 순서, @media 4방향,
var() 체인·fallback·**순환 가드(서브프로세스 20s 감시로 무행 증명)**,
flex space-between(13/263/513px 정확)·align-center(dy=40.0 정확)·flex:1
(400px), float 회피(x 13→213, 폭 1254→1054)·clear 강하·플로트 봉쇄 높이,
absolute/fixed/relative 오프셋 정확, z-index 페인트 순서 역전, ellipsis
(34자→'Supercalif…' 11자), ::before/::after 순서, overflow 클립 kind 6/7
브래킷, transform translate(정확 20,10)·scale(0) 소멸, nowrap 1줄화.

**발견된 결함 1건 — `line-height`가 블록 요소 직속 텍스트에 무시됨**:
Rust export가 Text 노드에 상속시키는 속성 화이트리스트(color·font-*·
text-align·white-space)에 line-height가 빠져 있고, LineLayout이 line-height를
Text 노드에서 읽는 구조라 `<p style="line-height:40px">`가 기본 1.25 배율
그대로(23.28px) 나온다. span 등 인라인 요소에 직접 주면 정상(46.56px).
네이버 홈 ×380 사용 주장 대비 실효가 제한적일 수 있음 → 체크리스트에 기록.

## 시각 증거 (`validation/render_evidence.py` → `validation/out/*.png`)

shell.py의 native 경로 그대로(디스플레이 리스트 Rust 상주 →
`render_frame`) 헤드리스 PNG 4장:

- **about:home**: 99cmd/86텍스트, 한글 타이포 정상
- **demo_live.html**: settle이 가상 시계를 대량 fast-forward — 초시계
  418897, rAF 26181102, setTimeout(3000) 문구 교체·5초 간격 리스트 성장
  전부 화면에 보임 = 타이머·rAF·DOM 변이의 시각적 증명
- **CSS 쇼케이스**(1280×1893, 비백색 91.3%, 색 1994종): 알약 radius·
  그림자·가상요소·z-index 재배열·flex·float·ellipsis·그라디언트 폴백·
  색상 칩·var() 체인·opacity:0 은닉까지 확인. 관찰: CSS content의
  `\3010` 유니코드 이스케이프 미해석, absolute가 relative 조상이 아닌
  뷰포트 기준(문서화된 근사), 인라인 SVG는 `<path>`만 래스터(rect/circle
  등 도형 요소는 자리표시자)
- **JS 전용 페이지**: 빈 `<div id=app>`만 있는 HTML에서 클래스 컴포넌트
  (#private=42)·제너레이터 피보나치 리스트 6항·구조분해/spread/옵셔널/
  Map/Set 계산 결과·JS가 바꾼 title까지 전부 픽셀로 렌더

## 재현 방법

```
cd ggcore && cargo test --lib               # 171종
maturin build --release -o dist && pip 재설치
python smoke_test.py                         # 149종 (egress 제한 환경은 141에서 네트워크 관문)
python validation/net_gauntlet.py            # 14종 — 네트워크 없는 환경에서도 HTTP 스택 실증
python validation/jsvm_gauntlet.py           # 30종 (--node로 node 대조 재생성)
xvfb-run -a python validation/css_gauntlet.py    # 23종
xvfb-run -a python validation/render_evidence.py # PNG 4장
```

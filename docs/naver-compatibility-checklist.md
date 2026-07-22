# 네이버 Chromium 동급 호환성 체크리스트

기준일: 2026-07-23

목표는 네이버 홈이 단순히 "보이는" 상태가 아니라, 같은 응답과 같은 뷰포트에서
Chromium 계열 브라우저와 거의 같은 화면과 동작을 제공하는 것이다. 모든 완료 판정은
현재 소스로 빌드한 release wheel과 실제 GG 실행 경로에서 다시 검증한다.

## 0. 범위와 완료 기준

### 1차 완성 범위

- `https://www.naver.com/` 데스크톱 홈
- 검색어 입력과 `search.naver.com` 검색 결과 이동
- 뉴스스탠드, 관심사 피드, 로그인 패널, 푸터
- 클릭, hover, focus, 스크롤, 뒤로/앞으로, 새로고침
- 로그인 페이지 표시와 쿠키 세션 유지

메일·카페·블로그·쇼핑·지도·페이의 서비스 내부 화면은 홈 완성 후 별도 호환성
바스켓으로 확장한다. 실제 계정 로그인과 2단계 인증은 자격 증명을 저장하지 않고
수동 검증한다.

### "완벽"의 합격선

- [ ] reader mode나 네이버 전용 DOM 주입 없이 네이버 자체 React DOM이 렌더된다.
- [ ] 빈 화면, 크래시, 무한 로딩, 치명적인 미처리 JS 예외가 없다.
- [ ] 주요 박스의 x/y/width/height가 Chromium 기준에서 2px 이내다.
- [ ] 줄바꿈, 말줄임, 텍스트 baseline과 카드 행 수가 Chromium과 같다.
- [ ] 고정 응답 재생 화면의 perceptual similarity가 0.98 이상이다.
- [ ] 검색·클릭·스크롤·키보드·히스토리 핵심 시나리오가 전부 통과한다.
- [ ] cold usable 시간이 Chromium의 2배 이내 또는 5초 이내다.
- [ ] warm usable 시간이 Chromium의 2배 이내 또는 2초 이내다.
- [ ] 입력 반응 p95가 100ms 이내이고 네이티브 스크롤이 55fps 이상이다.
- [ ] 100회 연속 로드와 30분 사용에서 크래시·무한 증가·상태 오염이 없다.

## 1. 측정 기준 고정 — 가장 먼저

- [x] 현재 Rust/Python 소스로 release wheel을 재빌드하고 재설치한다. (07-23)
- [ ] 실행 결과에 git commit, wheel hash, Python/Rust 버전을 기록한다.
- [ ] Rust, Python smoke, network/CSS/JSVM gauntlet을 모두 통과시킨다.
- [ ] 기준 환경을 Windows, 1280x900 CSS viewport, DPR 1.0으로 고정한다.
- [ ] Chromium과 GG가 동일한 User-Agent/언어/쿠키 상태를 사용하게 한다.
- [ ] 네이버 HTML, CSS, JS, 이미지, API 응답을 한 번 캡처해 결정론적 replay를 만든다.
- [ ] 동적 광고·시계·롤링 콘텐츠를 마스킹한 스크린샷 diff를 만든다.
- [ ] DOM snapshot, computed style, layout box, display list, 콘솔 오류를 한 보고서에 저장한다.
- [ ] live 네이버와 고정 replay를 분리해 둘 다 검증한다.

완료 조건: 동일 명령으로 Chromium 기준 이미지, GG 이미지, diff heatmap, 요소별
좌표 차이와 콘솔 오류 목록을 다시 생성할 수 있어야 한다.

## 2. 체감 로딩 시간 — P0

현재 구조는 HTML fetch만 백그라운드에서 수행하고, 이후 `load_document`의 대형
스크립트 실행, 최대 8초 async settle, 이미지 로드를 UI 스레드에서 동기적으로
끝낸 뒤 첫 프레임을 표시한다. 스크립트 예산도 파일 사이에서만 검사되므로 대형
번들 하나가 오래 실행되면 중간 paint가 불가능하다.

- [x] navigation/HTML, parser-time scripts, style, DOM export, first paint를
      구조화 로그로 기록한다. (07-23)
- [ ] compile, execute, React commit, async fetch, image, usable 시점을 더 세분화한다.
- [ ] Chromium과 GG의 cold/warm 단계별 시간을 같은 네트워크 응답으로 비교한다.
- [x] 브라우저 chrome과 로딩 상태를 HTML 도착 전부터 계속 그린다.
- [x] 전체 async quiescence, 전체 이미지, 웹폰트를 첫 프레임의 선행 조건에서 제거한다.
- [ ] React root에 첫 유효 콘텐츠가 commit되면 즉시 style/layout/paint한다.
- [x] 나머지 fetch, lazy card, 이미지와 폰트는 live loop에서 점진적으로 반영한다.
- [ ] 긴 `gg-js` 실행을 frame budget에서 yield하고 이어서 실행할 수 있게 만든다.
- [ ] Stop 또는 새 탐색이 script 실행과 settle까지 협조적으로 즉시 취소하게 한다.
- [ ] 네이버 profile의 `fn.apply`, property lookup, allocation, 앱의 JS JSON parser
      hot path를 범용 VM 최적화로 줄인다.
- [ ] 같은 URL의 중복 script/style/image fetch와 중복 style/export를 제거한다.
- [ ] 첫 paint 뒤 화면 밖 lazy 콘텐츠를 초기 viewport 때문에 강제 실행하지 않는다.
- [ ] 로딩 중 UI thread의 단일 무응답 구간이 100ms를 넘으면 테스트를 실패시킨다.
- [ ] cold usable ≤ min(Chromium×2, 5초), warm usable ≤ min(Chromium×2, 2초)를 만족한다.

완료 조건: 네이버가 완전히 끝날 때까지 빈 창으로 기다리지 않고, 검색창·로그인·
첫 뉴스 영역이 먼저 보이며 이후 카드와 이미지가 점진적으로 채워져야 한다.

## 3. JavaScript·DOM·웹 API

- [x] 네이버 React 18 `createRoot`의 실제 DOM 렌더와 commit을 역사적으로 확인했다.
- [x] reader mode 없이 뉴스·관심사 피드의 자체 React DOM 렌더를 확인했다.
- [ ] 현재 release wheel에서 네이버 번들 7종의 compile/runtime 오류 0을 재확인한다.
- [ ] Promise, microtask, timer, rAF 순서가 Chromium과 같은 fixture를 통과한다.
- [ ] fetch/XHR의 오류·취소·redirect·JSON 처리 순서를 검증한다.
- [ ] MutationObserver와 IntersectionObserver의 callback 순서와 rect를 비교한다.
- [ ] `getBoundingClientRect`, client/offset/scroll 계열 값을 요소별로 대조한다.
- [ ] 동적 script와 ES module의 load/error/lifecycle 순서를 실페이지에서 확인한다.
- [ ] 멀리 예약된 롤링 timer가 초기 settle을 폭주시키지 않는지 확인한다.
- [ ] 컴포넌트 오류로 React 서브트리가 조용히 버려지면 테스트를 실패시킨다.
- [ ] 네이버 전용 예외 없이 발견한 결함마다 최소 JS/DOM 회귀 fixture를 추가한다.

## 4. 글꼴과 텍스트 — 시각 차이 최우선

- [ ] WOFF/WOFF2 웹폰트 로드와 CSS 파일 기준 상대 URL을 지원한다.
- [ ] `font-family`, weight, style, synthetic bold/italic 선택을 Chromium과 맞춘다.
- [ ] 한글·영문·숫자 혼합 문자열의 advance, kerning, glyph fallback을 대조한다.
- [ ] 복합문자, variation selector, emoji와 combining mark shaping을 검증한다.
- [ ] block 직속 text에도 상속된 `line-height`가 정확히 적용되게 한다.
- [ ] baseline, ascent, descent, line box와 vertical-align을 요소별로 맞춘다.
- [ ] letter-spacing, word-spacing, white-space, ellipsis와 줄바꿈을 맞춘다.
- [ ] 검색창, 뉴스 제목, 피드 카드, 로그인 패널, 푸터의 텍스트 좌표 diff를 2px 이내로 줄인다.

완료 조건: 대표 텍스트 100개의 폭 오차 p95가 1px 이하이고, 기준 화면의 줄바꿈이
Chromium과 모두 같아야 한다.

## 5. 레이아웃

- [ ] `inline-block` formatting context와 baseline 정렬을 정식 구현한다.
- [ ] flex의 min/max-content, auto minimum size, shrink/grow, stretch를 맞춘다.
- [ ] percentage width/height와 definite containing block 판정을 맞춘다.
- [ ] absolute/fixed/sticky의 containing block과 inset auto 해석을 맞춘다.
- [ ] margin collapsing, overflow formatting context, float/clear 좌표를 재검증한다.
- [ ] replaced element의 intrinsic size, aspect-ratio, object-fit/object-position을 맞춘다.
- [ ] grid/table이 홈에서 사용될 경우 Chromium 좌표와 span 결과를 비교한다.
- [ ] 뉴스스탠드·관심사 탭의 간격, padding, 말줄임과 pagination을 맞춘다.
- [ ] 어떤 viewport 폭에서도 탭 겹침, 카드 중첩, 전체 페이지 overlay가 없어야 한다.
- [ ] 1024, 1280, 1440, 1920px 폭에서 반응형 레이아웃을 비교한다.

## 6. 페인트와 합성

- [ ] 다중 `background-image`와 url+gradient 레이어 순서를 지원한다.
- [ ] linear/radial gradient의 방향, stop, alpha를 실제로 보간한다.
- [ ] box-shadow의 blur, spread, inset과 다중 shadow를 구현한다.
- [ ] 모서리별 border-radius와 이미지·자식 clipping을 정확히 처리한다.
- [ ] opacity를 서브트리 단위 offscreen 합성으로 처리한다.
- [ ] transform의 translate/scale/rotate/matrix와 transform-origin을 완성한다.
- [ ] transform·sticky·scroll offset이 paint와 hit-test에서 동일하게 적용되게 한다.
- [ ] SVG stroke, gradient, clip-path 등 네이버가 실제 사용하는 표면을 지원한다.
- [ ] DPR, 이미지 보간, 색상 alpha, 글리프 anti-alias 차이를 정량화한다.
- [ ] transition과 `@keyframes`를 공통 frame scheduler에 연결한다.
- [ ] 롤링 뉴스와 hover 효과가 점프하지 않고 Chromium과 비슷하게 움직이게 한다.

## 7. 사용자 상호작용

- [ ] 검색창 클릭과 focus ring이 정확한 위치에 표시된다.
- [ ] 한글 IME compositionstart/update/end와 완성 문자열 입력이 동작한다.
- [ ] 좌우 이동, Home/End, 선택, Delete/Backspace, clipboard를 지원한다.
- [ ] Enter 검색 제출의 URL·인코딩·파라미터가 Chromium과 같다.
- [ ] 링크·버튼·탭의 hover/focus/active와 커서 모양을 맞춘다.
- [ ] 뉴스, 피드 카드, 언론사 로고를 클릭하면 올바른 URL로 이동한다.
- [ ] wheel, scrollbar drag, PageUp/Down, Home/End와 가로 스크롤을 검증한다.
- [ ] sticky 요소의 시각 위치와 클릭 위치가 스크롤 중에도 일치한다.
- [ ] 뒤로/앞으로가 DOM, 폼 값, focus, 스크롤 위치를 복원한다.
- [ ] 새로고침·중지·연속 탐색이 이전 요청과 timer를 안전하게 취소한다.
- [ ] 키보드만으로 검색창부터 주요 링크까지 순차 탐색할 수 있다.

## 8. 네트워크·저장소·로그인 경로

- [ ] HTTP/1.1 keep-alive, chunked, gzip과 실제 네이버 응답을 반복 검증한다.
- [ ] Brotli 응답 또는 광고하지 않는 content-encoding 협상을 명확히 처리한다.
- [ ] cache-control, Vary, ETag, Last-Modified가 새 콘텐츠를 가리지 않게 한다.
- [ ] redirect 시 method/body/cookie/referrer 처리 결과를 Chromium과 대조한다.
- [ ] Domain/Path/Secure/HttpOnly/SameSite/PSL 쿠키가 네이버 도메인 간 유지된다.
- [ ] localStorage/sessionStorage가 origin별로 격리되고 재방문 시 유지된다.
- [ ] CORS, preflight, credentials와 mixed-content 실패를 페이지에 정확히 전달한다.
- [ ] 로그인 페이지의 iframe 사용 여부를 조사하고 필요하면 문서 격리를 구현한다.
- [ ] 인증서·DNS·timeout·오프라인 오류를 빈 화면 대신 사용자 UI로 표시한다.
- [ ] 실제 로그인은 수동으로 성공/실패/로그아웃/재시작 세션 유지까지 검증한다.

## 9. 성능과 안정성

- [ ] 네이버 앱 번들의 compile, execute, JSON parse, DOM commit을 단계별 profile한다.
- [ ] 초기 렌더를 막는 가장 큰 hot path부터 IC·내장 함수·할당 비용을 줄인다.
- [ ] 화면 밖 lazy 콘텐츠 때문에 초기 로드가 전체 문서 렌더로 확장되지 않게 한다.
- [ ] display list cache와 damage tracking으로 스크롤 전체 재페인트를 없앤다.
- [ ] 애니메이션·hover·입력을 paint-only와 layout invalidation으로 분리한다.
- [ ] 장시간 피드 갱신을 위한 GC와 DOM/이미지/cache 메모리 회수를 검증한다.
- [ ] reload 100회, back/forward 100회, scroll 10분 stress를 통과한다.
- [ ] 네트워크 지연·응답 누락·잘못된 이미지에서도 UI thread가 멈추지 않는다.
- [ ] release와 debug의 동작 차이가 없도록 같은 호환성 suite를 실행한다.

## 10. 최종 Chromium 대조 게이트

- [ ] 고정 replay를 10회 실행해 DOM 수, 텍스트, 이미지, 좌표가 결정론적으로 같다.
- [ ] live 네이버를 20회 실행해 서버가 선택한 활성 피드 탭을 정확히 반영한다.
- [ ] logo/search/login/news/feed/footer의 시각 diff가 합격선을 통과한다.
- [ ] 검색, 뉴스 클릭, 피드 클릭, 스크롤, history 시나리오가 모두 통과한다.
- [ ] uncaught JS exception, failed critical request, overlap, invisible critical content가 0이다.
- [ ] reader mode와 네이버 전용 스타일·DOM·데이터 주입이 꺼져 있음을 검사한다.
- [ ] 테스트 결과와 남은 차이를 스크린샷 및 수치가 포함된 보고서로 남긴다.

## 바로 실행할 순서

1. release wheel 재빌드와 전체 회귀 게이트
2. 단계별 로딩 계측과 Chromium cold/warm 기준선 생성
3. 첫 paint를 full settle·전체 이미지 로드에서 분리
4. 긴 JS의 cooperative yield와 네이버 VM hot path 최적화
5. Chromium/GG 동일 응답 replay 및 screenshot/layout diff 생성
6. 글꼴·line-height·baseline 차이 제거
7. inline-block과 flex 좌표 차이 제거
8. shadow·gradient·radius·다중 배경 정확화
9. transition/keyframes와 frame scheduler
10. 한글 IME, 검색·history, 쿠키 세션·로그인 경로
11. GC, 스크롤 성능, 20회 live + 100회 stress 최종 게이트

모든 수정은 `재현 fixture → 범용 엔진 수정 → 단위/통합 테스트 → 네이버 replay →
live 네이버` 순서로 검증한다. 네이버 호스트명이나 클래스명을 조건으로 하는 패치는
허용하지 않는다.

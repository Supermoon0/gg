# GG 통합 TODO 체크리스트

프로젝트 전체의 남은 작업을 한 곳에 모은 목록. 세부 이력·완료 항목은
[네이버 완벽 구동 체크리스트](naver-perfect-checklist.md),
[jsvm 연구 노트](jsvm-research.md),
[AI 네이티브 전략](ai-native-strategy.md) 참조.
항목을 끝내면 `[x]`로 바꾸고 날짜를 적을 것 (원본 문서도 함께 갱신).

**검증 체인**: `cargo test --lib` → maturin 휠 빌드·재설치 →
`python smoke_test.py` → `python basket_test.py`

---

## P0 — 현재 프런티어: 네이버 main(React) 번들 부팅

polyfill·preload(jQuery)는 완주(07-19). 앱 부팅의 마지막 관문.

- [ ] search 번들 잔여 관문 1개 격파
- [ ] main(React) 번들 실행 완주 — React DOM 초기화 도달
- [ ] 부팅 후 첫 화면(뉴스·쇼핑·피드) 실렌더 확인 — B단계 판정
- [ ] 동적 주입 스크립트의 후속 로드·실행 경로 점검 (preload가 주입까지는 확인됨)

## P1 — 부팅 직후 체감 (성능·상호작용)

- [ ] JS 실행 성능: 1.3MB 번들을 예산 내 실행 — IC 확대부터, 병목 재측정
- [ ] M4 부분 무효화 ↔ hover/focus 재스타일 합류 — 네이버 규모 전체 재스타일 탈피
- [ ] 리페인트 부분화 — 디스플레이 리스트 캐시/타일 (인터랙션 60fps의 전제)
- [ ] winit(native) 셸에 라이브 틱 루프 배선 (현재 tkinter만)
- [ ] native 셸 텍스트 입력 배선 (포커스·타이핑·캐럿 — 현재 tkinter만)
- [ ] IME 한글 조합 (tkinter 캔버스 IME — 로컬 윈도우 실기 검증 필요)
- [ ] 캐럿 이동·선택 (현재 캐럿은 값 끝 고정)
- [ ] `document.cookie` ↔ 네트워크 계층 연동 (요청에 실어 보내기, 세션 유지)
- [ ] getBoundingClientRect 스크롤 반영 (현재 문서좌표 근사)
- [ ] 시작 시간 단축 — 파이썬 기동+창+폰트 수백 ms (프로파일 후 캐시)
- [ ] 윈도우 실기 재실측 — 스크롤 fps·네이버 웜 로드 (컨테이너 수치와 대조)

## P2 — 레이아웃·CSS 일반화 ("모든 웹사이트" 방향)

- [ ] 테이블 레이아웃 (옛 사이트·정부 사이트 뼈대)
- [ ] `display: grid` (GitHub·뉴스 사이트 도배 수준)
- [ ] `position: sticky`
- [ ] margin collapsing
- [ ] inline-block 정식 배치 (지금은 근사)
- [ ] `overflow: auto` 내부 스크롤 영역 (채팅창·사이드바)
- [ ] float 잔여: 인라인 흐름 안의 float, float 아래 텍스트 재확장, margin 스택 근사
- [ ] 폼 컨트롤 렌더링 (`<select>`·체크박스·라디오)
- [ ] `transition` / `@keyframes` 애니메이션 (시각 완성도)
- [ ] 웹폰트 woff/woff2 디코드 (실사이트 대부분이 woff2 — fontdue에 인플레이터 없음)
- [ ] 웹폰트 잔여: CSS 파일 기준 상대경로 resolve, unicode-range
- [ ] 바스켓 20개로 확장 (쇼핑몰·SPA·커뮤니티) — "실용적 완벽"의 결승선 지표

## P3 — gg-js 엔진 심화

- [ ] ES 모듈 (import/export)
- [ ] `Proxy` / `Reflect` — 의도적 보류 중 (반쪽 스텁은 폴리필 오판 유발, 실물로 갈 것)
- [ ] baseline JIT — 부팅 후 실행 시간이 병목으로 판명되면 (3~10배 목표)
- [ ] mark-sweep GC — arena는 자라기만 함, 무한 스크롤·긴 세션 누수
- [ ] 인스턴스별 메모리 상한 + 협조적 cancel 핸들
- [ ] `#x in obj` 프라이빗 브랜드 체크
- [ ] 루프/조건 안 `yield` (현재 명시 에러)
- [ ] spread에 이터레이터 프로토콜 적용 (`[...set]` — 현재 concat 디슈가라 미적용)
- [ ] Symbol 실물화 (현재 문자열 페이크 — typeof가 'string')

## P4 — 아키텍처·인프라

- [ ] 레이아웃 러스트 이식 — 피드 수천 노드 + 재레이아웃이 오면 파이썬이 병목
      (파싱/스타일 242→11ms 같은 승리가 한 번 더 남은 곳; 부팅 후 측정하고 결정)
- [ ] 탈 tkinter — tk PhotoImage 스왑 25ms가 잔여 병목 (native 셸 완성도와 연동)
- [ ] P5: C ABI + headless Linux 빌드 + Boa 폴백 제거
- [ ] 디스크 캐시 범위 확대 검토 (현재 명시적 max-age만)
- [ ] HTTP/2

## P5 — 장기 / 별도 프로젝트급

- [ ] 로그인 — 쿠키 보안 속성 전체(Secure/HttpOnly/SameSite), HTTPS 세션
- [ ] iframe 문서 격리, CORS (광고·로그인·임베드)
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>` / `<audio>` / WebGL (유튜브·지도류)
- [ ] HTML5 오류 복구 완전판, quirks 모드, EUC-KR 등 레거시 인코딩, RTL/양방향
- [ ] AI 통합 (엔진 수준 — 전략 문서의 주차장 항목):
  - [ ] 요약/질문 사이드 패널 (레이아웃 트리 텍스트 추출 기반, 2~3일 규모)
  - [ ] 그레이스풀 디그러데이션 AI — 렌더 실패 감지 → 리더 모드 재구성
  - [ ] 에러 로그 AI 분류 → 체크리스트 갱신 자동화

---

*작성: 2026-07-19 — 기준 상태: cargo 171종 · smoke 149종 · basket 8/9 읽을만함 ·
polyfill/preload 번들 완주, main(React) 부팅이 프런티어.*

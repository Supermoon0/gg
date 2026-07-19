# GG 통합 TODO 체크리스트

프로젝트 전체의 남은 작업을 한 곳에 모은 목록. 세부 이력·완료 항목은
[네이버 완벽 구동 체크리스트](naver-perfect-checklist.md),
[jsvm 연구 노트](jsvm-research.md),
[AI 네이티브 전략](ai-native-strategy.md) 참조.
항목을 끝내면 `[x]`로 바꾸고 날짜를 적을 것 (원본 문서도 함께 갱신).

**검증 체인**: `cargo test --lib` → maturin 휠 빌드·재설치 →
`python smoke_test.py` → `python basket_test.py`
(egress 제한 CI에서는 `GG_SKIP_NET=1` + xvfb, 휠은 tkinter 있는
파이썬 버전으로 `maturin build -i` 지정)

---

## 다음 스프린트 — 권장 공략 순서 (07-19 스프린트 이후)

07-19 스프린트가 컴파일 관문을 소진했다(React 18이 부팅·리렌더됨).
이제 병목은 **실행 성능과 렌더 루프**다. 순서 근거와 함께:

### 0. 로컬 검증 라운드 (컨테이너에서 불가능한 것 먼저 정리)
- [ ] **네이버 원본 search/main 번들 재검증** — u16 레지스터 이후
      어디까지 가는지. 07-19 이전의 "search 관문 1개"가 소멸했을
      가능성이 높다. 죽는 지점 로그를 받아오는 것이 다음 엔진
      캠페인의 입력
- [ ] basket_test 9곳 재실측 — P2 레이아웃 스윕(테이블/grid/
      inline-block/margin collapsing)이 실사이트 렌더를 어떻게
      바꿨는지. 우연히 이전 근사에 의존한 렌더 회귀 감시
- [ ] 윈도우 실기: 스크롤 fps·웜 로드 재측정, IME 한글 조합 검증

### 1. JS 실행 성능 캠페인 (부팅 다음의 병목)
- [ ] 프로파일 하니스로 병목 재측정 (`profile_phases` — React/네이버
      번들 기준. 07-16 실측은 JS가 웜 로드의 93%)
- [ ] IC(인라인 캐시) 커버리지 확대 — 미스 패턴 상위부터
- [x] u16 확장의 성능 여파 확인 — 07-19: 퇴행 없음 (parse 9.3ms·
      컴파일 6.5ms·웜 실행 0.46ms — 확장 전과 동일)

### 2. 렌더 루프 완성 (인터랙션 60fps의 전제)
- [ ] M4 부분 무효화 ↔ hover/focus 재스타일 합류
- [ ] 리페인트 부분화 — 디스플레이 리스트 캐시/타일
      (`position: sticky` 스크롤 핀도 여기서 함께)
- [ ] winit(native) 셸: 라이브 틱 루프 + 텍스트 입력 배선

### 3. 레이아웃 v1 → v2 (실사이트 피드백 기반)
- [x] 테이블 자동 컬럼 폭 — 07-19: 콘텐츠 natural width 비례 배분
      (colspan 분산, 컬럼 바닥 20px). 정부 사이트 실검증은 로컬
- [x] grid `grid-column: span N` 배치 — 07-19 (a/b 라인은 폭만,
      명시 시작 위치는 auto 배치 유지)
- [ ] float 잔여 (인라인 흐름 float, 텍스트 재확장)
- [x] 폼 컨트롤 렌더링 — 07-19 (체크박스·라디오·select 닫힌 상태
      + 클릭 토글)
- [ ] `overflow: auto` 내부 스크롤 (클립은 완료 — 스크롤 상호작용)

### 4. 엔진 심화 (필요가 확인되는 순서로)
- [ ] mark-sweep GC + 인스턴스별 메모리 상한 (긴 세션 전제 조건)
- [ ] dynamic `import()` (ES 모듈 링커의 다음 조각)
- [ ] 루프/조건 안 `yield`, Symbol 실물화 (`typeof` 교정)
- [x] woff2 디코드 — 07-19 완료 (woff2-patched → FontStore 앞단)

### 5. 그 다음 (측정 후 결정)
- [ ] baseline JIT · 레이아웃 러스트 이식 — 1의 실측이 가리키면
- [ ] transition/@keyframes · 캐럿 이동/선택 · 시작 시간
- [ ] 바스켓 20개 확장 · HTTP/2 · AI 리더 모드 1단계

---

## P0 — 현재 프런티어: 네이버 main(React) 번들 부팅

- [x] **React 18 UMD 실번들 부팅 (합성 검증)** — 07-19: u16 레지스터
      확장으로 react-dom 131KB 컴파일 관문("expression too deep")
      소멸 + MessageChannel 실물화(스케줄러 플러시). production.min
      기준 ReactDOM.render 동기 마운트, createRoot+useState+useEffect
      스케줄러 경유 마운트, dispatchEvent(click)→setState→리렌더까지
      전 루프 검증(`react_boot_diag`, 번들은 bench/js/react).
      **네이버 원본 번들 재검증은 로컬(네이버 egress 필요)에서**
- [ ] 네이버 원본 search/main 번들 완주 — 로컬 실측 (환경 egress 차단)
- [ ] 부팅 후 첫 화면(뉴스·쇼핑·피드) 실렌더 확인 — B단계 판정
- [x] **동적 주입 스크립트 실행 채널** — 07-19: pending_scripts
      큐 → settle/라이브 틱/드라이버 드레인 → load/error 발화.
      합성 픽스처 3단 체인 완주 (설계 N1)

## P1 — 부팅 직후 체감 (성능·상호작용)

- [ ] JS 실행 성능: 1.3MB 번들을 예산 내 실행 — IC 확대부터, 병목 재측정
- [ ] M4 부분 무효화 ↔ hover/focus 재스타일 합류 — 네이버 규모 전체 재스타일 탈피
      (07-19: 구조 변이는 refresh_partial 서브트리 스플라이스 v1 가동 — 설계 N3;
      잔여는 부분 레이아웃·리페인트)
- [ ] 리페인트 부분화 — 디스플레이 리스트 캐시/타일 (인터랙션 60fps의 전제)
- [ ] winit(native) 셸에 라이브 틱 루프 배선 (현재 tkinter만)
- [ ] native 셸 텍스트 입력 배선 (포커스·타이핑·캐럿 — 현재 tkinter만)
- [ ] IME 한글 조합 (tkinter 캔버스 IME — 로컬 윈도우 실기 검증 필요)
- [ ] 캐럿 이동·선택 (현재 캐럿은 값 끝 고정)
- [x] **`document.cookie` ↔ 네트워크 계층 연동** — 07-19: net.py에
      세션 쿠키 자(호스트 단위, name=value v1 — Secure는 http에서
      드롭, Max-Age≤0 삭제), 응답 Set-Cookie 수집 + 동일 호스트
      요청에 Cookie 헤더 자동 첨부. Doc.get/set_cookies로 document.
      cookie와 양방향 동기화(스크립트 실행 전 시드, lifecycle/settle
      후 회수). 잔여: Path/Domain/만료 정밀 시맨틱, 디스크 영속화
- [x] **getBoundingClientRect 스크롤 반영** — 07-19: 양 셸이
      clamp_scroll에서 Doc.set_scroll 푸시, gBCR이 스크롤을 빼고
      뷰포트 상대 좌표 반환(스펙 일치)
- [ ] 시작 시간 단축 — 파이썬 기동+창+폰트 수백 ms (프로파일 후 캐시)
- [ ] 윈도우 실기 재실측 — 스크롤 fps·네이버 웜 로드 (컨테이너 수치와 대조)

## P2 — 레이아웃·CSS 일반화 ("모든 웹사이트" 방향)

- [x] **테이블 레이아웃 v1** — 07-19: table/display:table + tr/td·th
      (행 그룹 통과, display:table-row/cell 수용). 균등 컬럼 분할,
      colspan은 그 배수 폭, 행 높이는 최고 셀. 07-19 v2: 콘텐츠 비례
      자동 컬럼 폭. 잔여: border-spacing, rowspan
- [x] **`display: grid` v1** — 07-19: grid-template-columns
      (px/%/em·fr·auto=1fr·repeat(n,…)), gap/row-gap/column-gap.
      자식을 행 우선 배치, 행 높이는 최고 아이템. 07-19 v2: `grid-column:
      span N`. 잔여: 명시 시작 위치·grid-area, 암시적 트랙 사이징
- [ ] `position: sticky` — 현재 일반 흐름 렌더(초기 화면은 정상),
      스크롤 시 고정(pinning)은 디스플레이 리스트 캐시와 함께
- [x] **margin collapsing** — 07-19: 인접 형제 마진 붕괴(CSS 2.1 —
      양수 max, 음수 min, 혼합 합). 잔여: 부모-자식 붕괴, 빈 블록
- [x] **inline-block 정식 배치** — 07-19: 폭 명시 박스가 라인의
      원자 박스로 배치(InlineBlockLayout — 미리 레이아웃한 블록을
      베이스라인 패스가 배치, finalize()가 내부 트리 이동). auto 폭은
      기존 인라인 흐름 폴백(float v1과 같은 게이트)
- [x] **`overflow: auto` 클리핑** — 07-19: hidden과 동일하게 클립
      (v1: 내부 스크롤은 아직 — 콘텐츠가 새어나오지만 않음)
- [ ] float 잔여: 인라인 흐름 안의 float, float 아래 텍스트 재확장, margin 스택 근사
- [x] **폼 컨트롤 렌더링** — 07-19: 체크박스/라디오(14px UA 박스,
      체크 마크·내부 점, 클릭 토글 — 라디오는 그룹 배타, Rust DOM
      미러), select 닫힌 상태(선택 옵션 라벨+화살표, option은
      display:none). 잔여: select 드롭다운 팝업
- [ ] `transition` / `@keyframes` 애니메이션 (시각 완성도)
- [x] **웹폰트 woff2 디코드** — 07-19: woff2-patched(순수 러스트)로
      ttf 변환 후 fontdue 등록, 실패 시 폴스루. Lato 샘플 E2E.
      잔여: woff(1)
- [ ] 웹폰트 잔여: CSS 파일 기준 상대경로 resolve, unicode-range
- [ ] 바스켓 20개로 확장 (쇼핑몰·SPA·커뮤니티) — "실용적 완벽"의 결승선 지표

## P3 — gg-js 엔진 심화

- [x] **ES 모듈 v1 (정적 링커)** — 07-19: `<script type=module>`을
      실행 전에 클래식 스크립트로 링크(browser/esmodules.py) —
      정적 import를 깊이우선 인라인(URL 중복 제거), 모듈별 IIFE가
      __ggmod에 export 등록, import 문은 var 읽기로 재작성(default/
      named/renamed/namespace/bare). 게이트: dynamic import()·
      re-export·라이브 바인딩·import.meta 미지원, 링크 실패 시 원본
      실행. E2E: 링크 결과가 gg-js에서 실행 검증
- [ ] `Proxy` / `Reflect` — 의도적 보류 중 (반쪽 스텁은 폴리필 오판 유발, 실물로 갈 것)
- [x] **u16 레지스터 파일** — 07-19: "expression too deep" 영구
      소멸(u8 250 한도가 최소화 번들의 마지막 컴파일 관문이었음).
      argc/nparams는 u8 유지, 16000은 폭주 가드
- [x] **spread 이터레이터 프로토콜** — 07-19: [...set]/f(...str)/
      new C(...x)가 IterMat 노드로 실물화(Set/Map/문자열/@@iterator).
      문자열은 문자 배열로 분해(for-of도 승격)
- [x] **`#x in obj` 브랜드 체크** — 07-19: 표현식 위치의 프라이빗
      이름이 '#x' 키 문자열로 낮춰져 own-property 검사 = 브랜드 체크
- [ ] baseline JIT — 부팅 후 실행 시간이 병목으로 판명되면 (3~10배 목표)
- [ ] mark-sweep GC — arena는 자라기만 함, 무한 스크롤·긴 세션 누수
- [ ] 인스턴스별 메모리 상한 + 협조적 cancel 핸들
- [ ] 루프/조건 안 `yield` (현재 명시 에러)
- [ ] Symbol 실물화 (현재 문자열 페이크 — typeof가 'string')

## P4 — 아키텍처·인프라

- [ ] 레이아웃 러스트 이식 — 피드 수천 노드 + 재레이아웃이 오면 파이썬이 병목
      (파싱/스타일 242→11ms 같은 승리가 한 번 더 남은 곳; 부팅 후 측정하고 결정)
- [ ] 탈 tkinter — tk PhotoImage 스왑 25ms가 잔여 병목 (native 셸 완성도와 연동)
- [ ] P5: C ABI + headless Linux 빌드 + Boa 폴백 제거
      (smoke는 이미 GG_SKIP_NET=1 + xvfb로 리눅스 완주 — 07-19)
- [ ] 디스크 캐시 범위 확대 검토 (현재 명시적 max-age만)
- [ ] HTTP/2

## P5 — 장기 / 별도 프로젝트급

- [ ] 로그인 — 쿠키 보안 속성 전체(HttpOnly/SameSite/Domain/Path), HTTPS 세션
      (세션 쿠키 자 v1은 07-19 가동 — P1 항목)
- [ ] iframe 문서 격리, CORS (광고·로그인·임베드)
- [ ] Web Worker / Service Worker / WebAssembly
- [ ] `<video>` / `<audio>` / WebGL (유튜브·지도류)
- [x] **EUC-KR 등 레거시 인코딩** — 07-19: Content-Type 헤더 우선 +
      헤더 침묵 시 첫 2KB에서 `<meta charset>`/http-equiv 스니핑
      (파이썬 코덱이 euc-kr 계열 전부 처리). 잔여: quirks 모드,
      HTML5 오류 복구 완전판, RTL/양방향
- [ ] AI 통합 (엔진 수준 — 전략 문서의 주차장 항목):
  - [ ] 요약/질문 사이드 패널 (레이아웃 트리 텍스트 추출 기반, 2~3일 규모)
  - [ ] 그레이스풀 디그러데이션 AI — 렌더 실패 감지 → 리더 모드 재구성
  - [ ] 에러 로그 AI 분류 → 체크리스트 갱신 자동화

---

*작성: 2026-07-19 · P0~P5 1차 스프린트 + 네이버 설계 구현 라운드 1
(N1~N4) 반영: 2026-07-19 — 기준 상태: cargo 176종 · smoke 178종
(GG_SKIP_NET=1, xvfb, cp312 휠) · React 18 UMD 부팅·리렌더 E2E ·
합성 네이버 픽스처(3단 주입→피드→클릭) 상시 검증. basket_test와
네이버 원본 번들은 egress 차단으로 이 환경에서 미측정(로컬 검증 항목).*

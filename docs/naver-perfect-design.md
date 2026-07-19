# 네이버 완벽 구동 설계 (v2 — 2026-07-19)

[체크리스트](naver-perfect-checklist.md)가 "무엇이 남았나"의 장부라면,
이 문서는 "어떤 순서로, 어떤 구조로 닫을 것인가"의 설계다.
07-19 스프린트 이후 상태(React 18 UMD 부팅·리렌더 E2E, 컴파일 관문
소진, cargo 174·smoke 170)를 출발점으로 한다.

## 1. 목표 판정 기준 (수치로)

"완벽"은 체크리스트의 A~D 단계를 그대로 쓰되, 수치 판정을 붙인다:

| 단계 | 판정 | 측정 방법 |
|---|---|---|
| A. 껍데기 픽셀 | 크롬 JS-off와 동일 (달성됨) | 골든 픽스처 diff |
| B. 첫 화면 완성 | 뉴스·쇼핑·피드 렌더, **paint 명령 ≥ 1,500 · 텍스트 노드 ≥ 800 · JS 콘솔 에러 0** | 진단 스크립트 스코어보드 |
| C. 사용 가능 | 검색 타이핑→제출→이동, 링크 클릭 이동, 자동완성 표시 | 드라이버 시나리오 3종 |
| D. 크롬급 속도 | **콜드 ≤ 2.5s · 웜 ≤ 0.8s · 스크롤 60fps · 호버 리스타일 ≤ 16ms** | 프로파일 하니스 |

B의 수치 근거: 리더 모드가 271 텍스트/292 paint였다. React 앱이 실제
피드를 그리면 그 5배 이상이 정상 범위다.

## 2. 부팅 체인 — 전체 시퀀스와 구멍의 위치

```
HTML 215KB (EAGER-DATA JSON이 85%)
  → polyfill.js (core-js)      ✅ 완주 (07-19)
  → preload.js (jQuery)        ✅ 완주 (07-19)
  → search.js / main.js(React) ◆ u16 이후 미실측 ← 로컬 라운드 입력
  → 웹팩 런타임 가동            ✅ 모듈 엔트리 468+
  → 앱이 <script src> 동적 주입  ◆◆ 주입 노드가 DOM에 들어가고 끝
  → 주입 번들 실행 → React 마운트 (React 자체는 검증됨)
  → EAGER-DATA JSON → 피드 렌더
  → rAF/타이머 루프 → 라이브 UI
```

◆◆가 코드로 확정된 마지막 구조적 구멍이다: `appendChild`는 script
노드를 트리에 넣지만(vm.rs), 그것을 페치·실행하는 주체가 없다.
fetch()는 pump가 Python에 위임하는 채널이 있지만 **script 주입은
채널 자체가 없다.**

## 3. 관문별 설계

### N1 — 동적 스크립트 주입 체인 (구조적 구멍, 최우선)

fetch 위임과 같은 패턴으로 만든다:

- **Rust**: `St.pending_scripts: Vec<(u32, String)>` (node idx, src).
  script 요소가 (a) src 속성을 갖고 (b) 문서에 연결되는 순간 큐잉.
  감지 지점은 두 곳 — appendChild/insertBefore류 삽입 시 태그 검사,
  그리고 이미 연결된 script의 `src`/`setAttribute("src")` 쓰기.
  inline 텍스트만 있는 주입 script는 삽입 즉시 실행 큐에.
  `Doc.pump`의 반환에 `(fetch_id, url)`과 나란히 script 목록 추가
  (기존 튜플 시그니처 유지 위해 별도 메서드 `Doc.take_pending_scripts()`
  로 분리 — 구 휠 호환은 hasattr 가드).
- **Python** (`native.settle_async`): 루프 안에서
  `take_pending_scripts()` → `net.request_text(resolve(src))` →
  `doc.run_scripts([code])` → onload 콜백 발화(주입 스크립트의 로드
  체이닝이 이걸 기다린다 — jQuery.getScript 패턴). 실패 시 onerror.
  fetch와 마찬가지로 "실행이 새 주입을 낳는" 재귀를 pump 루프가
  자연 처리.
- **판정**: 합성 픽스처(§5)에서 A.js가 B.js를 주입하고 B가 DOM을
  그리는 2단 체인 + onload 체이닝이 settle 안에 완주.

### N2 — 실행 성능 (부팅했다 ≠ 제때 부팅한다)

측정 우선. 07-16 실측: 웜 로드 1.0s의 93%가 JS. lazy parse/compile로
프런트엔드는 -72% 했으니 남은 것은 **실행 시간**이다.

1. `profile_phases`를 React 번들 기준으로 재실측 (u16 여파 포함 —
   Instr 크기가 커졌다: 캐시 미스 증가 여부를 먼저 확인, 퇴행이면
   Instr 인코딩 재검토가 IC보다 선행)
2. IC 확대: GetProp/SetProp 미스 로그를 상위 빈도부터 —
   폴리필의 `hasOwnProperty`/`toString` 추출 호출과 웹팩 모듈 레코드
   접근이 후보. 목표: 웜 실행 1,003ms → **400ms대**
3. 그래도 미달이면 그때 baseline JIT 착수 (설계 비범위 — jsvm 노트의
   기존 로드맵대로). **JIT를 먼저 짓지 않는다** — 측정이 가리킬 때만.

### N3 — React 리렌더 → 부분 무효화 합류 (라이브 UI의 코어 루프)

현재: 라이브 틱이 DOM 변이를 감지하면 **전체 재빌드**가 기본,
hover/focus만 restyle_diff의 paint-only 경로를 탄다.
React는 setState마다 소수 노드만 바꾼다 — 이 루프의 설계:

- `Doc.tick` 후 dom_version이 오르면: 구조 변이 노드 목록을 Rust가
  수집(`St.mutated_nodes` — appendChild/removeChild/setAttribute/
  textContent 쓰기 지점에서 마킹, 이미 있는 변이 카운터에 노드 id만
  얹는 구조)
- 변이가 서브트리 K개 이하(임계 ~32)면: 해당 서브트리만 재-export
  → 부모 BlockLayout만 재레이아웃(형제 y-시프트는 translate로) →
  디스플레이 리스트 부분 갱신. 초과하면 기존 전체 경로 폴백.
- 판정: 합성 카운터 앱(1초 setState)에서 틱당 재렌더 비용이
  전체 재빌드 대비 5배 이상 절감, 화면 결과 동일(골든 diff).

### N4 — 첫 화면 픽셀 (부팅 후 즉시 문제될 것들)

- **woff2**: 네이버 웹폰트 4종이 실사이트에선 woff2. fontdue는
  디코더가 없으므로 `woff2-patched`/`ttf-parser` 계열 크레이트로
  woff2→ttf 변환층을 FontStore 앞단에 둔다(실패 시 현행 스킵 유지
  — 시스템 폴백 렌더는 이미 무해).
- 스프라이트·이미지: 코드 경로는 있음 — 실사이트 URL 패턴(pstatic
  CDN)과 lazy-load 속성(`data-src`)이 IntersectionObserver 즉시발화
  스텁과 맞물리는지 로컬 라운드에서 확인.
- 광고 SDK 3종(ndpsdk/NBP_CORP/apply): **실행하지 않는 것이 설계다.**
  외부 전역을 요구하는 스크립트는 스킵 목록 유지, 단 스킵이 앱
  부팅을 막지 않는지(전역 존재 검사에 undefined 반환) 확인.

### N5 — 상호작용 3종 시나리오 (C 단계 판정 그 자체)

1. **검색**: 클릭 포커스 → 타이핑(로컬: IME 조합) → 자동완성
   (fetch → 드롭다운 DOM — N1·N3가 전제) → Enter GET 제출 → 이동
2. **클릭 내비**: 뉴스 헤드라인 클릭 → 히트테스트 → 새 문서 로드
   (쿠키 자가 세션 유지 — 07-19 가동)
3. **스크롤**: 60fps는 native 셸 경로로 이미 488fps —
   tkinter 셸은 측정 제외(탈 tkinter 방향 유지), 남은 것은
   스크롤 중 lazy 콘텐츠 로드(IO 스텁 즉시발화라 이미 로드됨 — 확인만)

## 4. 실행 순서 (의존 관계)

```
N1 동적 주입 체인      ← 다른 모든 것의 전제 (구조적 구멍)
  → 로컬 라운드 1: 원본 번들 로그 수집 (죽는 지점 목록)
    → N2 실행 성능 (측정 → IC)     ┐ 병렬 가능
    → N3 부분 무효화 합류           ┘
      → 로컬 라운드 2: B 단계 수치 판정
        → N4 픽셀 (woff2·이미지)
          → N5 시나리오 → C 판정
            → D 수치 측정 → (미달 항목만) JIT/이식 결정
```

## 5. 검증 설계 — egress 제약이 기본값

원본에 접속할 수 없는 환경이 기본이므로, 검증을 두 축으로 분리한다:

- **합성 네이버 픽스처** (`bench/naver-fixture/`, CI 상시):
  원본의 *구조*를 모사한 로컬 페이지 — EAGER-DATA JSON 인라인,
  1190px 고정폭, 웹폰트 @font-face, polyfill→preload→entry 번들
  체인, entry가 React UMD를 **동적 주입**하고 EAGER-DATA를 읽어
  피드를 렌더. 진짜 네이버 코드는 한 줄도 안 들어가지만 부팅
  체인의 모든 관문(N1·N3·EAGER-DATA·픽셀)을 헤드리스로 재현한다.
  smoke의 드라이버 섹션에 시나리오로 편입.
- **로컬 라운드 프로토콜**: 로컬(네이버 접속 가능)에서
  `python basket_test.py` + 진단 스크립트로 (1) 죽는 지점 로그
  (2) 스코어보드 수치 (3) 스크린샷을 수집 → 컨테이너 세션에
  붙여넣기 → 수정 → 다음 라운드. 12라운드 건틀릿과 같은 루프를
  라운드당 로그 왕복 1회로 압축하는 것이 목표.

## 6. 명시적 비범위 (이 문서 기준 — 전체 목표에서는 범위 안)

07-19 목표 재정의로 아래는 [전체 웹 로드맵](roadmap.md)의 T3~T4
소유가 됐다. **이 설계 문서(=T2 마일스톤) 안에서만** 비범위다:

- **로그인** → 로드맵 T3 (쿠키 완전판·오리진/CORS·iframe과 한 묶음)
- 광고 SDK 실행 → 로드맵 상시 트랙 (스킵→샌드박스 실행으로 전환 예정)
- `<video>`, 지도/웹툰 → 로드맵 T4 (미디어 웹)
- tkinter 셸의 60fps (탈 tkinter 방향이 이미 결정 — 이건 계속 비범위)

## 7. 리스크와 대비

| 리스크 | 신호 | 대비 |
|---|---|---|
| u16으로 인터프리터 퇴행 | N2 재실측에서 웜 실행 증가 | Instr 필드 재패킹(dst만 u16 등) 또는 회귀 |
| React 스케줄러가 가상 시계와 어긋남 | 마운트가 settle 안에 안 끝남 | pump 예산 내 강제 플러시(이미 MessageChannel은 setTimeout 0 경유 — 검증됨) |
| 주입 스크립트가 폭주(광고 체인) | settle 타임아웃 빈발 | 주입 깊이·개수 상한 + 도메인 스킵 목록(광고 SDK와 동일 범주) |
| woff2 크레이트가 무겁거나 불안정 | 빌드 시간·패닉 | 기능 플래그로 격리, 실패 시 현행 스킵 |
| 부분 무효화가 조용히 틀림 | 골든 diff 불일치 | 임계 초과·의심 시 전체 재빌드 폴백(조용한 오답 금지 원칙) |

## 8. 구현 라운드 1 결과 (07-19 저녁)

- **N1 완료**: pending_scripts 큐(삽입 3경로 + 노드당 1회 게이트,
  JSON 블록 스킵) → settle/라이브 틱/드라이버 3곳 드레인, 페치 후
  load/error 발화(on* 프로퍼티가 expando 실물이 됨), settle당 50개
  주입 상한. 합성 픽스처의 3단 체인이 헤드리스로 완주.
- **합성 픽스처 가동**: bench/naver-fixture — EAGER-DATA + polyfill
  게이트 + entry가 react/react-dom/app을 동적 주입, 피드 렌더 +
  클릭 리렌더까지 smoke 상시 검증(4체크).
- **N2 측정 완료**: u16 퇴행 없음(순수 parse 9.3ms·컴파일 6.5ms·
  웜 실행 0.46ms — 07-18 기준과 동일). §7 첫 리스크 해소.
  IC 확대는 로컬 라운드의 실번들 미스 로그가 입력.
- **N3 v1 완료**: Document.mutated 기록(6 메서드 + vm 직접 변이
  3곳) → take_mutated/export_subtree → refresh_partial이 ≤32
  서브트리를 제자리 스플라이스(최상위 조상 붕괴, 의심 시 전체
  폴백). 골든 동등성 + 미변이 형제 객체 동일성 검증.
  잔여: 부분 레이아웃(지금은 스플라이스 후 전체 레이아웃),
  원거리 형제 재스타일 지연(문서화된 v1 갭).
- **N4 woff2 완료**: woff2-patched(순수 러스트) 디코드를 FontStore
  앞단에 — 실패 시 기존 ttf 경로 폴스루. Lato 샘플(OFL) E2E,
  webfonts.py가 woff2 소스를 로더블로 승격.
- 남은 관문: **로컬 라운드 1**(원본 번들 로그 — §5 프로토콜),
  N4 이미지 lazy-load 실사이트 확인, N5 시나리오 중 IME.

## 9. 첫 삽 (완료됨 — §8 참조)

N1의 Rust 쪽 (`pending_scripts` 큐 + `take_pending_scripts()`)과
합성 픽스처의 뼈대가 첫 커밋이다. 이 둘이 있으면 나머지 관문은
전부 "픽스처에서 재현 → 수정 → 로컬 라운드로 확증"의 루프를 탄다.

*작성: 2026-07-19. 근거 실측: 체크리스트 07-11~07-19 기록,
appendChild 경로 코드 확인(vm.rs — script 주입 미처리 확정).*

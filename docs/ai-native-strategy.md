# AI 네이티브 엔진 전략 — "에이전트 시대의 브라우저를 처음부터"

2026-07-09. 코드베이스 전수 분석(5개 차원, 파일 단위 근거) 기반의 상업화 전략.
전제: GG는 교육용이 아니라 **판매 가능한 제품**을 목표로 한다. 포지션은 인간용
브라우저(웹 호환성 트레드밀 — Presto·EdgeHTML이 죽은 이유)가 아니라
**AI 에이전트가 대규모로 구동하는 엔진**이다.

(2026-07-13 재구성: 파일이 절단되어 세션 메모리를 원본으로 복원. 파일:라인
인용은 현 리포 기준으로 재검증함.)

## 1. 명제 (thesis)

GG의 구조적 우위는 단 하나이고, 그것은 진짜다: **DOM 노드가 VM 값 그 자체**
(NaN 박스 arena 인덱스, value.rs:17; DOM 네이티브가 `Value::dom_node`를 직접
반환, vm.rs:2162–2195 부근)라는 것, 그리고 엔진이 오케스트레이터(Python)와
**같은 프로세스**에 있다는 것. headless Chromium + Playwright는 모든 DOM
읽기에 두 개의 경계를 지불한다 — V8↔Blink C++ 래퍼 교차와 별도 렌더러
프로세스로의 CDP/JSON 왕복. GG는 이 둘을 모두 지운다.

from-scratch라는 약점이 이 포지션에서는 강점이 된다: 에이전트는 **렌더링**
열화는 관용한다. 그래서 GG는 픽셀 퍼펙트/웹 호환성 트레드밀에서 벗어난다.
단, 이벤트 루프는 건너뛸 수 없다 — fetch/Promise가 돌지 않는 페이지는 DOM이
"열화"가 아니라 **빈 채로 부재**한다. (→ P3에서 해결, §6 진행 로그 참조.)

## 2. 쐐기 (the wedge — 코드에 실재)

- O(1) `getElementById` (dom.rs:49–52, 확인됨 2026-07-13).
- 초기 근거였던 "arena가 Blink 대비 dom_create 3.3x"는 Boa `{__idx}` 경로의
  수치였고, gg-js 대 Chromium 직접 비교는 미측정이었다 → P1 벤치마크가 이를
  프로덕션 gg-js 경로에서 재확인함 (§5).

## 3. 준비도 점수 (2026-07-09, 상업화 기준 /100)

| 차원 | 점수 |
|---|---|
| 시맨틱 DOM 추출 | 25 |
| 자동화 API | 17 |
| 스케일/결정론 | 30 |
| 실제 페이지 JS 커버리지 | 10 |
| 보안 + 라이선스 | 32 |

라이선스가 밝은 지점: 의존성 전부 관용 라이선스(MIT/Apache)이고, gg-js는
Boa를 은퇴시키는 클린룸 소유 가능 IP다.

(점수는 P1–P4 진행 이전의 스냅샷. 시맨틱 추출·자동화 API·JS 커버리지는 이후
작업으로 실질 상승 — §6.)

## 4. 로드맵

- **P1** (2–4주): headless Page 드라이버 + 벤치마크 입증 **[GO/NO-GO 게이트]**
- **P2** (4–8주): 시맨틱 스냅샷(role + name + visible-text + box) + 진짜 셀렉터
- **P3** (8–16주, 가장 깊은 벽돌): async 런타임 — Promise/microtask/
  setTimeout/fetch + 가상 시계. SPA가 실체화되도록.
- **P4** (8–14주): 플릿 안전성 — 실행 연료(fuel), mark-sweep GC, 메모리 상한,
  쿠키/스토리지
- **P5** (6–12주): C ABI + headless Linux 빌드 + Boa 제거

## 5. GO/NO-GO 게이트 — **통과** (2026-07-09)

두 벤치마크 모두 리포 안에서 재현 가능 (bench/run_bench.py,
bench/agent_bench.py — 2026-07-13 확인, 둘 다 존재):

- **엔진 내 DOM** (bench/run_bench.py, gg-js 대 V8+Blink via Edge):
  dom_create_2000 gg-js 3.1ms vs V8 9.4ms = **Blink보다 3.0x 빠름**
  (프로덕션 gg-js 경로에서 확인 — Boa 아님); array_sort 1.3x 빠름; 시작
  0.99ms. 정직한 혼재: get_by_id 1.6x 느림, query_class 4.4x 느림(셀렉터
  매처 미최적화).
- **에이전트 액션 end-to-end** (bench/agent_bench.py — GG 드라이버 in-process
  대 WARM Playwright+Edge, 동일 페이지/추출/클릭): 초기 **GG 54.5ms vs
  Playwright fresh-context 755ms = 13.8x 저렴 → 게이트 통과.** Playwright의
  최저 바닥(페이지 재사용, navigate만, 격리 없음) 대비 2.9x. P2 네이티브
  스냅샷 이후 **GG 23.9ms → fresh-context 28.5x, 바닥 모드 8.5x — 두 모드
  모두 게이트 통과.**
- 명제는 **격리 컨텍스트 대량 시장**(멀티테넌트 플릿, 평가 하네스)에서
  검증됨 — Chromium은 new_context에 액션당 ~500–750ms를 반드시 지불한다.
- 핵심 통찰: GG의 병목은 엔진이 아니라 Python 전체 트리 마샬이었다(액션당
  export() 2–3회). 엔진 내 시작 1ms, DOM 연산 한 자릿수 ms. 드라이버 핫패스
  정리(export 캐시 + 불필요 restyle 생략)만으로 77→54.5ms, P2 네이티브
  스냅샷으로 23.9ms.
- Playwright는 설치된 Edge를 구동(channel="msedge", 브라우저 다운로드 없음).

## 6. 진행 로그

**P1 완료 (2026-07-09):** browser/driver.py — GUI 없는 headless `Page` 클래스
(goto/query/query_all/text/attr/click/evaluate/run/snapshot/links), 현재 휠
위의 순수 Python, Rust 변경 없음. 셀렉터 해석은 **엔진의 진짜**
querySelectorAll을 재사용(두 번째 셀렉터 구현이 표류할 일 없음). evaluate()는
console.log(TOKEN + JSON.stringify(expr))로 구조화 읽기. 두 엔진 모두 동작
(engine="boa"|"ggjs"); live example.com 추출, JS 렌더 콘텐츠 실체화,
클릭→onclick, 에이전트 스냅샷까지 end-to-end 검증. smoke_test.py에 드라이버
체크 5개 추가.

**P2 완료 (2026-07-09):** Rust ggcore 메서드 2개 추가 (lib.rs):
`Doc.snapshot()` — 시맨틱 노드별 (ridx, role, tag, accessible-name, href,
type, id, interactive) + script/style/template/noscript와 display:none을
건너뛰는 visible-text; `Doc.query(selector, first)` —
css::parse_selector_list(엔진의 진짜 매처) 경유. driver.py는 있으면 사용
(`_HAS_NATIVE` 프로브), 구형 휠에서는 export 경로로 폴백. 전체 트리 Python
마샬 제거 → 벤치 54.5→23.9ms. bbox는 아직 스냅샷에 없음(레이아웃 지오메트리가
DOM arena로 표면화되지 않음 — 시각 그라운딩용 후속 과제).

**P3 완료 (2026-07-09) — 가장 깊은 벽돌. SPA가 실체화된다.** run-to-completion
exec_loop를 건드리지 않고 gg-js에 완전한 이벤트 루프 추가: microtask VecDeque
+ 가상 시계 타이머 리스트; Promise는 평범한 Obj + 사이드 arena; `pump` 자유
함수; setTimeout/setInterval/clearTimeout/queueMicrotask/fetch +
Promise.resolve/reject 네이티브. fetch()는 Promise를 반환하고 HTTP를 Python
드라이버에 **위임**(Doc.pump가 pending (id,url) 반환; net.py가 서비스;
Doc.resolve_fetch로 settle). Date.now는 가상 시계 기준 → 결정론적, 절대
잠들지 않음(500ms setTimeout이 벽시계 ~0ms). 드라이버: goto(settle=True),
settle(), wait_for(sel|predicate). end-to-end SPA 테스트로 pump가 하중을
받는 것을 입증; async 경로 포함 비용 게이트 유지(async-SPA 액션 ~10ms).

**P3b 완료 (2026-07-09) — 호스트 객체 + new Promise.** Math(22개 함수 + 상수),
Object.keys/values/entries/assign/freeze, Array.isArray/from, isNaN/isFinite —
전부 단일 `Native::HostFn(u16)` variant로 디스패치. Math.random은 결정론적
시드 xorshift64* — 재현 가능. `new Promise(executor)`: resolve/reject가
일반 Native 호출 경로를 타서 새 콜사이트 match arm이 0개. executor throw는
reject. 참고: Number/String은 호출 가능(coercion)이라 statics
(Number.isInteger 등)는 미설치 — 함수 값이 static prop을 못 얹는 것이 원인.

**P3c 완료 (2026-07-09) — 네이티브 async/await 구문 + Promise.all/race.**
컴파일러가 async fn을 `Promise.resolve().then(...)` 체인으로 **디슈가**
(suspendable VM 없음). 최상위 문장 수준 await만 지원 — 루프/조건문/중첩
표현식 내 await는 codegen에서 안전하게 **에러**(조용히 틀리지 않음);
트랜스파일된 번들은 어차피 .then으로 낮춰져 이 배관을 탄다. Promise.all/race는
JS 프렐류드(page.rs PROMISE_PRELUDE). 미변환 async/await SPA가 end-to-end
실체화 확인.

**P4 시작 (2026-07-09) — 실행 연료.** St.fuel (u64, 기본 80M 명령)을 exec_loop
디스패치마다 감산; 0이면 "script exceeded its instruction budget" 에러.
턴마다 리셋(run_source, pump의 microtask/타이머 콜백, 이벤트 핸들러). 적대적
`while(true){}`/폭주 타이머가 워커를 **절대 못 잠근다** — 죽고, VM은 계속.

**naver 행 수정 (2026-07-09):** layout.py `_layout_positioned` 무한 루프
(absolute 박스 레이아웃이 자기 absolute 자손을 순회 중인 리스트에 추가).
인덱스 드레인 + id-set + 5000 캡으로 수정 → 8초+ 무한 → 1ms. native.py
load_document에 JS 예산(스크립트 간 3초, 400KB 초과 스킵). 참고: GUI 셸은
로드 시 async 이벤트 루프를 아직 안 돌림(P3는 드라이버 우선) — GUI 창에서도
SPA가 렌더되게 하는 소규모 후속 필요.

**커버리지 확장 (2026-07-09 → 07-10): 16/31 → 24/31.** naver 763KB 번들이
연료 안전하게 ~1.2초 실행(무한 행이었음) — 실패 모드는 크래시가 아니라
미지원 구문의 컴파일 에러. 랜딩: `?.` 옵셔널 체이닝(JumpIfNullish + 공유 bail
체인), `??` nullish 병합, 계산된/숫자 객체 키, **디스트럭처링**(객체 rename +
기본값, 배열 hole/`...rest`/기본값, 중첩), 기본값 + 디스트럭처링 **매개변수**,
배열 스프레드 `[...a, b]` → `[].concat(...)` 디슈가. rest 매개변수와 호출
스프레드 `f(...a)`는 깔끔히 에러(VM 가변 인자 확장 필요). Rust 테스트 85개
green (2026-07-10 기준).

**현재 상태 요약 (2026-07-13):** ggjs 경로는 Promise/fetch/타이머/async-await
SPA를 Math/Object/Array와 함께 돌리고, 폭주 루프에 대해 플릿 안전. 드라이버
기본 엔진은 아직 `"boa"` (driver.py:91) — ggjs로 뒤집는 것이 유력 후보.

**다음 벽돌:** P4 계속 — mark-sweep GC(arena는 자라기만 함; 긴 에이전트
세션은 누수), 인스턴스별 메모리 상한, 협조적 cancel 핸들. 그다음 루프 내부
await(CPS 변환), Map/Set/RegExp, 제네릭 thenable, for-await-of, Number/String
statics(호출 가능 함수-객체 필요). P2b: css.rs 셀렉터([attr], >/+/~, :nth) +
폼 .value/.checked + fill()/type(). P5: C ABI + headless Linux + Boa 제거.

## 7. KILL 기준

1. P1 벤치마크에서 에이전트 액션당 end-to-end 비용이 headless
   Chromium+Playwright 대비 **5–10x 이상 저렴하지 않으면**, in-process 해자가
   Chromium의 커버리지 완전성을 못 이긴다. — *결과: 격리 컨텍스트 모드 28.5x,
   바닥 모드 8.5x로 통과 (§5).*
2. 타깃 사이트가 거의 전부 무거운 SPA여서 비어 있지 않은 DOM에 도달하는 것
   자체가 V8+Blink 재구축이 되면, 호환성 트레드밀이 돌아온 것이다. naver급
   중량 포털은 명제가 **좇지 않기로 한** 케이스 — static/SSR + 가벼운 SPA는
   이미 동작하고 이긴다.

둘 중 하나라도 발동하면 → static/SSR 스크래핑 니치로 축소하거나, 기존 엔진
위의 얇은 in-process 추출 레이어로 피벗.

## 8. 최대 리스크

JS 커버리지 (당초 10/100). compile()이 첫 미지원 노드에서 스크립트 전체를
실패시키고, Promise/fetch/이벤트 루프 부재, 모던 구문 거부, 호스트 객체
부재였다. "에이전트는 렌더링 열화를 관용한다"는 "페이지 JS가 안 도는 것"까지
연장되지 **않는다**.

→ P3/P3b/P3c + 커버리지 작업(24/31 구문)으로 실질 완화됐지만, 잔여 갭
(클래스, regex, 루프 내 await, Map/Set)이 리스크의 꼬리로 남아 있다.

# shopsquare(터보팩) 하이드레이션 정지 — 수사 기록 (2차 개정)

naver.com 메인의 쇼핑 박스(`shopsquare.naver.com` iframe, Next.js
app-router + turbopack)가 SSR 탭 7개까지만 그리고 멈춘다. 이 문서는
현재까지의 확정 사실과 열린 앞단을 기록한다.

## 정정 (1차 기록의 오류)

1차 기록의 "조건은 참인데 if 분기가 소실되는 미컴파일" 결론은
**하네스 버그가 만든 허상**이었다. sqpatch 계열 진단 스크립트의
출력부가 `[q]/[reg]/[gate]/error`를 포함한 줄만 인쇄해서, 그 밖의
마커([entry]/[s0]/[pre]/[post]...)가 "실행 안 됨"으로 오독됐다.
GG_TRACE_FN 명령어 트레이스(stderr 직행, 필터 무관)로 재검증한
결과 해당 경로는 전부 실행되고 있었다. **교훈: 진단 하네스의 출력
필터는 마커를 추가할 때마다 함께 갱신하거나, 아예 필터 없이 찍어라.**

## 이후 실제로 찾아 고친 것 (bc3b53b~ 이 커밋)

- `el.attributes` 라이브 NamedNodeMap, `getElementsByName`,
  `attachShadow` 강등, `crypto`, 문자열 직접호출 arm 동률 (이전 커밋들).
- **전역 `Promise`가 plain object였다** — `typeof Promise`가
  "object"라 모든 라이브러리 기능 탐지가 폴리필을 강제했고,
  core-js Promise 폴리필의 스케줄러는 이 엔진에서 돌지 않아 async
  함수 전체가 첫 await에서 무음 정지했다. `Native::PromiseCtor`로
  실제 생성자 함수가 됐고(동적 `new t(exec)` 지원), 스태틱은
  fn_props, prototype에는 then/catch/finally 위임을 얹었다.
- `String(fn)` 변환이 "function"만 줘서 `/native code/` 검사가
  깨졌다 — Function.prototype.toString 메서드와 문자열을 통일.
- `Function.prototype.toString`이 브랜드 toString으로 새서
  "[object Function]"을 주던 것을 프렐류드에서 추출형으로 고정.
- UA에 Chrome/122 토큰 추가(net.py + navigator.userAgent) —
  core-js V8_VERSION 스니프가 서브클래싱 프로브(우리가 통과 못 함)를
  건너뛰게 한다.
- 바운디드 마이크로태스크 드레인이 소진 시 잡을 하나 삼키던
  오프바이원 수정(pop 전에 예산 검사).
- 진단 인프라: `zz_dump_bytecode` 테스트(GG_DUMP_SRC/GG_DUMP_PAT —
  바이트코드 현미경 + 실바이트 구동 하네스), `GG_TRACE_FN`(함수명
  매칭 명령어 트레이스), err/type_err의 `[gg-raise]` 트레이스.

## 현재 확정 상태 (GG_TRACE_FN + 무필터 로그 기준)

- registerChunk 15회 활성화 전부 완주. 게이트 await 해소,
  포스트-어웨이트 연속체(m177 p1) 실행, **엔트리 모듈 21479 평가
  성공**(팩토리-부재 에러 없음).
- 그럼에도 DOM은 lis=7 그대로: 21479(등록 모듈)가 요구하는
  app-index 부트(98028/37505 require 체인) 안 어딘가에서 무음
  탈락. `new ReadableStream`이 생성되지 않는다(bare 재바인딩
  래퍼로 확인 — 전역 함수 재바인딩은 스크립트 경계를 넘어
  유효함, 단 DOM 메서드는 expando 호출 경로로만).
- 벤치: 잡음 raise는 12건 전부 무해한 기능 프로브(core-js Set
  메서드 탐지 Cf ×7, canParse/Reflect 프로브 등).

## 다음 지렛대

1. GG_TRACE_FN을 app-index 쪽 함수명(예: hydrate/appBootstrap의
   축약명 — 21479가 require하는 98028 모듈의 proto 이름을
   zz_dump_bytecode로 먼저 알아낸다)에 걸어 어느 문장까지 가는지
   본다. 필터 없이.
2. 후보: `document.readyState` 'loading' 분기에서 DOMContentLoaded
   대기 → 프레임 라이프사이클과의 타이밍; RSC 스트림 마감(C)
   미도달; hydrateRoot 진입 전 조건 분기.
3. recoshopping(webpack)은 같은 페이지에서 정상 렌더 — 대조군.

## 도구 사용법

- 바이트코드: `GG_DUMP_SRC=<js파일> GG_DUMP_PAT=<이름조각>
  cargo test zz_dump_bytecode -- --nocapture`
- 명령어 트레이스: `GG_TRACE_FN=<이름조각>` (페이지 로드 하네스에
  같이) — stderr로 `[gg-fn] m.. p.. name ip..: Instr` 나옴.
- raise 트레이스: `GG_JS_TRACE=1` — `[gg-raise] Kind: msg` +
  미장식 호출 실패에는 `[at m.. p.. ip.. in name]`.
- 런타임 텍스트 패치: 백엔드 인스턴스 `request`를 감싸 특정 URL
  본문을 치환(반환 튜플 모양 보존). for-of 헤드에 콤마식 주입 금지
  (SyntaxError).

# shopsquare(터보팩) 하이드레이션 정지 — 수사 기록

naver.com 메인의 쇼핑 박스(`shopsquare.naver.com` iframe, Next.js
app-router + turbopack)가 SSR 탭 7개까지만 그리고 조용히 멈춘다.
같은 구조의 recoshopping(webpack)은 정상 렌더된다. 이 문서는 어디까지
좁혀졌는지, 무엇이 이미 반증됐는지의 기록이다. 재수사 시 여기서
시작할 것.

## 확정된 사실 (계측 근거)

터보팩 런타임(`turbopack-*.js`)을 네트워크 계층에서 텍스트 패치해
등록/게이트/엔트리 각 단계에 로그를 심는 하네스로 얻은 단일 런 관측:

1. 청크 15개 전부 실행되고, 전부 `registerChunk`에 올바른 경로
   (`static/chunks/….js` — `document.currentScript.getAttribute("src")`
   에서 하드코딩된 CDN 프리픽스를 벗겨 파생)로 등록된다.
2. 런타임 청크의 게이트
   `await Promise.all(otherChunks.map(t=>P(0,e,t)))`가 **통과한다**
   (4개 의존 청크의 W 엔트리 전부 resolve, `[gate] passed` 출력).
3. 게이트 직후 같은 문장의 주입 로그가
   `ids=[21479] len=1 cond=true`를 출력한다. 즉 조건식은 참이다.
4. 그런데 바로 다음의 `for (let r of t.runtimeModuleIds)
   !function(e,t){…}(e,r)` 가 **한 번도 돌지 않는다** — IIFE 첫 줄에
   심은 로그가 침묵한다. 에러도, unhandled rejection도 없다.
5. 엔트리 모듈 21479(= Next.js app-index 부트: `__next_f` 소비자,
   flight ReadableStream 생성, hydrateRoot)가 평가되지 않으므로
   리스너 0개, 플라이트 행 8개가 raw로 잔류, 렌더 없음 — 관측된
   증상 전부가 이 한 지점에서 설명된다.

즉 **조건이 참으로 평가된 if의 결과절(for-of)이 실행되지 않는
엔진 미컴파일**이 실물 페이지에서 재현된다. 위치는 async 메서드
`registerChunk(e,t)` 내부, await가 if-조건의 콤마열 안에 있는 지점.

## 반증된 가설 (전부 격리 재현 통과)

- 1인자 `setTimeout(f)` 미스케줄 — 정상.
- 배열 `push` 오버라이드 무시 / `length=0` 절단 — 다중 스크립트에
  걸친 실제 시퀀스 포함 전부 정상.
- `globalThis` 부재/이상 — `globalThis === window === self` 정상.
- await를 품은 if-콤마-조건 + for-let-of + IIFE — 실물 소스를 문자
  그대로 복사한 재현(스텁 W/S/P/L, 지연 resolve, 동시 활성화 15회,
  바깥 `let e`/`let t`를 파라미터가 섀도잉하는 IIFE 클로저 포함)이
  **전부 통과한다**. 작은 재현으로는 안 터진다.
- `pushOverridden` 관측은 내장 메서드 identity 버그(`a.push !==
  a.push`, bc3b53b에서 수정)가 만든 유령이었다.

## 유력한 다음 지렛대

작은 재현과 실물의 남은 차이는 **컴파일 경로**다. 실물 런타임은
~10KB 비동기 아닌 화살표 IIFE 안에 있고, 그 본문은 `try_lazy_body`
→ `compile_lazy`(지연 세션, 캡처 환경 재구성) 경로를 탈 수 있다.
지연 세션 안에서 async 메서드의 연속체(스필/호이스트)가 어긋나면
정확히 "조건은 참인데 분기 소실" 류의 미컴파일이 된다.

다음 세션 할 일:

1. 실물 `turbopack-*.js`를 엔진에 단독 로드해 `registerChunk`가
   속한 proto의 **바이트코드를 덤프**하고(연료 소진 시 disasm을
   찍는 기존 디버그 경로 재활용), await 이후 연속체에서 for-of
   분기가 어디로 컴파일됐는지 읽는다.
2. 대조: 같은 함수를 지연 컴파일이 **안** 걸리는 형태(본문 축소)로
   컴파일해 바이트코드를 비교한다.
3. 미컴파일 지점을 고치고, 실패 형태를 cargo 테스트로 고정한다.

## 하네스 사용법

네트워크 계층 텍스트 패치는 세션 백엔드 인스턴스의 `request`를
감싸면 된다(반환 튜플 모양을 보존할 것 — 리스트로 받아 body만
바꿔 되돌려준다). 터보팩 소스의 안정된 앵커 문자열:

- 등록: `e={async registerChunk(e,t){`
- 게이트: `if(await Promise.all(t.otherChunks.map(t=>P(0,e,t))),`
- 엔트리 루프: `for(let r of t.runtimeModuleIds)!function(e,t){`

주의: for-of 헤드에 콤마식을 주입하면 실JS에서도 SyntaxError다.
계측은 문장 위치에만 넣을 것 — 이번 수사에서 잘못된 주입이
증거를 한 차례 오염시켰다.

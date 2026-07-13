// GG 브라우저 JS 엔진 벤치마크 — 순수 연산. 어느 엔진에서든 동일하게 실행된다.
// 출력 형식: "BENCH <이름> <ms>" (console.log 한 줄씩)

var now = (typeof performance !== "undefined" && performance.now)
    ? function () { return performance.now(); }
    : function () { return Date.now(); };

function bench(name, fn) {
    var t0 = now();
    var r = fn();
    var t1 = now();
    console.log("BENCH " + name + " " + (t1 - t0).toFixed(2));
    return r;
}

// 1. 재귀 호출 — 함수 호출 오버헤드 + 산술
bench("fib26", function () {
    function fib(n) { return n < 2 ? n : fib(n - 1) + fib(n - 2); }
    return fib(26);
});

// 2. 정수 핫루프 — 인터프리터 디스패치 비용이 그대로 드러난다
bench("loop_sum_3e6", function () {
    var s = 0;
    for (var i = 0; i < 3000000; i++) s += i;
    return s;
});

// 3. 배열 생성 + 비교 함수 정렬
bench("array_sort_5e4", function () {
    var a = [];
    for (var i = 0; i < 50000; i++) a.push((i * 2654435761) % 100000);
    a.sort(function (x, y) { return x - y; });
    return a[0];
});

// 4. 문자열 조립 — 문자열 표현(rope/버퍼) 전략이 승부처
bench("string_build_3e4", function () {
    var s = "";
    for (var i = 0; i < 30000; i++) s += "x";
    return s.length;
});

// 5. 객체 프로퍼티 읽기/쓰기 — 셰이프(히든 클래스) + 인라인 캐시가 승부처
bench("object_churn_2e5", function () {
    var o = { x: 0, y: 1, z: 2 };
    var t = 0;
    for (var i = 0; i < 200000; i++) {
        o.x = i;
        t += o.x + o.y + o.z;
    }
    return t;
});

console.log("BENCH_DONE");

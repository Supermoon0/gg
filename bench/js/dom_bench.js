// GG 브라우저 DOM 벤치마크 — 우리 엔진과 실제 브라우저(Edge/Blink) 양쪽에서
// 동일하게 실행된다. 우리 DOM 바인딩이 지원하는 API만 사용할 것.
// 출력 형식: "BENCH <이름> <ms>"

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

// 1. 노드 생성 + textContent + 트리 삽입
bench("dom_create_2000", function () {
    var body = document.body;
    for (var i = 0; i < 2000; i++) {
        var d = document.createElement("div");
        d.textContent = "node " + i;
        body.appendChild(d);
    }
    return 0;
});

// 2. id 검색 — 트리 순회/인덱스 성능
bench("dom_get_by_id_5000", function () {
    var probe = document.createElement("span");
    probe.setAttribute("id", "bench-probe");
    probe.textContent = "probe";
    document.body.appendChild(probe);
    var hits = 0;
    for (var i = 0; i < 5000; i++) {
        if (document.getElementById("bench-probe")) hits++;
    }
    return hits;
});

// 3. 클래스 셀렉터 질의 — CSS 매처 경유 경로
bench("dom_query_class_300", function () {
    for (var i = 0; i < 50; i++) {
        var d = document.createElement("div");
        d.setAttribute("class", "bench-item");
        document.body.appendChild(d);
    }
    var total = 0;
    for (var j = 0; j < 300; j++) {
        total += document.querySelectorAll(".bench-item").length;
    }
    return total;
});

console.log("BENCH_DONE");

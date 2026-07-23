"""Built-in about: pages."""

import base64
import struct
import zlib


def _demo_png():
    """Generate a small gradient/checker PNG (stdlib only)."""
    w, h = 96, 64
    rows = b""
    for y in range(h):
        rows += b"\x00"
        for x in range(w):
            r, g, b = int(255 * x / w), int(255 * y / h), 160
            if (x // 8 + y // 8) % 2 == 0:
                r, g, b = 255 - r, 255 - g, 90
            rows += bytes((r, g, b))

    def chunk(tag, data):
        core = struct.pack(">I", len(data)) + tag + data
        return core + struct.pack(">I", zlib.crc32(tag + data))

    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(rows))
    png += chunk(b"IEND", b"")
    return "data:image/png;base64," + base64.b64encode(png).decode()


DEMO_IMG = _demo_png()

HOME_PAGE = """<!doctype html>
<html>
<head><title>GG Browser</title>
<style>
  body { background-color: white; }
  .hero { text-align: center; margin-top: 40px; margin-bottom: 8px; }
  .tagline { text-align: center; color: #666666; margin-top: 0px; }
  .card { margin: 24px; }
  .muted { color: #888888; font-size: 13px; }
</style>
</head>
<body>
  <h1 class="hero">GG Browser</h1>
  <p class="tagline">밑바닥부터 직접 만든 브라우저 엔진</p>
  <hr>
  <div class="card">
    <h2>가볼 만한 곳</h2>
    <ul>
      <li><a href="https://example.com">example.com</a> &mdash; 가장 간단한 테스트 페이지</li>
      <li><a href="https://browser.engineering">browser.engineering</a> &mdash; 브라우저 엔진 교과서</li>
      <li><a href="https://motherfuckingwebsite.com">motherfuckingwebsite.com</a> &mdash; 순수 HTML 페이지</li>
      <li><a href="https://text.npr.org">text.npr.org</a> &mdash; 텍스트 뉴스</li>
      <li><a href="about:demo">about:demo</a> &mdash; 이 엔진의 기능 데모</li>
      <li><a href="about:js">about:js</a> &mdash; JavaScript 데모 (gg-js 엔진)</li>
    </ul>
  </div>
  <div class="card">
    <h2>지금 동작하는 것</h2>
    <ul>
      <li>HTTP / HTTPS + 리다이렉트 + gzip + chunked 전송</li>
      <li>HTML 파서 (주석, 엔티티, 암묵적 태그)</li>
      <li>CSS 쿼스케이드: 셀렉터, 명시도, 상속, style 속성</li>
      <li>블록 / 인라인 레이아웃, 줄바꿈, 글꼴 스타일</li>
      <li>링크 클릭, 뒤로/앞으로, 스크롤</li>
    </ul>
  </div>
  <p class="muted">주소창에 URL을 입력하거나 위 링크를 클릭해 보세요.</p>
</body>
</html>
"""

DEMO_PAGE = """<!doctype html>
<html>
<head><title>엔진 기능 데모</title>
<style>
  h1 { color: #2b5aa6; }
  .notice { background-color: #fff3cd; color: #664d03; margin: 16px; }
  #special { color: green; font-weight: bold; }
  blockquote i { color: purple; }
</style>
</head>
<body>
  <h1>GG 엔진 렌더링 데모</h1>
  <p>이 페이지는 외부 라이브러리 없이 밑바닥부터 구현한
     엔진이 그리고 있습니다.</p>

  <h2>텍스트 스타일</h2>
  <p>일반 텍스트, <b>굵게</b>, <i>기울임</i>,
     <b><i>굵은 기울임</i></b>,
     <span style="color: red">인라인 style 속성</span>,
     <code>코드 글꼴</code>,
     <small>작은 글씨</small>, <big>큰 글씨</big>,
     그리고 <a href="about:blank">링크</a>.</p>

  <h2>CSS 셀렉터</h2>
  <p class="notice">클래스 셀렉터로 색을 입힌 공지 박스입니다.</p>
  <p id="special">ID 셀렉터로 스타일된 문단.</p>
  <blockquote>인용문 안의 <i>기울임</i>은
    자손 셀렉터(blockquote i)로 보라색이 됩니다.</blockquote>

  <h2>목록과 구분선</h2>
  <ul>
    <li>첫 번째 항목</li>
    <li>두 번째 항목 &mdash; 엔티티도 됩니다: &lt;tag&gt; &amp; &copy;</li>
    <li>세 번째 항목</li>
  </ul>
  <hr>

  <h2>이미지</h2>
  <p>data: URL로 내장된 PNG를 러스트 디코더가 그립니다 &mdash;
     원본 크기와 확대(바이리니어) 두 가지:</p>
  <p><img src="{DEMO_IMG}">
     <img src="{DEMO_IMG}" width="192">
     텍스트와 이미지가 같은 줄에 베이스라인 정렬됩니다.</p>

  <h2>코드 블록</h2>
  <pre>def hello():
    print("Hello, GG Browser!")
    return 42</pre>

  <center>가운데 정렬된 텍스트입니다.</center>
  <p><a href="about:home">&larr; 홈으로</a></p>
</body>
</html>
"""

DEMO_PAGE = DEMO_PAGE.replace("{DEMO_IMG}", DEMO_IMG)

JS_PAGE = """<!doctype html>
<html>
<head><title>JS 데모</title>
<style>
  .btn { background-color: #2b5aa6; color: white; font-weight: bold;
         margin: 8px; }
  #count { color: #2b5aa6; font-size: 24px; font-weight: bold; }
  .log { background-color: #f2f2f2; font-family: monospace; }
</style>
</head>
<body>
  <h1>JavaScript 데모</h1>
  <p>이 페이지의 스크립트는 직접 만든 gg-js 엔진이
     우리 DOM 위에서 직접 실행합니다.</p>

  <h2>카운터</h2>
  <p>클릭 횟수: <span id="count">0</span></p>
  <p class="btn" onclick="increment()">[ +1 증가 ]</p>
  <p class="btn" onclick="reset()">[ 리셋 ]</p>

  <h2>DOM 조작</h2>
  <p class="btn" onclick="addItem()">[ 목록에 항목 추가 ]</p>
  <ul id="list">
    <li>처음부터 있던 항목</li>
  </ul>

  <h2>innerHTML</h2>
  <p class="btn" onclick="swap()">[ 아래 내용을 마크업으로 교체 ]</p>
  <p id="target">원래 텍스트입니다.</p>

  <h2>querySelector + addEventListener</h2>
  <p class="btn" id="evbtn">[ addEventListener로 등록된 버튼 ]</p>
  <p id="evout">아직 클릭 안 함</p>

  <p><a href="about:home">&larr; 홈으로</a></p>

  <script>
    var n = 0;
    function increment() {
      n = n + 1;
      document.getElementById("count").textContent = String(n);
      document.title = "JS 데모 (" + n + ")";
      console.log("count =", n);
    }
    function reset() {
      n = 0;
      document.getElementById("count").textContent = "0";
    }
    var items = 0;
    function addItem() {
      items = items + 1;
      var li = document.createElement("li");
      li.textContent = "JS가 추가한 항목 #" + items;
      document.getElementById("list").appendChild(li);
    }
    function swap() {
      document.getElementById("target").innerHTML =
        "<b>굵은 글씨</b>와 <i style=\\"color: purple\\">보라색 기울임</i>을 " +
        "innerHTML로 넣었습니다.";
    }
    var evClicks = 0;
    document.getElementById("evbtn").addEventListener("click", function() {
      evClicks = evClicks + 1;
      var btns = document.querySelectorAll(".btn");
      document.getElementById("evout").textContent =
        "리스너 실행 " + evClicks + "회 / querySelectorAll('.btn') = "
        + btns.length + "개";
    });
    console.log("페이지 스크립트 로드 완료:",
                document.getElementById("count") !== null);
  </script>
</body>
</html>
"""

BLANK_PAGE = "<html><head><title>about:blank</title></head><body></body></html>"


def about_page(name):
    if name in ("home", "", "welcome"):
        return HOME_PAGE
    if name == "demo":
        return DEMO_PAGE
    if name == "js":
        return JS_PAGE
    if name == "blank":
        return BLANK_PAGE
    return error_page(f"about:{name}", "알 수 없는 about 페이지입니다.")


def error_page(url, message):
    return f"""<!doctype html>
<html>
<head><title>페이지를 열 수 없음</title></head>
<body>
  <h1>페이지를 열 수 없습니다</h1>
  <p><b>{url}</b></p>
  <pre>{message}</pre>
  <p><a href="about:home">홈으로 돌아가기</a></p>
</body>
</html>"""

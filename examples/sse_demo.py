"""SSE streaming demo — simulates AI Agent token-by-token output."""
import time
import threading
from pyreframework import Pyre, PyreStream

app = Pyre()


@app.get("/")
def index(req):
    return """<html><body>
<h1>Pyre SSE Demo</h1>
<pre id="output"></pre>
<script>
const source = new EventSource('/stream');
const output = document.getElementById('output');
source.onmessage = (e) => { output.textContent += e.data; };
source.addEventListener('done', () => {
    output.textContent += '\\n[DONE]';
    source.close();
});
</script>
</body></html>"""


@app.get("/stream", gil=True)
def stream_tokens(req):
    """Simulate LLM streaming: emit tokens one by one."""
    stream = PyreStream()

    def generate():
        tokens = "Hello! I am Pyre, a high-performance Python web framework powered by Rust. ".split()
        for token in tokens:
            stream.send_event(token + " ")
            time.sleep(0.05)  # Simulate LLM token generation delay
        stream.send_event("[DONE]", event="done")
        stream.close()

    # Start generation in background thread
    threading.Thread(target=generate, daemon=True).start()
    return stream


@app.get("/fast")
def fast(req):
    return "fast route still works"


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000)

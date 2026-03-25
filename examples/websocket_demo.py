"""WebSocket demo — echo server + broadcast example."""

from pyreframework import Pyre

app = Pyre()


@app.get("/")
def index(req):
    return """<html><body>
<h1>Pyre WebSocket Demo</h1>
<input id="msg" type="text" placeholder="Type a message...">
<button onclick="send()">Send</button>
<pre id="log"></pre>
<script>
const ws = new WebSocket('ws://127.0.0.1:8000/ws');
const log = document.getElementById('log');
ws.onmessage = (e) => { log.textContent += e.data + '\\n'; };
ws.onopen = () => { log.textContent += '[connected]\\n'; };
ws.onclose = () => { log.textContent += '[disconnected]\\n'; };
function send() {
    const msg = document.getElementById('msg').value;
    ws.send(msg);
    document.getElementById('msg').value = '';
}
</script>
</body></html>"""


@app.websocket("/ws")
def echo(ws):
    """Echo server — sends back whatever the client sends."""
    while True:
        msg = ws.recv()
        if msg is None:
            break
        ws.send(f"echo: {msg}")


if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000)

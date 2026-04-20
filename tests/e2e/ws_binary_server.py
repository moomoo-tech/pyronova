"""WebSocket binary test server."""
from pyreframework import Pyre

app = Pyre()

@app.websocket("/echo")
def echo(ws):
    while True:
        msg = ws.recv_message()
        if msg is None:
            break
        msg_type, data = msg
        if msg_type == "text":
            ws.send(f"echo: {data}")
        elif msg_type == "binary":
            ws.send_bytes(data)

@app.get("/")
def index(req):
    return "ok"

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000)

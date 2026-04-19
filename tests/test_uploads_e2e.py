"""End-to-end multipart file upload through the HTTP stack."""

import json
import os

from tests.conftest import feature_server_factory

SERVER = '''
import os
from pyreframework import Pyre
from pyreframework.uploads import parse_multipart

app = Pyre()

@app.get("/__ping")
def ping(req):
    return "pong"

@app.post("/upload")
def upload(req):
    form = parse_multipart(req)
    return {
        k: {"filename": v.filename, "size": v.size}
        for k, v in form.items()
    }

if __name__ == "__main__":
    app.run(
        host="127.0.0.1",
        port=int(os.environ["PYRE_PORT"]),
        mode=os.environ["PYRE_MODE"],
    )
'''

feature_server = feature_server_factory(SERVER)


def _build_multipart(filename: str, content: bytes) -> tuple[bytes, str]:
    boundary = "----PyreBoundary123"
    body = (
        f"--{boundary}\r\n"
        f'Content-Disposition: form-data; name="file"; filename="{filename}"\r\n'
        f"Content-Type: text/plain\r\n"
        f"\r\n"
    ).encode() + content + f"\r\n--{boundary}--\r\n".encode()
    return body, f"multipart/form-data; boundary={boundary}"


def test_upload_single_file_roundtrip(feature_server):
    body, ct = _build_multipart("test.txt", b"hello world")
    status, resp, _ = feature_server.post(
        "/upload", body=body, headers={"Content-Type": ct},
    )
    assert status == 200
    data = json.loads(resp)
    assert data["file"]["filename"] == "test.txt"
    assert data["file"]["size"] == 11

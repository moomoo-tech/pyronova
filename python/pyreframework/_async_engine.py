"""Async engine script — injected into each sub-interpreter worker.

This script runs inside a sub-interpreter with its own GIL.
It drives a Python asyncio event loop that processes requests
received from Rust via the _pyre_recv/_pyre_send C-FFI bridge.

Template variables (replaced by Rust before exec):
  {worker_idx}     — this worker's index (0..N)
  {handlers_array}  — comma-separated quoted handler names
"""

import asyncio
import threading

# Injected by Rust: WORKER_ID = {worker_idx}
# Injected by Rust: HANDLER_NAMES = [{handlers_array}]


async def _process_request(req_id, handler_idx, method, path, query, body_bytes, headers_json):
    try:
        handler_name = HANDLER_NAMES[int(handler_idx)]
        handler = globals().get(handler_name)
        if handler is None:
            _pyre_send(WORKER_ID, req_id, 500, "text/plain", b"handler not found")
            return

        import json as _json
        headers = _json.loads(headers_json) if headers_json else {}
        req = _PyreRequest(method, path, {}, query, body_bytes, headers)
        res = handler(req)

        if asyncio.iscoroutine(res):
            res = await res

        if isinstance(res, _PyreResponse):
            body = (
                str(res.body).encode("utf-8")
                if not isinstance(res.body, bytes)
                else res.body
            )
            _pyre_send(
                WORKER_ID,
                req_id,
                res.status_code,
                res.content_type or "text/plain",
                body,
            )
        elif isinstance(res, dict):
            import json

            _pyre_send(
                WORKER_ID,
                req_id,
                200,
                "application/json",
                json.dumps(res).encode("utf-8"),
            )
        elif isinstance(res, bytes):
            _pyre_send(WORKER_ID, req_id, 200, "application/octet-stream", res)
        else:
            body = str(res).encode("utf-8")
            ct = (
                "application/json"
                if body.startswith(b"{") or body.startswith(b"[")
                else "text/plain"
            )
            _pyre_send(WORKER_ID, req_id, 200, ct, body)
    except Exception as e:
        _pyre_send(WORKER_ID, req_id, 500, "text/plain", str(e).encode("utf-8"))


def _fetcher_thread(loop):
    while True:
        req_data = _pyre_recv(WORKER_ID)
        if req_data is None:
            break
        req_id, handler_idx, method, path, query, body_bytes, headers_json = req_data
        asyncio.run_coroutine_threadsafe(
            _process_request(req_id, handler_idx, method, path, query, body_bytes, headers_json),
            loop,
        )


async def _pyre_engine():
    loop = asyncio.get_running_loop()
    t = threading.Thread(target=_fetcher_thread, args=(loop,), daemon=False)
    t.start()
    await asyncio.to_thread(t.join)


asyncio.run(_pyre_engine())

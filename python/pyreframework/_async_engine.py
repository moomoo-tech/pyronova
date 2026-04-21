"""Async engine script — injected into each sub-interpreter worker.

This script runs inside a sub-interpreter with its own GIL.
It drives a Python asyncio event loop that processes requests
received from Rust via the _pyre_recv/_pyre_send C-FFI bridge.

Template variables (replaced by Rust before exec):
  {worker_idx}     — this worker's index (0..N)
  {handlers_array}  — comma-separated quoted handler names
"""

import asyncio
import logging
import threading

_log = logging.getLogger("pyreframework.async")

# Prefer orjson for fast JSON serialization (same strategy as Rust side)
try:
    import orjson as _orjson
    def _json_dumps_bytes(obj):
        return _orjson.dumps(obj)
except ImportError:
    import json as _json_mod
    def _json_dumps_bytes(obj):
        return _json_mod.dumps(obj).encode("utf-8")

# Injected by Rust: WORKER_ID = {worker_idx}
# Injected by Rust: HANDLER_NAMES = [{handlers_array}]


# Timeout for async handlers — 2s before Rust's 30s gateway timeout,
# so Python can abort cleanly instead of computing a result nobody wants.
_HANDLER_TIMEOUT = 28


async def _process_request(req_id, handler_idx, method, path, params, query, body_bytes, headers, client_ip):
    try:
        handler_name = HANDLER_NAMES[int(handler_idx)]
        handler = globals().get(handler_name)
        if handler is None:
            _pyre_send(WORKER_ID, _pyre_pool_id, req_id, 500, "text/plain", b"handler not found")
            return

        req = _PyreRequest(method, path, params, query, body_bytes, headers, client_ip)
        res = handler(req)

        if asyncio.iscoroutine(res):
            # Bracket pattern: bound the coroutine's lifetime so cancelled/timed-out
            # requests don't accumulate as phantom load in the event loop.
            res = await asyncio.wait_for(res, timeout=_HANDLER_TIMEOUT)

        if isinstance(res, _PyreResponse):
            body = (
                str(res.body).encode("utf-8")
                if not isinstance(res.body, bytes)
                else res.body
            )
            _pyre_send(
                WORKER_ID,
                _pyre_pool_id,
                req_id,
                res.status_code,
                res.content_type or "text/plain",
                body,
            )
        elif isinstance(res, dict):
            _pyre_send(
                WORKER_ID,
                _pyre_pool_id,
                req_id,
                200,
                "application/json",
                _json_dumps_bytes(res),
            )
        elif isinstance(res, bytes):
            _pyre_send(WORKER_ID, _pyre_pool_id, req_id, 200, "application/octet-stream", res)
        else:
            body = str(res).encode("utf-8")
            ct = (
                "application/json"
                if body.startswith(b"{") or body.startswith(b"[")
                else "text/plain"
            )
            _pyre_send(WORKER_ID, _pyre_pool_id, req_id, 200, ct, body)
    except asyncio.TimeoutError:
        _pyre_send(WORKER_ID, _pyre_pool_id, req_id, 504, "text/plain", b"handler timeout")
    except asyncio.CancelledError:
        # Propagated cancellation — client disconnected or Rust future dropped.
        # Don't send response; the oneshot receiver is already gone.
        pass
    except Exception as e:
        # Log BEFORE sending so the server-side record survives even if
        # _pyre_send itself fails. Returning only `str(e)` to the client
        # was masking real handler bugs — the exception type + traceback
        # are what operators need at 3am, not the reply envelope.
        _log.exception("async handler req_id=%s path=%s raised", req_id, path)
        _pyre_send(WORKER_ID, _pyre_pool_id, req_id, 500, "text/plain", str(e).encode("utf-8"))


def _fetcher_thread(loop):
    while True:
        # The pool_id argument is the zombie-worker guard (see
        # src/interp.rs :: pyre_recv_cfunc). A stale worker whose pool
        # has been replaced will see None here and exit the loop.
        req_data = _pyre_recv(WORKER_ID, _pyre_pool_id)
        if req_data is None:
            break
        req_id, handler_idx, method, path, params, query, body_bytes, headers, client_ip = req_data
        asyncio.run_coroutine_threadsafe(
            _process_request(req_id, handler_idx, method, path, params, query, body_bytes, headers, client_ip),
            loop,
        )


async def _pyre_engine():
    loop = asyncio.get_running_loop()
    t = threading.Thread(target=_fetcher_thread, args=(loop,), daemon=False)
    t.start()
    await asyncio.to_thread(t.join)


asyncio.run(_pyre_engine())

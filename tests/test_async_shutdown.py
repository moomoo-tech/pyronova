"""Regression for the graceful-shutdown bug (benchmark-17 audit bug #3).

Before the fix, the async-worker's `_pyronova_engine` exited the moment the
fetcher thread returned, with pending asyncio tasks still in flight.
`Py_EndInterpreter` then tore the VM down mid-task, which could
segfault on ungraceful cleanup of coroutine frames and left orphan
sockets for anything using asyncio-driven I/O (asyncpg, aiohttp, etc.).

Now `_pyronova_engine`'s `finally` branch cancels every pending task,
gathers them with `return_exceptions=True`, and runs
`loop.shutdown_asyncgens()` before the caller re-enters Rust.

Structural test: verify the cancel/gather/shutdown triple is present.
A full runtime test is hard because it needs a real async-worker
tear-down path under controlled timing; the structural assertion
stops the fix from silently regressing.
"""

import pathlib

_REPO = pathlib.Path(__file__).parent.parent


def test_async_engine_graceful_shutdown_present():
    src = (_REPO / "python/pyronova/_async_engine.py").read_text()
    assert "asyncio.all_tasks(loop)" in src, (
        "_pyronova_engine's finally block must enumerate pending tasks to cancel"
    )
    assert "task.cancel()" in src
    assert "asyncio.gather" in src and "return_exceptions=True" in src, (
        "cancellations must be drained (gather with return_exceptions) "
        "before Py_EndInterpreter runs — otherwise pending coroutines "
        "get collected by CPython's emergency GC and can SIGSEGV"
    )
    assert "loop.shutdown_asyncgens()" in src, (
        "async generators (asyncpg cursor-style APIs etc.) must be "
        "closed explicitly; otherwise their __aexit__ clauses are never "
        "called and their connection pools leak"
    )
    # The shutdown has to be in a `finally:` branch so it runs even when
    # the fetcher thread joins normally (not just on exception).
    # Sanity: find `finally:` after `await asyncio.to_thread`.
    idx_join = src.find("await asyncio.to_thread(t.join)")
    assert idx_join != -1
    finally_idx = src.find("finally:", idx_join)
    assert finally_idx != -1, "shutdown must be in a finally: block"

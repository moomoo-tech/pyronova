# Advisor audit triage — 2026-04-19

Over a single session an external advisor ran 7 "Haskell-style formal
verification" sweeps over Pyre's source and produced 31 claimed bugs.
This document tracks what's **still open** — the items we accepted but
haven't fixed, and the items we rejected with reasons.

Fixed items are intentionally not listed here; look at git log for
commits between `d4bce1c` and `1e39a86` (plus the graceful-shutdown
commit following this doc update) for the full record.

## Open — accepted, deferred

| # | Area | Reason it's not fixed yet |
|---|---|---|
| 9 | GIL-mode SSE: `run_until_complete` halts per-thread event loop, background tasks (`asyncio.create_task`) freeze after handler returns | Real. Fix needs a dedicated long-running asyncio thread pool (run_forever) plus a coroutine-dispatch primitive. Non-trivial refactor across `handlers.rs` + `_async_engine.py` |
| 15 | `impl Clone for PyreRequest` resets `OnceLock<headers_cache>` → middleware chain re-parses headers | Real, impact unclear. `LazyHeaders::Converted` already caches the parsed form; the reset only costs re-parse on `Raw`. **Benchmark before fix.** If >2x per-request overhead under realistic hook chain, move to `Arc<OnceLock<_>>`. Otherwise close as documented tradeoff |
| 16 | `query_params` returns `HashMap<String, String>` — HPP loses duplicate keys | Real but API-breaking. **Proposed plan:** add `query_params_all() -> HashMap<String, Vec<String>>` additively; keep `query_params` as last-wins. Needs user-facing decision |
| 17 | `json.rs` `PyMapping.items()` called during recursive serialize of parent `PyDict` → outer `PyDict_Next` can observe a resized root dict | Real but narrow. Requires a hostile user Mapping subclass that mutates an ancestor dict during `.items()`. Fix = materialize the outer dict's pairs into a `Vec` before recursing. For stdlib dict values there is no risk |
| 21 | `response_map` passive cleanup fails if a worker deadlocks — orphaned oneshot Senders accumulate until OOM | Real. Needs a background sweeper task running every N seconds, iterating each worker's `response_map`, dropping entries whose `tx.is_closed()` returns true |
| 23 | `stream.rs::send` raising `BlockingIOError` on channel full → Python user loop spin-locks the GIL thread | Real but fixing changes the public API (sync send → async send or auto-backoff). Needs product decision |
| 24 | `pyre_send_cfunc` deep-copies body via `slice.to_vec()` — 50 MB responses allocate twice | Real perf concern. Correct fix = PyBuffer-based zero-copy with refcount-deferred DECREF after hyper write completes. Big refactor |
| 30 | (duplicate of #9) | — |
| 31 | Sub-interp `_async_engine.py` has no `isinstance(res, _PyreStream)` branch — streaming handlers fall into `str(res)` | Real architectural gap: the `_pyre_send` bridge has no streaming API. Document as "streaming in sub-interp mode not yet supported"; integrate with #9's redesign |

## Rejected

| # | Claim | Why rejected |
|---|---|---|
| 5 | WS handler: `Python::attach` in a spawned OS thread crosses PEP 684 boundaries into a sub-interp | False by design. WS handlers are registered in the main interpreter (`@app.websocket(...)` runs at import time in main). `Python::attach` on a fresh OS thread binds to main's tstate — matches. No sub-interp ever reached |
| 12 | `_PyreRequest` must set `Py_TPFLAGS_HAVE_GC` + `tp_traverse` + `tp_clear` per CPython gh-116946 | Empirically refuted. Enabling these made per-request leak jump from 63 B/req to 313 B/req under 400k rps sustained load. gh-116946's concern is finalize-time cycle collection; our workload has no cycles. Documented in `src/pyre_request_type.rs` header |
| 13 | `PyDict_Clear` saves 54 B/req because `dict_dealloc` defers items via `Py_TRASHCAN` | False. CPython 3.14.4 `Objects/dictobject.c::dict_dealloc` does not use `Py_TRASHCAN_BEGIN/END` — it uses `_PyObject_ResurrectStart/End` for dict-event watchers. `dict_dealloc` and `PyDict_Clear` both funnel through `dictkeys_decref` and are semantically equivalent. The measured 54 B/req gap is within run-to-run noise (±15 B/req) |
| — | `for p in args_arr { Py_DECREF(*p) }` in `build_request` — double-free | Rejected as stale-code. That loop existed in `d67a988` (Route A). When we moved to Route B (`d4bce1c`), `build_request` was rewritten to transfer owned refs via `PyObjRef::into_raw()` into `alloc_and_init`. No double-free in current source |
| — | `_pyre_recv` blocks the asyncio event loop | False. `_async_engine.py:113` runs `_pyre_recv` on a dedicated `_fetcher_thread`; the event loop uses `asyncio.run_coroutine_threadsafe` to schedule handlers back. The event loop is never blocked |
| 18 | Sync handler run inside `_process_request` blocks the asyncio event loop | False. `app.rs:359` gates registration with `inspect.iscoroutinefunction`; only async handlers are routed to the async pool. Sync handlers run in a separate sync pool. The hypothetical mis-routing the advisor described cannot occur |

## Meta-observations on the advisor

68% signal rate (21/31 real) is useful but not self-certifying.
Every claim must be:
1. Grepped against the *current* source (advisor repeatedly cited
   stale code from pre-`d4bce1c` Route A).
2. Cross-referenced with CPython source when the claim depends on
   CPython internals (gh-116946 and the Trashcan theory were both
   refuted by reading `Include/internal/pycore_freelist.h` and
   `Objects/dictobject.c` directly).
3. Measured when the claim is performance- or memory-shaped
   (HAVE_GC made things measurably worse, allocator swap was null).

Pattern-matching analysis at the advisor's level catches the
*shape* of a class-of-bug but cannot replace the "what does the
code actually do today, and what does reality measure" step.

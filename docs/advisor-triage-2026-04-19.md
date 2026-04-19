# Advisor audit triage — 2026-04-19

Over a single session an external advisor ran repeated "Haskell-style
formal verification" passes over Pyre's source and produced 16 claimed
bugs. This document records the disposition of each so future readers
don't rewalk the same ground.

## Summary

- 13 real bugs — 12 fixed, 2 deferred (SSE deadlock, json reentrancy)
- 6 rejected — claim did not match current source or was empirically
  refuted

## Accepted and fixed (committed)

| # | Area | Fix commit | Notes |
|---|---|---|---|
| 1 | `parse_sky_response`: `PyDict_Next` + `PyObject_Str` re-entrancy | `d1e60df` | Snapshot borrowed k/v refs before `PyObject_Str` — user `__str__` can't corrupt the iterator mid-walk |
| 2 | `py_str_dict` leaves `MemoryError` set on py_str OOM | `d1e60df` | Explicit `PyErr_Clear()` in every `None`-return path |
| 3 | `PyDict_SetItem` return value ignored | `d1e60df` | Check for `-1`, clear, return None |
| 4 | `PyObject_IsInstance == 1` — `-1` (error) path missing | `d1e60df` | Match on `{1, 0, -1}`, clear on -1, fall through to duck-type |
| 6 | WS handler: `Py<PyAny>` drops after `Python::attach` scope | `d1e60df` | Explicit `drop(handler)` inside `attach` |
| 7 | WS handler: `async def` coroutine silently dropped | `d1e60df` | Detect via `asyncio.iscoroutine`, drive via `asyncio.run` |
| 8 | `LoopGuard` drop after Py_Finalize segfaults | `4326c77` | `std::mem::forget(loop_obj)` on dead interp — OS reclaims |
| 10 | Hyper serve_connection aborted by Tokio runtime drop — TCP RST on shutdown | `4326c77` | Per-connection `tokio::select!` on `cancelled()`, call `graceful_shutdown()` to drain in-flight |
| 11 | Python subclass inheriting Rust `_PyreRequest` → `subtype_dealloc` bypasses `tp_dealloc` | `d4bce1c` | Strip `Py_TPFLAGS_BASETYPE`; attach helpers via monkey-patch on the heap type |
| 14 | `static_fs.rs` canonicalize → File::open TOCTOU | *fixed in this commit* | `OpenOptions` + `O_NOFOLLOW` — refuses symlink swap between containment check and open |
| 19 | `spawn_rss_sampler` JoinHandle dropped, thread races `Py_Finalize` | *fixed in this commit* | Save handle in `APP_RSS_HANDLE`, join on shutdown |

## Accepted, deferred

| # | Area | Reason |
|---|---|---|
| 9 | SSE deadlock — `run_until_complete` halts per-thread event loop, background tasks (`asyncio.create_task(...)`) freeze after handler returns | Real. Fix needs a dedicated long-running asyncio thread pool (run_forever) plus a coroutine-dispatch primitive. Non-trivial refactor across `handlers.rs` + `_async_engine.py`. Kept open; tracked in future-work list |
| 15 | `impl Clone for PyreRequest` resets `OnceLock<headers_cache>` → middleware chain re-parses headers | Real, impact unclear. `LazyHeaders::Converted` already caches the parsed form; the reset only costs re-parse on `Raw`. Needs a benchmark under a realistic hook chain before we commit to `Arc<OnceLock<_>>` (which introduces its own sharing semantics) |
| 16 | `query_params` returns `HashMap<String, String>` — HPP loses duplicate keys | Real but API-breaking. Route: add `query_params_all() -> HashMap<String, Vec<String>>` additively in a future release; leave `query_params` as last-wins for backcompat. Discuss with users before shipping |
| 17 | `json.rs` `PyMapping.items()` called during recursive serialize of parent `PyDict` → outer `PyDict_Next` can observe a resized root dict | Real but narrow. Requires a hostile user Mapping subclass that mutates an ancestor dict during `.items()`. Fix = materialize the outer dict's pairs into a `Vec` before recursing. Low priority; tracked. For the common case (stdlib dict values) there is no risk |

## Rejected

| # | Claim | Why rejected |
|---|---|---|
| 5 | WS handler: `Python::attach` in a spawned OS thread crosses PEP 684 boundaries into a sub-interp | False by design. WS handlers are registered in the main interpreter (`@app.websocket(...)` runs at import time in main). `Python::attach` on a fresh OS thread binds to main's tstate — matches. No sub-interp ever reached |
| 12 | `_PyreRequest` must set `Py_TPFLAGS_HAVE_GC` + `tp_traverse` + `tp_clear` per CPython gh-116946 | Empirically refuted. Enabling these made per-request leak jump from 63 B/req to 313 B/req under 400k rps sustained load. gh-116946's concern is finalize-time cycle collection; our workload has no cycles. Documented in `src/pyre_request_type.rs` header |
| 13 | `PyDict_Clear` saves 54 B/req because `dict_dealloc` defers items via Py_TRASHCAN | False. CPython 3.14.4 `Objects/dictobject.c::dict_dealloc` does not use `Py_TRASHCAN_BEGIN/END` — it uses `_PyObject_ResurrectStart/End` for dict-event watchers. `dict_dealloc` and `PyDict_Clear` both funnel through `dictkeys_decref` and are semantically equivalent. The measured 54 B/req gap is within run-to-run noise (±15 B/req) |
| — | `for p in args_arr { Py_DECREF(*p) }` in `build_request` — double-free | Rejected as stale-code. That loop existed in `d67a988` (Route A). When we moved to Route B (`d4bce1c`), `build_request` was rewritten to transfer owned refs via `PyObjRef::into_raw()` into `alloc_and_init`. No double-free in current source |
| — | `_pyre_recv` blocks the asyncio event loop | False. `_async_engine.py:113` runs `_pyre_recv` on a dedicated `_fetcher_thread`; the event loop uses `asyncio.run_coroutine_threadsafe` to schedule handlers back. The event loop is never blocked |
| 18 | Sync handler run inside `_process_request` blocks the asyncio event loop | False. `app.rs:359` gates registration with `inspect.iscoroutinefunction`; only async handlers are routed to the async pool. Sync handlers run in a separate sync pool. The hypothetical mis-routing the advisor described cannot occur |

## Decision matrix for open items

| Item | Next action | Trigger |
|---|---|---|
| #14 TOCTOU | **Fix now.** `OpenOptions` + `O_NOFOLLOW` + post-open inode-check for containment. Cheap, no API impact, real safety |
| #15 Clone resets cache | **Benchmark before fix.** Write a micro-bench that clones `PyreRequest` N times with `LazyHeaders::Raw` and counts header-dict allocations. If >2x over shared-cache variant, fix with `Arc<OnceLock<_>>`. Otherwise close as "documented tradeoff" |
| #16 `query_params` HPP | **Discuss before fix.** Propose adding `query_params_all()` additively; keep `query_params` as `last-wins`. Decision affects user-facing API — needs sign-off, not unilateral commit |

## Meta-observations on the advisor

75% signal rate (12/16 real) is useful but not self-certifying.
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

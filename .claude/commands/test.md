Run all Pyre tests: Rust compile check + Python unit + integration.

Steps:
1. `cargo test` — Rust compilation check (catches type errors, unused imports, etc.)
2. `source .venv/bin/activate && maturin develop --release` — build Python extension
3. `pytest tests/test_mcp.py tests/test_cookies_unit.py tests/test_testclient.py tests/test_rpc.py tests/test_static_files.py tests/test_websocket.py tests/test_async_isolation.py tests/test_logging.py -v` — pytest unit tests
4. `python tests/test_all_features.py` — integration tests (starts real servers, 22 end-to-end tests)
5. Report a summary table of results (pass/fail counts per stage)

If any test fails, investigate the failure and report what went wrong. Do NOT auto-fix — just report.

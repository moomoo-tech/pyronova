# E2E drivers

Manual-run end-to-end scripts that pytest does **not** collect.

These exercise real client/server pairs (network traffic, WebSocket
handshakes, process lifecycles) that don't fit pytest's collection
model. Run them by hand when touching the relevant codepath.

## ws_binary

Exercises WebSocket text + binary message round-trip.

```bash
# Shell 1 — start the echo server
python tests/e2e/ws_binary_server.py

# Shell 2 — run the client
python tests/e2e/ws_binary_client.py
```

Success = client prints `✅ text`, `✅ binary`, `✅ closed` and exits 0.

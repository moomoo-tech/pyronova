"""WebSocket binary test client."""
import asyncio
import websockets

async def test():
    async with websockets.connect("ws://127.0.0.1:8000/echo") as ws:
        # Text message
        await ws.send("hello")
        r = await ws.recv()
        assert r == "echo: hello", f"text failed: {r}"
        print(f"  ✅ text: {r}")

        # Binary message
        data = bytes([0, 1, 2, 3, 255])
        await ws.send(data)
        r = await ws.recv()
        assert r == data, f"binary failed: {r!r}"
        print(f"  ✅ binary: {len(r)} bytes, content={list(r)}")

        await ws.close()
        print("  ✅ closed")

asyncio.run(test())

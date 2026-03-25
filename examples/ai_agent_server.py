"""
AI Agent Server — demonstrates Pyre for agentic AI applications.

Features used:
  - MCP Server (AI tool discovery)
  - SSE streaming (token-by-token LLM output)
  - async handlers (concurrent LLM calls)
  - SharedState (agent session memory)
  - Pydantic validation
  - CORS (frontend integration)

Run:
  python examples/ai_agent_server.py

Test:
  # Chat endpoint (simulated LLM)
  curl -X POST http://127.0.0.1:8000/chat -H 'Content-Type: application/json' \
    -d '{"prompt": "What is Python?", "session_id": "user1"}'

  # Streaming response
  curl -N http://127.0.0.1:8000/stream?prompt=hello

  # MCP tool discovery
  curl -X POST http://127.0.0.1:8000/mcp -H 'Content-Type: application/json' \
    -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'

  # Check session memory
  curl http://127.0.0.1:8000/memory/user1
"""

import json
import time
import threading
from pydantic import BaseModel
from skytrade import Pyre, SkyResponse, SkyStream

app = Pyre()
app.enable_cors()
app.enable_logging()


# ---------------------------------------------------------------------------
# Pydantic models
# ---------------------------------------------------------------------------

class ChatRequest(BaseModel):
    prompt: str
    session_id: str = "default"
    max_tokens: int = 50


# ---------------------------------------------------------------------------
# Simulated LLM (replace with real OpenAI/Anthropic client)
# ---------------------------------------------------------------------------

def fake_llm_generate(prompt: str, max_tokens: int = 50):
    """Simulate LLM token generation with realistic latency."""
    response = f"I received your question about '{prompt}'. "
    response += "Here is a thoughtful response that demonstrates "
    response += "streaming capabilities of the Pyre framework. "
    response += "Each token is sent individually with realistic delays."

    words = response.split()
    for word in words[:max_tokens]:
        time.sleep(0.03)  # ~30ms per token, realistic for LLM
        yield word + " "


# ---------------------------------------------------------------------------
# MCP Tools (discoverable by Claude Desktop, AI agents)
# ---------------------------------------------------------------------------

@app.mcp.tool(description="Search knowledge base for relevant documents")
def search_docs(query: str, top_k: int = 3) -> list:
    """Simulate RAG vector search."""
    return [
        {"title": f"Doc about {query}", "score": 0.95, "snippet": f"Information about {query}..."},
        {"title": f"Related: {query} guide", "score": 0.87, "snippet": f"A guide covering {query}..."},
    ][:top_k]


@app.mcp.tool(description="Execute Python code in sandbox")
def run_code(code: str) -> dict:
    """Simulate code execution sandbox."""
    try:
        result = eval(code, {"__builtins__": {}}, {})
        return {"output": str(result), "error": None}
    except Exception as e:
        return {"output": None, "error": str(e)}


@app.mcp.tool(description="Get current timestamp")
def get_timestamp() -> str:
    import datetime
    return datetime.datetime.now().isoformat()


@app.mcp.resource("memory://sessions", description="Active session list")
def list_sessions():
    keys = app.state.keys()
    sessions = [k.replace("session:", "") for k in keys if k.startswith("session:")]
    return {"active_sessions": sessions, "count": len(sessions)}


# ---------------------------------------------------------------------------
# Routes
# ---------------------------------------------------------------------------

@app.get("/")
def index(req):
    return {
        "service": "Pyre AI Agent Server",
        "endpoints": [
            "POST /chat — chat with AI (JSON response)",
            "GET /stream?prompt=... — streaming SSE response",
            "GET /memory/{session_id} — view session memory",
            "POST /mcp — MCP tool discovery (for Claude Desktop)",
        ],
    }


@app.post("/chat", model=ChatRequest, gil=True)
def chat(req, request: ChatRequest):
    """Synchronous chat — returns full response at once."""
    # Build response from simulated LLM
    tokens = list(fake_llm_generate(request.prompt, request.max_tokens))
    response = "".join(tokens)

    # Store in session memory (SharedState — cross-worker, nanosecond)
    history = json.loads(app.state.get(f"session:{request.session_id}") or "[]")
    history.append({"role": "user", "content": request.prompt})
    history.append({"role": "assistant", "content": response})
    app.state[f"session:{request.session_id}"] = json.dumps(history[-20:])  # Keep last 20

    return {
        "response": response,
        "session_id": request.session_id,
        "tokens": len(tokens),
    }


@app.get("/stream", gil=True)
def stream_chat(req):
    """SSE streaming — sends tokens one by one (like ChatGPT)."""
    prompt = req.query_params.get("prompt", "hello")
    session_id = req.query_params.get("session_id", "default")

    stream = SkyStream()

    def generate():
        full_response = []
        for token in fake_llm_generate(prompt):
            stream.send_event(token, event="token")
            full_response.append(token)

        # Store in memory
        response_text = "".join(full_response)
        history = json.loads(app.state.get(f"session:{session_id}") or "[]")
        history.append({"role": "user", "content": prompt})
        history.append({"role": "assistant", "content": response_text})
        app.state[f"session:{session_id}"] = json.dumps(history[-20:])

        stream.send_event(json.dumps({"done": True, "tokens": len(full_response)}), event="done")
        stream.close()

    threading.Thread(target=generate, daemon=True).start()
    return stream


@app.get("/memory/{session_id}", gil=True)
def get_memory(req):
    """View conversation history for a session."""
    session_id = req.params["session_id"]
    history = json.loads(app.state.get(f"session:{session_id}") or "[]")
    return {"session_id": session_id, "messages": history, "count": len(history)}


if __name__ == "__main__":
    app.run()

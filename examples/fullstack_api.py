"""
Full-stack REST API — demonstrates Pyre as a FastAPI replacement.

Features used:
  - CRUD routes (GET/POST/PUT/DELETE)
  - Pydantic validation
  - Cookie-based auth
  - File upload
  - Redirect
  - CORS
  - SharedState as in-memory database
  - Structured logging
  - Hot reload ready

Run:
  python examples/fullstack_api.py
  # Or with hot reload:
  PYRE_RELOAD=1 python examples/fullstack_api.py

Test:
  # Register
  curl -X POST http://127.0.0.1:8000/auth/register -H 'Content-Type: application/json' \
    -d '{"username": "alice", "email": "alice@example.com", "password": "secret123"}'

  # Login (sets cookie)
  curl -c cookies.txt -X POST http://127.0.0.1:8000/auth/login \
    -H 'Content-Type: application/json' -d '{"username": "alice", "password": "secret123"}'

  # Create item (with auth cookie)
  curl -b cookies.txt -X POST http://127.0.0.1:8000/items \
    -H 'Content-Type: application/json' -d '{"name": "Widget", "price": 9.99, "tags": ["new"]}'

  # List items
  curl http://127.0.0.1:8000/items

  # Upload avatar
  curl -b cookies.txt -F "avatar=@photo.jpg" http://127.0.0.1:8000/auth/avatar
"""

import json
import hashlib
import time
from pydantic import BaseModel, Field, field_validator
from pyreframework import Pyre, PyreResponse, redirect
from pyreframework.cookies import get_cookie, set_cookie, delete_cookie
from pyreframework.uploads import parse_multipart

app = Pyre()
app.enable_cors()
app.enable_logging()


# ---------------------------------------------------------------------------
# Models
# ---------------------------------------------------------------------------

class UserRegister(BaseModel):
    username: str = Field(min_length=3, max_length=20)
    email: str
    password: str = Field(min_length=6)

    @field_validator("email")
    @classmethod
    def validate_email(cls, v):
        if "@" not in v:
            raise ValueError("invalid email")
        return v


class UserLogin(BaseModel):
    username: str
    password: str


class ItemCreate(BaseModel):
    name: str = Field(min_length=1, max_length=100)
    price: float = Field(gt=0)
    tags: list[str] = []


class ItemUpdate(BaseModel):
    name: str | None = None
    price: float | None = None
    tags: list[str] | None = None


# ---------------------------------------------------------------------------
# Auth helpers
# ---------------------------------------------------------------------------

def hash_password(password: str) -> str:
    return hashlib.sha256(password.encode()).hexdigest()


def get_current_user(req) -> dict | None:
    token = get_cookie(req, "session_token")
    if not token:
        return None
    user_json = app.state.get(f"session:{token}")
    if not user_json:
        return None
    return json.loads(user_json)


def require_auth(req):
    user = get_current_user(req)
    if not user:
        return PyreResponse(
            body=json.dumps({"error": "unauthorized"}),
            status_code=401,
            content_type="application/json",
        )
    return user


# ---------------------------------------------------------------------------
# Auth routes
# ---------------------------------------------------------------------------

@app.get("/")
def index(req):
    return {
        "service": "Pyre Full-stack API",
        "endpoints": {
            "auth": ["POST /auth/register", "POST /auth/login", "POST /auth/logout", "GET /auth/me"],
            "items": ["GET /items", "POST /items", "GET /items/{id}", "PUT /items/{id}", "DELETE /items/{id}"],
            "files": ["POST /auth/avatar"],
        },
    }


@app.post("/auth/register", model=UserRegister, gil=True)
def register(req, user: UserRegister):
    # Check if user exists
    if app.state.get(f"user:{user.username}"):
        return PyreResponse(
            body=json.dumps({"error": "username already taken"}),
            status_code=409,
            content_type="application/json",
        )

    user_data = {
        "username": user.username,
        "email": user.email,
        "password_hash": hash_password(user.password),
        "created_at": time.time(),
        "avatar_size": 0,
    }
    app.state[f"user:{user.username}"] = json.dumps(user_data)

    return {"status": "registered", "username": user.username}


@app.post("/auth/login", model=UserLogin, gil=True)
def login(req, creds: UserLogin):
    user_json = app.state.get(f"user:{creds.username}")
    if not user_json:
        return PyreResponse(
            body=json.dumps({"error": "invalid credentials"}),
            status_code=401,
            content_type="application/json",
        )

    user = json.loads(user_json)
    if user["password_hash"] != hash_password(creds.password):
        return PyreResponse(
            body=json.dumps({"error": "invalid credentials"}),
            status_code=401,
            content_type="application/json",
        )

    # Create session token
    token = hashlib.sha256(f"{creds.username}:{time.time()}".encode()).hexdigest()[:32]
    app.state[f"session:{token}"] = json.dumps({"username": creds.username, "email": user["email"]})

    resp = PyreResponse(body=json.dumps({"status": "logged in", "username": creds.username}),
                       content_type="application/json")
    return set_cookie(resp, "session_token", token, httponly=True, max_age=86400)


@app.get("/auth/me", gil=True)
def me(req):
    auth = require_auth(req)
    if isinstance(auth, PyreResponse):
        return auth
    return auth


@app.post("/auth/logout", gil=True)
def logout(req):
    token = get_cookie(req, "session_token")
    if token:
        app.state.delete(f"session:{token}")
    resp = PyreResponse(body=json.dumps({"status": "logged out"}), content_type="application/json")
    return delete_cookie(resp, "session_token")


@app.post("/auth/avatar", gil=True)
def upload_avatar(req):
    auth = require_auth(req)
    if isinstance(auth, PyreResponse):
        return auth

    form = parse_multipart(req)
    avatar = form.get("avatar")
    if not avatar:
        return PyreResponse(body=json.dumps({"error": "no avatar file"}), status_code=400,
                           content_type="application/json")

    # Store avatar size (in real app, save to S3/disk)
    user_json = app.state.get(f"user:{auth['username']}")
    if user_json:
        user = json.loads(user_json)
        user["avatar_size"] = avatar.size
        app.state[f"user:{auth['username']}"] = json.dumps(user)

    return {
        "status": "uploaded",
        "filename": avatar.filename,
        "size": avatar.size,
        "content_type": avatar.content_type,
    }


# ---------------------------------------------------------------------------
# CRUD — Items
# ---------------------------------------------------------------------------

@app.get("/items", gil=True)
def list_items(req):
    items_json = app.state.get("items_db")
    items = json.loads(items_json) if items_json else []

    # Pagination
    page = int(req.query_params.get("page", "1"))
    per_page = int(req.query_params.get("per_page", "10"))
    start = (page - 1) * per_page
    end = start + per_page

    return {
        "items": items[start:end],
        "total": len(items),
        "page": page,
        "per_page": per_page,
    }


@app.post("/items", model=ItemCreate, gil=True)
def create_item(req, item: ItemCreate):
    auth = require_auth(req)
    if isinstance(auth, PyreResponse):
        return auth

    items = json.loads(app.state.get("items_db") or "[]")
    new_item = {
        "id": len(items) + 1,
        "name": item.name,
        "price": item.price,
        "tags": item.tags,
        "created_by": auth["username"],
        "created_at": time.time(),
    }
    items.append(new_item)
    app.state["items_db"] = json.dumps(items)

    return PyreResponse(
        body=json.dumps(new_item),
        status_code=201,
        content_type="application/json",
    )


@app.get("/items/{item_id}", gil=True)
def get_item(req):
    item_id = int(req.params["item_id"])
    items = json.loads(app.state.get("items_db") or "[]")
    for item in items:
        if item["id"] == item_id:
            return item
    return PyreResponse(body=json.dumps({"error": "not found"}), status_code=404,
                       content_type="application/json")


@app.put("/items/{item_id}", model=ItemUpdate, gil=True)
def update_item(req, updates: ItemUpdate):
    auth = require_auth(req)
    if isinstance(auth, PyreResponse):
        return auth

    item_id = int(req.params["item_id"])
    items = json.loads(app.state.get("items_db") or "[]")
    for item in items:
        if item["id"] == item_id:
            if updates.name is not None:
                item["name"] = updates.name
            if updates.price is not None:
                item["price"] = updates.price
            if updates.tags is not None:
                item["tags"] = updates.tags
            app.state["items_db"] = json.dumps(items)
            return item

    return PyreResponse(body=json.dumps({"error": "not found"}), status_code=404,
                       content_type="application/json")


@app.delete("/items/{item_id}", gil=True)
def delete_item(req):
    auth = require_auth(req)
    if isinstance(auth, PyreResponse):
        return auth

    item_id = int(req.params["item_id"])
    items = json.loads(app.state.get("items_db") or "[]")
    items = [i for i in items if i["id"] != item_id]
    app.state["items_db"] = json.dumps(items)
    return {"status": "deleted", "id": item_id}


# ---------------------------------------------------------------------------
# Start
# ---------------------------------------------------------------------------

if __name__ == "__main__":
    app.run()

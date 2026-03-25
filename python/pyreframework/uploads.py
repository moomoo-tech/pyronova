"""File upload support — multipart/form-data parser.

Usage::

    from pyreframework.uploads import parse_multipart

    @app.post("/upload")
    def upload(req):
        form = parse_multipart(req)
        f = form["file"]
        return {"filename": f.filename, "size": len(f.data)}
"""

from __future__ import annotations
from dataclasses import dataclass


@dataclass
class UploadFile:
    """A single uploaded file or form field."""
    name: str
    filename: str | None
    content_type: str
    data: bytes

    @property
    def text(self) -> str:
        return self.data.decode("utf-8", errors="replace")

    @property
    def size(self) -> int:
        return len(self.data)


def parse_multipart(req) -> dict[str, UploadFile]:
    """Parse multipart/form-data from request.

    Returns dict mapping field name → UploadFile.
    For file fields, filename and content_type are set.
    For text fields, filename is None.
    """
    ct = req.headers.get("content-type", "")
    if "multipart/form-data" not in ct:
        raise ValueError(f"Expected multipart/form-data, got: {ct}")

    # Extract boundary
    boundary = None
    for part in ct.split(";"):
        part = part.strip()
        if part.startswith("boundary="):
            boundary = part[9:].strip().strip('"')
            break

    if not boundary:
        raise ValueError("Missing boundary in Content-Type")

    body = req.body if isinstance(req.body, bytes) else req.body.encode()
    boundary_bytes = f"--{boundary}".encode()
    end_boundary = f"--{boundary}--".encode()

    parts = body.split(boundary_bytes)
    result = {}

    for part in parts:
        if not part or part.strip() == b"--" or part.strip() == b"":
            continue

        # Split headers from body (separated by \r\n\r\n)
        if b"\r\n\r\n" in part:
            header_section, file_data = part.split(b"\r\n\r\n", 1)
        elif b"\n\n" in part:
            header_section, file_data = part.split(b"\n\n", 1)
        else:
            continue

        # Strip trailing \r\n
        if file_data.endswith(b"\r\n"):
            file_data = file_data[:-2]
        elif file_data.endswith(b"\n"):
            file_data = file_data[:-1]

        # Parse headers
        headers = {}
        for line in header_section.decode("utf-8", errors="replace").split("\n"):
            line = line.strip()
            if ":" in line:
                key, _, val = line.partition(":")
                headers[key.strip().lower()] = val.strip()

        # Parse Content-Disposition
        disposition = headers.get("content-disposition", "")
        field_name = None
        filename = None

        for param in disposition.split(";"):
            param = param.strip()
            if param.startswith("name="):
                field_name = param[5:].strip('"')
            elif param.startswith("filename="):
                filename = param[9:].strip('"')

        if field_name:
            content_type = headers.get("content-type", "application/octet-stream" if filename else "text/plain")
            result[field_name] = UploadFile(
                name=field_name,
                filename=filename,
                content_type=content_type,
                data=file_data,
            )

    return result

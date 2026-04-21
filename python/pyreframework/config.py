"""Pydantic-settings base for Pyre apps.

Thin wrapper that adds Pyre-friendly defaults so user code stays short::

    from pyreframework.config import PyreSettings

    class Settings(PyreSettings):
        database_url: str
        log_level: str = "INFO"

    settings = Settings()  # reads env + .env by default

Defaults:
- ``env_file = ".env"`` — loaded if present, ignored otherwise.
- ``env_file_encoding = "utf-8"``.
- ``extra = "ignore"`` — unknown env vars don't crash the app.
- ``case_sensitive = False`` — ``DATABASE_URL`` and ``database_url``
  both populate the ``database_url`` field.

Subclasses override any of these via the usual ``model_config``::

    class Settings(PyreSettings):
        model_config = SettingsConfigDict(env_prefix="APP_")
        port: int = 8000

``pydantic-settings`` is an optional dependency. Import is deferred so
users who don't opt in don't pay the cost.
"""

from __future__ import annotations


def _load_pydantic_settings():
    try:
        from pydantic_settings import BaseSettings, SettingsConfigDict
    except ImportError as e:  # pragma: no cover — clear install guidance
        raise ImportError(
            "pyreframework.config requires pydantic-settings. Install with:\n"
            "    pip install 'pydantic-settings>=2'"
        ) from e
    return BaseSettings, SettingsConfigDict


_BaseSettings, _SettingsConfigDict = None, None


def __getattr__(name: str):
    """Lazy-import pydantic_settings only when PyreSettings is touched."""
    global _BaseSettings, _SettingsConfigDict
    if name == "PyreSettings":
        if _BaseSettings is None:
            _BaseSettings, _SettingsConfigDict = _load_pydantic_settings()

        class PyreSettings(_BaseSettings):
            model_config = _SettingsConfigDict(
                env_file=".env",
                env_file_encoding="utf-8",
                extra="ignore",
                case_sensitive=False,
            )

        return PyreSettings
    raise AttributeError(f"module 'pyreframework.config' has no attribute {name!r}")


__all__ = ["PyreSettings"]

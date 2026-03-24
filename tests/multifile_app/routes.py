"""Separate routes file — imported by main app."""


def hello(req):
    return "hello from routes.py"


def compute(req):
    from .helpers import multiply
    return {"result": multiply(7, 6)}

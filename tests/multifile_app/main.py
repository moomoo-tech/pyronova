"""Multi-file app — imports routes from separate module."""
import sys, os
sys.path.insert(0, os.path.dirname(os.path.dirname(__file__)))

from pyreframework import Pyre
from multifile_app.routes import hello, compute

app = Pyre()

app.get("/hello", hello)
app.get("/compute", compute)

@app.get("/inline")
def inline(req):
    return "inline route"

if __name__ == "__main__":
    app.run(host="127.0.0.1", port=8000, mode="subinterp")

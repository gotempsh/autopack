"""Imports a native extension at startup so a missing shared library is fatal.

`psycopg2` links against the system libpq. If the runtime image lacks libpq5
this import raises ImportError and the container dies — which is exactly the
failure a build-only check cannot see.
"""

import psycopg2
from flask import Flask

app = Flask(__name__)


@app.route("/")
def index():
    return f"hello from autopack (libpq via psycopg2 {psycopg2.__version__})\n"

"""The `python` tool's session process: a persistent REPL spoken to over newline-delimited
JSON-RPC 2.0 on stdio (ADR-0001). Standard library only; Python 3.11+.

Wire: one JSON object per line.
  -> {"jsonrpc": "2.0", "id": n, "method": "...", "params": {...}}
  <- {"jsonrpc": "2.0", "id": n, "result": ...}
  <- {"jsonrpc": "2.0", "id": n, "error": {"code": c, "message": m, "data": ...?}}

Methods:
  eval  {code}  -> {ok, stdout, stderr, value, error}   run `code` in the persistent namespace;
                   `value` is the repr of a trailing expression (or null); an exception raised by
                   the code is NOT a JSON-RPC error: the call succeeded with ok=false and
                   error={type, message, traceback}.
  reset {}      -> null                                  fresh namespace
  info  {}      -> {python, cwd, variables}
  ping  {}      -> "pong"

JSON-RPC errors are protocol failures only: -32700 parse error, -32600 invalid request,
-32601 unknown method, -32602 invalid params, -32603 internal.

The RPC channel is a private duplicate of fd 1; fd 1 itself is redirected to stderr so that
stray output (user code writing to sys.__stdout__, or subprocesses it starts) can never corrupt
the protocol stream. A SIGTERM handler exits promptly: under `--unshare-pid` this process is PID 1
in its namespace and would otherwise ignore an unhandled SIGTERM (D15).
"""

import ast
import io
import json
import os
import platform
import signal
import sys
import traceback
from contextlib import redirect_stderr, redirect_stdout

PARSE_ERROR = -32700
INVALID_REQUEST = -32600
METHOD_NOT_FOUND = -32601
INVALID_PARAMS = -32602
INTERNAL_ERROR = -32603

# Take the protocol stream away from fd 1 before anything else can write to it.
_RPC = os.fdopen(os.dup(1), "w", encoding="utf-8", buffering=1)
os.dup2(2, 1)


def _on_sigterm(signum, _frame):
    os._exit(128 + signum)


signal.signal(signal.SIGTERM, _on_sigterm)


class Repl:
    """A namespace that lives as long as the process."""

    def __init__(self):
        self.reset()

    def reset(self):
        self.ns = {"__name__": "__main__", "__builtins__": __builtins__}

    def eval(self, code):
        out, err = io.StringIO(), io.StringIO()
        value = None
        error = None
        try:
            tree = ast.parse(code, mode="exec")
            last_expr = None
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                last_expr = ast.Expression(tree.body.pop().value)
            with redirect_stdout(out), redirect_stderr(err):
                exec(compile(tree, "<python>", "exec"), self.ns)
                if last_expr is not None:
                    result = eval(compile(last_expr, "<python>", "eval"), self.ns)
                    if result is not None:
                        value = repr(result)
        except BaseException as exc:  # noqa: BLE001 - REPL semantics: report everything, keep running
            error = {
                "type": type(exc).__name__,
                "message": str(exc),
                "traceback": traceback.format_exc(),
            }
        return {
            "ok": error is None,
            "stdout": out.getvalue(),
            "stderr": err.getvalue(),
            "value": value,
            "error": error,
        }

    def info(self):
        names = sorted(k for k in self.ns if not k.startswith("__"))
        return {
            "python": platform.python_version(),
            "cwd": os.getcwd(),
            "variables": names,
        }


def respond(obj):
    _RPC.write(json.dumps(obj) + "\n")
    _RPC.flush()


def fail(rid, code, message, data=None):
    err = {"code": code, "message": message}
    if data is not None:
        err["data"] = data
    respond({"jsonrpc": "2.0", "id": rid, "error": err})


def dispatch(repl, rid, method, params):
    if method == "eval":
        code = params.get("code")
        if not isinstance(code, str):
            return fail(rid, INVALID_PARAMS, "params.code must be a string")
        return respond({"jsonrpc": "2.0", "id": rid, "result": repl.eval(code)})
    if method == "reset":
        repl.reset()
        return respond({"jsonrpc": "2.0", "id": rid, "result": None})
    if method == "info":
        return respond({"jsonrpc": "2.0", "id": rid, "result": repl.info()})
    if method == "ping":
        return respond({"jsonrpc": "2.0", "id": rid, "result": "pong"})
    return fail(rid, METHOD_NOT_FOUND, f"unknown method {method!r}")


def main():
    repl = Repl()
    stdin = sys.stdin.buffer
    for raw in iter(stdin.readline, b""):
        line = raw.decode("utf-8", errors="replace").strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except json.JSONDecodeError as exc:
            fail(None, PARSE_ERROR, f"parse error: {exc}")
            continue
        if not isinstance(req, dict):
            fail(None, INVALID_REQUEST, "request must be a JSON object")
            continue
        rid = req.get("id")
        method = req.get("method")
        params = req.get("params")
        if params is None:
            params = {}
        if not isinstance(method, str) or not isinstance(params, dict):
            fail(rid, INVALID_REQUEST, "method must be a string and params an object")
            continue
        try:
            dispatch(repl, rid, method, params)
        except Exception as exc:  # noqa: BLE001 - a bug in this server, not in user code
            fail(rid, INTERNAL_ERROR, str(exc), {"traceback": traceback.format_exc()})


if __name__ == "__main__":
    main()

"""Prototype A guest: a persistent Python REPL spoken to over JSON-RPC 2.0 on stdio.

One JSON object per line (newline-delimited JSON). Methods:
  ping()                    -> "pong"
  eval(code: str)           -> {ok, stdout, stderr, value, error}
  reset()                   -> null           (fresh namespace)
  info()                    -> {pid, python, modules}

Structured failures: exceptions raised by the user's code are *not* JSON-RPC
errors (the call itself succeeded); they come back as ok=false with an `error`
record {type, message, traceback}. JSON-RPC errors are reserved for protocol
problems (bad JSON, unknown method, bad params).

Throwaway spike code (P0.3). Do not reuse; see docs/spikes/extension-mechanism.md.
"""
import ast
import io
import json
import os
import sys
import traceback
from contextlib import redirect_stderr, redirect_stdout

PARSE_ERROR, INVALID_REQUEST, METHOD_NOT_FOUND, INVALID_PARAMS, INTERNAL = (
    -32700, -32600, -32601, -32602, -32603)


class Repl:
    def __init__(self):
        self.reset()

    def reset(self):
        self.ns = {"__name__": "__repl__", "__builtins__": __builtins__}

    def eval(self, code: str):
        out, err = io.StringIO(), io.StringIO()
        value = None
        error = None
        try:
            tree = ast.parse(code, mode="exec")
            last_expr = None
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                last_expr = ast.Expression(tree.body.pop().value)
            with redirect_stdout(out), redirect_stderr(err):
                exec(compile(tree, "<repl>", "exec"), self.ns)
                if last_expr is not None:
                    v = eval(compile(last_expr, "<repl>", "eval"), self.ns)
                    if v is not None:
                        value = repr(v)
        except BaseException as e:  # noqa: BLE001 - REPL semantics: report everything
            error = {
                "type": type(e).__name__,
                "message": str(e),
                "traceback": traceback.format_exc(),
            }
        return {"ok": error is None, "stdout": out.getvalue(),
                "stderr": err.getvalue(), "value": value, "error": error}


def respond(obj):
    sys.__stdout__.write(json.dumps(obj) + "\n")
    sys.__stdout__.flush()


def main():
    repl = Repl()
    inp = sys.stdin.buffer
    for raw in iter(inp.readline, b""):
        raw = raw.strip()
        if not raw:
            continue
        try:
            req = json.loads(raw)
        except json.JSONDecodeError as e:
            respond({"jsonrpc": "2.0", "id": None,
                     "error": {"code": PARSE_ERROR, "message": f"parse error: {e}"}})
            continue
        rid = req.get("id")
        method = req.get("method")
        params = req.get("params") or {}
        try:
            if method == "ping":
                result = "pong"
            elif method == "eval":
                if not isinstance(params.get("code"), str):
                    raise TypeError("params.code must be a string")
                result = repl.eval(params["code"])
            elif method == "reset":
                repl.reset()
                result = None
            elif method == "info":
                result = {"pid": os.getpid(), "python": sys.version,
                          "modules": len(sys.modules)}
            else:
                respond({"jsonrpc": "2.0", "id": rid,
                         "error": {"code": METHOD_NOT_FOUND, "message": f"unknown method {method!r}"}})
                continue
        except TypeError as e:
            respond({"jsonrpc": "2.0", "id": rid,
                     "error": {"code": INVALID_PARAMS, "message": str(e)}})
            continue
        except Exception as e:  # noqa: BLE001
            respond({"jsonrpc": "2.0", "id": rid,
                     "error": {"code": INTERNAL, "message": str(e),
                               "data": {"traceback": traceback.format_exc()}}})
            continue
        respond({"jsonrpc": "2.0", "id": rid, "result": result})


if __name__ == "__main__":
    main()

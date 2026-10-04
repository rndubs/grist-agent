#!/usr/bin/env python3
"""text_stats: an example out-of-process extension (docs/specs/extension-manifest.md).

Speaks newline-delimited JSON-RPC 2.0 on stdio, the stdio subset of MCP the harness uses:
`initialize`, `tools/list`, `tools/call`. Reads requests until stdin closes, so the same script
serves a Stateless extension (one request, then EOF) and a Session one (many requests).
Standard library only.
"""

import collections
import json
import os
import re
import signal
import sys

PROTOCOL_VERSION = "2025-06-18"
WORD = re.compile(r"[A-Za-z0-9']+")


def _count(args):
    text = _read(args)
    return {
        "lines": len(text.splitlines()),
        "words": len(WORD.findall(text)),
        "chars": len(text),
    }


def _top_words(args):
    limit = args.get("limit", 5)
    if not isinstance(limit, int) or not 1 <= limit <= 100:
        raise ToolError("`limit` must be an integer between 1 and 100")
    counts = collections.Counter(w.lower() for w in WORD.findall(_read(args)))
    ranked = sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:limit]
    return {"words": [{"word": w, "count": n} for w, n in ranked]}


TOOLS = {
    "count": _count,
    "top_words": _top_words,
}


class ToolError(Exception):
    """A failure the model should see (`isError: true`), not a protocol error."""


def _read(args):
    path = args.get("path")
    if not isinstance(path, str) or not path:
        raise ToolError("`path` must be a non-empty string")
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError as e:
        raise ToolError(f"cannot read {path}: {e.strerror}")


def _call(params):
    name = params.get("name")
    fn = TOOLS.get(name)
    if fn is None:
        return None, (-32602, f"unknown tool: {name!r}")
    args = params.get("arguments") or {}
    try:
        value = fn(args)
    except ToolError as e:
        return {"content": [{"type": "text", "text": str(e)}], "isError": True}, None
    return {
        "content": [{"type": "text", "text": json.dumps(value)}],
        "structuredContent": value,
    }, None


def handle(req):
    method = req.get("method")
    params = req.get("params") or {}
    if method == "initialize":
        return {
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "text_stats", "version": "0.1.0"},
        }, None
    if method == "tools/list":
        return {"tools": [{"name": n} for n in TOOLS]}, None
    if method == "tools/call":
        return _call(params)
    return None, (-32601, f"method not found: {method}")


def main():
    # Under `--unshare-pid` this process is PID 1 and ignores an unhandled SIGTERM; exit promptly.
    signal.signal(signal.SIGTERM, lambda *_: os._exit(143))
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except ValueError as e:
            reply = {"jsonrpc": "2.0", "id": None, "error": {"code": -32700, "message": str(e)}}
        else:
            result, error = handle(req)
            reply = {"jsonrpc": "2.0", "id": req.get("id")}
            if error is None:
                reply["result"] = result
            else:
                reply["error"] = {"code": error[0], "message": error[1]}
        sys.stdout.write(json.dumps(reply) + "\n")
        sys.stdout.flush()


if __name__ == "__main__":
    main()

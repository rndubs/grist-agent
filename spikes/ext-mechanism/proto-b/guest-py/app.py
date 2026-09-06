"""Prototype B guest: the same persistent REPL, as a WASM component built by
componentize-py against wit/repl.wit (world `repl-tool`).

Module globals live as long as the component instance, so `NS` persists across
host calls exactly like the process-based prototype's namespace.
Throwaway spike code (P0.3).
"""
import ast
import io
import sys
import traceback
from contextlib import redirect_stderr, redirect_stdout

from wit_world import exports
from wit_world.exports.repl import ErrorInfo, EvalResult

NS = {}


def _fresh():
    NS.clear()
    NS.update({"__name__": "__repl__", "__builtins__": __builtins__})


_fresh()


class Repl(exports.Repl):
    def eval(self, code: str) -> EvalResult:
        out, err = io.StringIO(), io.StringIO()
        value = None
        error = None
        try:
            tree = ast.parse(code, mode="exec")
            last_expr = None
            if tree.body and isinstance(tree.body[-1], ast.Expr):
                last_expr = ast.Expression(tree.body.pop().value)
            with redirect_stdout(out), redirect_stderr(err):
                exec(compile(tree, "<repl>", "exec"), NS)
                if last_expr is not None:
                    v = eval(compile(last_expr, "<repl>", "eval"), NS)
                    if v is not None:
                        value = repr(v)
        except BaseException as e:  # noqa: BLE001
            error = ErrorInfo(kind=type(e).__name__, message=str(e),
                              traceback=traceback.format_exc())
        return EvalResult(ok=error is None, stdout=out.getvalue(),
                          stderr=err.getvalue(), value=value, error=error)

    def reset(self) -> None:
        _fresh()

    def info(self) -> str:
        return f"{sys.version} platform={sys.platform} modules={len(sys.modules)}"

"""Variant that imports numpy at build time, with the Linux x86_64 numpy wheel's
site-packages on the python path. Expected to fail: numpy's extension modules are
native .so files, not wasm32-wasi. Kept to record the exact failure."""
import numpy  # noqa: F401
from app import Repl  # noqa: F401

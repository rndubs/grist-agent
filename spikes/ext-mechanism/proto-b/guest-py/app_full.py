"""Variant of app.py that imports a broad stdlib slice at build (pre-init) time so
componentize-py snapshots those modules into the component. Used only to tell
"module not snapshotted" apart from "module not in the WASI CPython build"."""
import json, statistics, math, decimal, zlib, hashlib, struct, pickle, csv, tempfile, pathlib, time, asyncio  # noqa: E401,F401
import xml.etree.ElementTree  # noqa: F401
_unavailable = {}
for _m in ("sqlite3", "ssl", "socket", "subprocess", "threading", "multiprocessing", "ctypes", "select", "signal", "mmap", "fcntl"):
    try:
        __import__(_m)
    except Exception as _e:  # noqa: BLE001
        _unavailable[_m] = f"{type(_e).__name__}: {_e}"
from app import Repl as _Base  # noqa: E402


class Repl(_Base):
    def info(self) -> str:
        return super().info() + f" build_time_unavailable={_unavailable}"

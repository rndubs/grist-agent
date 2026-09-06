//! Prototype B host (P0.3): the same Python REPL, hosted as a WASM Component
//! Model component (built by componentize-py) inside wasmtime. Throwaway spike.
//!
//! Usage: proto-b-wasm-host [--wasm ../repl.wasm] [--out results.json] [--n 200]

use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};
use wasmtime::component::{Component, Linker, ResourceTable};
use wasmtime::{Config, Engine, Store, Trap};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

const PROBE: &str = r#"
import importlib
mods = ["json","statistics","math","decimal","sqlite3","ssl","socket","subprocess","threading","multiprocessing","ctypes","asyncio","zlib","hashlib","struct","pickle","csv","xml.etree.ElementTree","tempfile","pathlib","time"]
bad = {}
for m in mods:
    try: importlib.import_module(m)
    except Exception as e: bad[m] = type(e).__name__
ops = {}
def t(name, f):
    try: f(); ops[name] = "ok"
    except BaseException as e: ops[name] = type(e).__name__ + ": " + str(e)[:60]
import time, os
t("open_write_tmp", lambda: open("/tmp/p03_probe.txt","w").write("x"))
t("os_getcwd", lambda: os.getcwd())
t("os_listdir_root", lambda: os.listdir("/"))
t("time_sleep_10ms", lambda: time.sleep(0.01))
def sp():
    import subprocess; subprocess.run(["true"], check=True)
t("subprocess_true", sp)
def th():
    import threading; x=threading.Thread(target=lambda: None); x.start(); x.join()
t("thread_start", th)
def sk():
    import socket; socket.socket().connect(("127.0.0.1", 9))
t("socket_connect", sk)
t("env", lambda: dict(os.environ))
{"missing_modules": bad, "ops": ops}
"#;
const CPU: &str = "sum(i*i for i in range(2_000_000))";

wasmtime::component::bindgen!({ world: "repl-tool", path: "../wit" });
use exports::grist::repl::repl::EvalResult;

struct Host { ctx: WasiCtx, table: ResourceTable }
impl WasiView for Host {
    fn ctx(&mut self) -> WasiCtxView<'_> { WasiCtxView { ctx: &mut self.ctx, table: &mut self.table } }
}

struct Session { store: Store<Host>, bindings: ReplTool }

impl Session {
    fn new(engine: &Engine, component: &Component, linker: &Linker<Host>) -> wasmtime::Result<Session> {
        let ctx = WasiCtxBuilder::new().inherit_stderr().build();
        let mut store = Store::new(engine, Host { ctx, table: ResourceTable::new() });
        // Epoch interruption: a deadline of 1 tick means the next increment_epoch() traps.
        store.set_epoch_deadline(1);
        store.epoch_deadline_trap();
        let bindings = ReplTool::instantiate(&mut store, component, linker)?;
        Ok(Session { store, bindings })
    }
    fn eval(&mut self, code: &str) -> wasmtime::Result<EvalResult> {
        self.bindings.grist_repl_repl().call_eval(&mut self.store, code)
    }
    fn info(&mut self) -> wasmtime::Result<String> {
        self.bindings.grist_repl_repl().call_info(&mut self.store)
    }
}

#[derive(Serialize, Default)]
struct Stats { n: usize, min_us: f64, p50_us: f64, p95_us: f64, p99_us: f64, max_us: f64, mean_us: f64 }
fn stats(mut v: Vec<Duration>) -> Stats {
    v.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let pct = |p: f64| us(v[((v.len() as f64 - 1.0) * p).round() as usize]);
    Stats { n: v.len(), min_us: us(v[0]), p50_us: pct(0.5), p95_us: pct(0.95), p99_us: pct(0.99),
        max_us: us(*v.last().unwrap()), mean_us: v.iter().map(|d| us(*d)).sum::<f64>() / v.len() as f64 }
}
fn rss_kb() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()
}

#[derive(Serialize, Default)]
struct Results {
    wasmtime_version: String,
    component_bytes: u64,
    precompiled_bytes: u64,
    compile_cold_ms: f64,
    deserialize_precompiled_ms: f64,
    instantiate_ms: Vec<f64>,
    instantiate_median_ms: f64,
    first_call_ms: f64,
    guest_python: String,
    state_persists: bool,
    error_structured: Option<Value>,
    latency_eval_trivial: Stats,
    latency_info: Stats,
    latency_eval_1mb_result: Stats,
    cancel: Option<Value>,
    restart_ms: f64,
    numpy: Option<Value>,
    rss_kb_after_instantiate: Option<u64>,
    rss_kb_after: Option<u64>,
    probe: Option<Value>,
    cpu_sum_squares: Stats,
    cpu_float_loop: Stats,
    cpu_dict_str: Stats,
}

fn main() -> wasmtime::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
    let wasm: PathBuf = arg("--wasm").unwrap_or_else(|| concat!(env!("CARGO_MANIFEST_DIR"), "/../repl.wasm").into()).into();
    let out: Option<PathBuf> = arg("--out").map(Into::into);
    let n: usize = arg("--n").map(|s| s.parse().unwrap()).unwrap_or(200);
    let mut r = Results { wasmtime_version: env!("CARGO_PKG_VERSION").to_string(), ..Default::default() };
    r.wasmtime_version = wasmtime_version();

    let mut config = Config::new();
    config.epoch_interruption(true);
    let engine = Engine::new(&config)?;
    let mut linker: Linker<Host> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_sync(&mut linker)?;

    if let Some(code) = arg("--eval") {
        let component = Component::from_file(&engine, &wasm)?;
        let mut s = Session::new(&engine, &component, &linker)?;
        println!("{}", s.info()?);
        let v = s.eval(&code)?;
        println!("{}", serde_json::to_string_pretty(&json!({"ok": v.ok, "value": v.value, "stdout": v.stdout, "stderr": v.stderr,
            "error": v.error.map(|e| json!({"type": e.kind, "message": e.message, "traceback": e.traceback}))}))?);
        return Ok(());
    }

    // Startup, part 1: compile. Cold (cranelift, all cores) vs. precompiled artifact.
    r.component_bytes = std::fs::metadata(&wasm)?.len();
    let t0 = Instant::now();
    let component = Component::from_file(&engine, &wasm)?;
    r.compile_cold_ms = t0.elapsed().as_secs_f64() * 1e3;
    let cwasm = component.serialize()?;
    r.precompiled_bytes = cwasm.len() as u64;
    let cwasm_path = wasm.with_extension("cwasm");
    std::fs::write(&cwasm_path, &cwasm)?;
    let t0 = Instant::now();
    // SAFETY: we just wrote this file ourselves from the same engine config. Spike code.
    let component = unsafe { Component::deserialize_file(&engine, &cwasm_path)? };
    r.deserialize_precompiled_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("compile cold {:.0} ms; precompiled {} bytes loads in {:.1} ms", r.compile_cold_ms, r.precompiled_bytes, r.deserialize_precompiled_ms);

    // Startup, part 2: instantiate (Python interpreter is pre-initialised by componentize-py's wizer pass).
    let mut sess = None;
    for _ in 0..5 {
        let t0 = Instant::now();
        let s = Session::new(&engine, &component, &linker)?;
        r.instantiate_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        sess = Some(s);
    }
    let mut sorted = r.instantiate_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    r.instantiate_median_ms = sorted[2];
    let mut s = sess.unwrap();
    r.rss_kb_after_instantiate = rss_kb();
    let t0 = Instant::now();
    r.guest_python = s.info()?;
    r.first_call_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("instantiate median {:.1} ms; first call {:.1} ms; guest: {}", r.instantiate_median_ms, r.first_call_ms, r.guest_python);

    // State persistence.
    s.eval("x = 41")?;
    let v = s.eval("x + 1")?;
    r.state_persists = v.value.as_deref() == Some("42");
    println!("state persists across calls: {} (x+1 -> {:?})", r.state_persists, v.value);

    // Structured error.
    let v = s.eval("1/0")?;
    let e = v.error.as_ref().unwrap();
    println!("user error: ok={} type={} msg={}", v.ok, e.kind, e.message);
    r.error_structured = Some(json!({"ok": v.ok, "type": e.kind, "message": e.message}));

    // Latency.
    let mut d = Vec::with_capacity(n);
    for _ in 0..n { let t0 = Instant::now(); s.info()?; d.push(t0.elapsed()); }
    r.latency_info = stats(d);
    let mut d = Vec::with_capacity(n);
    for _ in 0..n { let t0 = Instant::now(); let v = s.eval("1+1")?; assert_eq!(v.value.as_deref(), Some("2")); d.push(t0.elapsed()); }
    r.latency_eval_trivial = stats(d);
    let mut d = Vec::with_capacity(20);
    for _ in 0..20 { let t0 = Instant::now(); let v = s.eval("'a'*1_000_000")?; assert_eq!(v.value.unwrap().len(), 1_000_002); d.push(t0.elapsed()); }
    r.latency_eval_1mb_result = stats(d);
    println!("info p50={:.0}us  eval(1+1) p50={:.0}us p99={:.0}us  eval(1MB) p50={:.0}us",
        r.latency_info.p50_us, r.latency_eval_trivial.p50_us, r.latency_eval_trivial.p99_us, r.latency_eval_1mb_result.p50_us);
    r.rss_kb_after = rss_kb();

    // Cancellation: infinite loop, host bumps the epoch after 500 ms, call traps, instance is discarded.
    s.eval("y = 'set before hang'")?;
    let eng = engine.clone();
    let t0 = Instant::now();
    let th = std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(500)); eng.increment_epoch(); });
    match s.eval("while True: pass") {
        Err(e) => {
            let trap = e.downcast_ref::<Trap>().copied();
            println!("cancelled after {:?}: trap={trap:?}", t0.elapsed());
            r.cancel = Some(json!({"trap": format!("{trap:?}"), "total_ms": t0.elapsed().as_secs_f64()*1e3}));
        }
        Ok(_) => panic!("expected trap"),
    }
    th.join().unwrap();
    // After a trap the store is poisoned for further calls; a Session-kind tool restarts.
    drop(s);
    let t0 = Instant::now();
    let mut s = Session::new(&engine, &component, &linker)?;
    let v = s.eval("y")?;
    r.restart_ms = t0.elapsed().as_secs_f64() * 1e3;
    let fresh = v.error.as_ref().map(|e| e.kind.as_str()) == Some("NameError");
    println!("restarted in {:.1} ms; state after restart is fresh: {fresh}", r.restart_ms);
    assert!(fresh);

    // Dependency story.
    let v = s.eval("import numpy as np; float(np.arange(10).sum())")?;
    r.numpy = Some(match v.error { None => json!({"available": true, "value": v.value}),
        Some(e) => json!({"available": false, "error": e.kind, "message": e.message}) });
    println!("numpy: {}", r.numpy.as_ref().unwrap());
    // What *does* work: the pure-Python stdlib.
    let v = s.eval("import json, statistics, math, decimal, sqlite3; statistics.mean([1,2,3])")?;
    println!("stdlib probe: ok={} value={:?} err={:?}", v.ok, v.value, v.error.map(|e| e.kind));

    // Facilities probe + CPU-bound benchmark (compare against Prototype A).
    let v = s.eval(PROBE)?;
    r.probe = Some(json!({"value": v.value, "error": v.error.map(|e| format!("{}: {}", e.kind, e.message))}));
    println!("probe: {}", r.probe.as_ref().unwrap());
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.eval("sum(i*i for i in range(2_000_000))")?; assert_eq!(v.value.as_deref(), Some("2666664666667000000"), "cpu_sum_squares"); d.push(t0.elapsed()); }
    r.cpu_sum_squares = stats(d);
    println!("cpu_sum_squares p50={:.1} ms", r.cpu_sum_squares.p50_us / 1e3);
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.eval("import math\nacc = 0.0\nfor i in range(1, 500_001):\n    acc += math.sqrt(i) * 0.5\nround(acc, 3)")?; assert_eq!(v.value.as_deref(), Some("117851306.871"), "cpu_float_loop"); d.push(t0.elapsed()); }
    r.cpu_float_loop = stats(d);
    println!("cpu_float_loop p50={:.1} ms", r.cpu_float_loop.p50_us / 1e3);
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.eval("d = {}\nfor i in range(300_000):\n    d[str(i)] = i\nsum(len(k) for k in d)")?; assert_eq!(v.value.as_deref(), Some("1688890"), "cpu_dict_str"); d.push(t0.elapsed()); }
    r.cpu_dict_str = stats(d);
    println!("cpu_dict_str p50={:.1} ms", r.cpu_dict_str.p50_us / 1e3);

    if let Some(p) = out { std::fs::write(&p, serde_json::to_string_pretty(&r).unwrap())?; println!("wrote {}", p.display()); }
    Ok(())
}

fn wasmtime_version() -> String {
    // Cargo.lock is the source of truth; read it so the write-up records the resolved version.
    let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.lock")).unwrap_or_default();
    let mut it = lock.lines();
    while let Some(l) = it.next() {
        if l == "name = \"wasmtime\"" { if let Some(v) = it.next() { return v.trim_start_matches("version = ").trim_matches('"').to_string(); } }
    }
    "unknown".into()
}

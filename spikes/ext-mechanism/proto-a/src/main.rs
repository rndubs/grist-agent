//! Prototype A host (P0.3): a long-lived Python REPL process driven over
//! newline-delimited JSON-RPC 2.0 on stdio. Throwaway spike code.
//!
//! Usage: proto-a-jsonrpc [--python PATH] [--out results.json] [--n 200]

use serde::Serialize;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

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
const SERVER: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/repl_server.py");

#[derive(Debug)]
enum CallError {
    /// The guest returned a JSON-RPC error object (protocol-level failure).
    Rpc { code: i64, message: String, data: Option<Value> },
    /// The call exceeded its deadline; the host killed the session.
    Cancelled { signal: i32, exit: String, kill_took: Duration },
    /// The process died or the pipe broke.
    Transport(String),
}

struct Session {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    pid: u32,
}

impl Session {
    async fn spawn(python: &str) -> Result<Session, CallError> {
        let mut child = Command::new(python)
            .arg("-u")
            .arg(SERVER)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .env_clear() // D10: scrubbed environment
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CallError::Transport(format!("spawn: {e}")))?;
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let pid = child.id().unwrap();
        Ok(Session { child, stdin, stdout, next_id: 0, pid })
    }

    async fn call(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
        self.next_id += 1;
        let id = self.next_id;
        let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut line = serde_json::to_string(&req).unwrap();
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| CallError::Transport(format!("write: {e}")))?;
        let mut buf = String::new();
        let read = tokio::time::timeout(timeout, self.stdout.read_line(&mut buf)).await;
        match read {
            Err(_elapsed) => Err(self.cancel().await),
            Ok(Err(e)) => Err(CallError::Transport(format!("read: {e}"))),
            Ok(Ok(0)) => Err(CallError::Transport("eof: guest exited".into())),
            Ok(Ok(_)) => {
                let resp: Value = serde_json::from_str(&buf)
                    .map_err(|e| CallError::Transport(format!("bad json from guest: {e}")))?;
                if resp["id"] != json!(id) {
                    return Err(CallError::Transport(format!("id mismatch: {resp}")));
                }
                if let Some(err) = resp.get("error") {
                    return Err(CallError::Rpc {
                        code: err["code"].as_i64().unwrap_or(0),
                        message: err["message"].as_str().unwrap_or("").to_string(),
                        data: err.get("data").cloned(),
                    });
                }
                Ok(resp["result"].clone())
            }
        }
    }

    /// D15: the sandbox launcher sends SIGTERM to a running tool. Escalate to
    /// SIGKILL if the guest ignores it.
    async fn cancel(&mut self) -> CallError {
        let t0 = Instant::now();
        // SAFETY: plain syscall on a pid we own. Spike code.
        unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGTERM) };
        let mut signal = libc::SIGTERM;
        let status = match tokio::time::timeout(Duration::from_secs(1), self.child.wait()).await {
            Ok(Ok(s)) => s,
            _ => {
                signal = libc::SIGKILL;
                let _ = self.child.kill().await;
                self.child.wait().await.unwrap()
            }
        };
        CallError::Cancelled { signal, exit: format!("{status:?}"), kill_took: t0.elapsed() }
    }
}

#[derive(Serialize, Default)]
struct Stats { n: usize, min_us: f64, p50_us: f64, p95_us: f64, p99_us: f64, max_us: f64, mean_us: f64 }

fn stats(mut v: Vec<Duration>) -> Stats {
    v.sort();
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let pct = |p: f64| us(v[((v.len() as f64 - 1.0) * p).round() as usize]);
    Stats {
        n: v.len(), min_us: us(v[0]), p50_us: pct(0.5), p95_us: pct(0.95), p99_us: pct(0.99),
        max_us: us(*v.last().unwrap()),
        mean_us: v.iter().map(|d| us(*d)).sum::<f64>() / v.len() as f64,
    }
}

#[derive(Serialize, Default)]
struct Results {
    python: String,
    python_version: String,
    startup_ms: Vec<f64>,
    startup_median_ms: f64,
    state_persists: bool,
    error_structured: Option<Value>,
    rpc_error_code: Option<i64>,
    latency_ping: Stats,
    latency_eval_trivial: Stats,
    latency_eval_1mb_result: Stats,
    cancel: Option<Value>,
    restart_ms: f64,
    numpy: Option<Value>,
    rss_kb_after: Option<u64>,
    probe: Option<Value>,
    cpu_sum_squares: Stats,
    cpu_float_loop: Stats,
    cpu_dict_str: Stats,
}

fn rss_kb(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    s.lines().find(|l| l.starts_with("VmRSS:"))?.split_whitespace().nth(1)?.parse().ok()
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).map(|i| args[i + 1].clone());
    let python = arg("--python").unwrap_or_else(|| "python3".into());
    let out: Option<PathBuf> = arg("--out").map(Into::into);
    let n: usize = arg("--n").map(|s| s.parse().unwrap()).unwrap_or(200);
    let t = Duration::from_secs(30);
    let mut r = Results { python: python.clone(), ..Default::default() };

    // 1. startup: spawn -> first successful round-trip, five samples.
    let mut sess = None;
    for _ in 0..5 {
        let t0 = Instant::now();
        let mut s = Session::spawn(&python).await.unwrap();
        let pong = s.call("ping", json!({}), t).await.unwrap();
        assert_eq!(pong, json!("pong"));
        r.startup_ms.push(t0.elapsed().as_secs_f64() * 1e3);
        sess = Some(s);
    }
    let mut sorted = r.startup_ms.clone();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    r.startup_median_ms = sorted[2];
    let mut s = sess.unwrap();
    let info = s.call("info", json!({}), t).await.unwrap();
    r.python_version = info["python"].as_str().unwrap_or("").to_string();

    // 2. state persistence across calls.
    s.call("eval", json!({"code": "x = 41"}), t).await.unwrap();
    let v = s.call("eval", json!({"code": "x + 1"}), t).await.unwrap();
    r.state_persists = v["value"] == json!("42");
    println!("state persists across calls: {} (x+1 -> {})", r.state_persists, v["value"]);

    // 3. structured error from user code.
    let v = s.call("eval", json!({"code": "1/0"}), t).await.unwrap();
    println!("user error: ok={} type={} msg={}", v["ok"], v["error"]["type"], v["error"]["message"]);
    r.error_structured = Some(json!({"ok": v["ok"], "type": v["error"]["type"], "message": v["error"]["message"]}));

    // 4. protocol error from the RPC layer.
    match s.call("no_such_method", json!({}), t).await {
        Err(CallError::Rpc { code, message, .. }) => { println!("rpc error: {code} {message}"); r.rpc_error_code = Some(code); }
        other => panic!("expected rpc error, got {other:?}"),
    }

    // 5. latency.
    let mut d = Vec::with_capacity(n);
    for _ in 0..n { let t0 = Instant::now(); s.call("ping", json!({}), t).await.unwrap(); d.push(t0.elapsed()); }
    r.latency_ping = stats(d);
    let mut d = Vec::with_capacity(n);
    for _ in 0..n { let t0 = Instant::now(); let v = s.call("eval", json!({"code": "1+1"}), t).await.unwrap(); assert_eq!(v["value"], json!("2")); d.push(t0.elapsed()); }
    r.latency_eval_trivial = stats(d);
    let mut d = Vec::with_capacity(20);
    for _ in 0..20 { let t0 = Instant::now(); let v = s.call("eval", json!({"code": "'a'*1_000_000"}), t).await.unwrap(); assert_eq!(v["value"].as_str().unwrap().len(), 1_000_002); d.push(t0.elapsed()); }
    r.latency_eval_1mb_result = stats(d);
    println!("ping p50={:.0}us  eval(1+1) p50={:.0}us p99={:.0}us  eval(1MB) p50={:.0}us",
        r.latency_ping.p50_us, r.latency_eval_trivial.p50_us, r.latency_eval_trivial.p99_us, r.latency_eval_1mb_result.p50_us);
    r.rss_kb_after = rss_kb(s.pid);

    // 6. cancellation: an infinite loop, 500 ms deadline, SIGTERM, restart.
    s.call("eval", json!({"code": "y = 'set before hang'"}), t).await.unwrap();
    let t0 = Instant::now();
    match s.call("eval", json!({"code": "while True: pass"}), Duration::from_millis(500)).await {
        Err(CallError::Cancelled { signal, exit, kill_took }) => {
            println!("cancelled after {:?}: signal={signal} exit={exit} kill_took={kill_took:?}", t0.elapsed());
            r.cancel = Some(json!({"signal": signal, "exit": exit, "kill_took_ms": kill_took.as_secs_f64()*1e3, "total_ms": t0.elapsed().as_secs_f64()*1e3}));
        }
        other => panic!("expected cancellation, got {other:?}"),
    }
    let t0 = Instant::now();
    let mut s = Session::spawn(&python).await.unwrap();
    let v = s.call("eval", json!({"code": "y"}), t).await.unwrap();
    r.restart_ms = t0.elapsed().as_secs_f64() * 1e3;
    println!("restarted in {:.1} ms; state after restart is fresh: {}", r.restart_ms, v["error"]["type"] == json!("NameError"));
    assert_eq!(v["error"]["type"], json!("NameError"));

    // 7. dependency story: numpy, if this interpreter has it.
    let t0 = Instant::now();
    let v = s.call("eval", json!({"code": "import numpy as np; a = np.arange(1_000_000, dtype='f8'); float(a.sum())"}), t).await.unwrap();
    let import_ms = t0.elapsed().as_secs_f64() * 1e3;
    if v["ok"] == json!(true) {
        let t0 = Instant::now();
        let v2 = s.call("eval", json!({"code": "float((a * 2).mean())"}), t).await.unwrap();
        r.numpy = Some(json!({"available": true, "version": s.call("eval", json!({"code": "np.__version__"}), t).await.unwrap()["value"],
            "import_and_sum_ms": import_ms, "sum": v["value"], "reuse_ms": t0.elapsed().as_secs_f64()*1e3, "reuse_value": v2["value"]}));
    } else {
        r.numpy = Some(json!({"available": false, "error": v["error"]["type"]}));
    }
    println!("numpy: {}", r.numpy.as_ref().unwrap());

    // 8. facilities probe + CPU-bound benchmark (compare against Prototype B).
    let v = s.call("eval", json!({"code": PROBE}), t).await.unwrap();
    r.probe = Some(json!({"value": v["value"], "error": v["error"]["type"]}));
    println!("probe: {}", v["value"]);
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.call("eval", json!({"code": "sum(i*i for i in range(2_000_000))"}), t).await.unwrap(); assert_eq!(v["value"], json!("2666664666667000000"), "cpu_sum_squares"); d.push(t0.elapsed()); }
    r.cpu_sum_squares = stats(d);
    println!("cpu_sum_squares p50={:.1} ms", r.cpu_sum_squares.p50_us / 1e3);
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.call("eval", json!({"code": "import math\nacc = 0.0\nfor i in range(1, 500_001):\n    acc += math.sqrt(i) * 0.5\nround(acc, 3)"}), t).await.unwrap(); assert_eq!(v["value"], json!("117851306.871"), "cpu_float_loop"); d.push(t0.elapsed()); }
    r.cpu_float_loop = stats(d);
    println!("cpu_float_loop p50={:.1} ms", r.cpu_float_loop.p50_us / 1e3);
    let mut d = Vec::with_capacity(5);
    for _ in 0..5 { let t0 = Instant::now(); let v = s.call("eval", json!({"code": "d = {}\nfor i in range(300_000):\n    d[str(i)] = i\nsum(len(k) for k in d)"}), t).await.unwrap(); assert_eq!(v["value"], json!("1688890"), "cpu_dict_str"); d.push(t0.elapsed()); }
    r.cpu_dict_str = stats(d);
    println!("cpu_dict_str p50={:.1} ms", r.cpu_dict_str.p50_us / 1e3);

    if let Some(p) = out { std::fs::write(&p, serde_json::to_string_pretty(&r).unwrap()).unwrap(); println!("wrote {}", p.display()); }
}

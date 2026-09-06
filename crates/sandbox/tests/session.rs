//! `JsonRpcSession` with the embedded `repl_server.py`, launched through `NoneBackend`
//! (feature `dev-sandbox-none`). Runs on any Linux host with `python3`.
#![cfg(feature = "dev-sandbox-none")]

mod support;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::cancel::CancellationToken;
use kernel::host::Command;
use kernel::sandbox::{RpcRequest, RpcResponse, SandboxBackend, SandboxError, SessionProcess};
use kernel::tool::Tool;
use sandbox::session::SessionSpec;
use sandbox::tools::{PythonTool, REPL_SERVER_SOURCE};
use sandbox::{JsonRpcSession, NoneBackend};
use serde_json::{Value, json};

fn backend() -> NoneBackend {
    NoneBackend::new(Arc::new(support::FsHost)).with_grace(Duration::from_millis(500))
}

fn repl_command(workdir: &std::path::Path) -> Command {
    PythonTool::new(workdir)
        .session_command()
        .expect("session tool has a command")
}

async fn launch(timeout: Duration) -> (tempfile::TempDir, Box<dyn SessionProcess>) {
    let (dir, work) = support::workdir();
    let policy = support::policy(&[], timeout);
    let session = backend()
        .launch_session(&policy, repl_command(&work))
        .await
        .expect("launch python session");
    (dir, session)
}

async fn eval(session: &dyn SessionProcess, code: &str) -> Value {
    let req = RpcRequest {
        method: "eval".into(),
        params: json!({ "code": code }),
    };
    match session.call(req, CancellationToken::new()).await {
        Ok(RpcResponse::Result(v)) => v,
        other => panic!("eval {code:?}: {other:?}"),
    }
}

#[tokio::test]
async fn state_persists_across_calls() {
    let (_dir, session) = launch(Duration::from_secs(10)).await;
    let r = eval(session.as_ref(), "x = 41").await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["value"], Value::Null);
    let r = eval(session.as_ref(), "x + 1").await;
    assert_eq!(r["ok"], json!(true));
    assert_eq!(r["value"], json!("42"));
    let r = eval(
        session.as_ref(),
        "print('hi'); import sys; print('err', file=sys.stderr)",
    )
    .await;
    assert_eq!(r["stdout"], json!("hi\n"));
    assert_eq!(r["stderr"], json!("err\n"));
    assert!(session.is_alive());
    session.terminate().await.expect("terminate");
    assert!(!session.is_alive());
}

#[tokio::test]
async fn user_code_exceptions_are_successful_calls_with_ok_false() {
    let (_dir, session) = launch(Duration::from_secs(10)).await;
    let r = eval(session.as_ref(), "1/0").await;
    assert_eq!(r["ok"], json!(false));
    assert_eq!(r["error"]["type"], json!("ZeroDivisionError"));
    assert!(
        r["error"]["traceback"]
            .as_str()
            .unwrap()
            .contains("ZeroDivisionError")
    );
    let r = eval(session.as_ref(), "def (").await;
    assert_eq!(r["error"]["type"], json!("SyntaxError"));
    let r = eval(session.as_ref(), "raise SystemExit(3)").await;
    assert_eq!(
        r["error"]["type"],
        json!("SystemExit"),
        "exit() does not kill the session"
    );
    assert!(session.is_alive());
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn unknown_method_is_a_protocol_error() {
    let (_dir, session) = launch(Duration::from_secs(10)).await;
    let r = session
        .call(
            RpcRequest {
                method: "nope".into(),
                params: json!({}),
            },
            CancellationToken::new(),
        )
        .await
        .expect("protocol error is Ok(Error)");
    assert!(
        matches!(r, RpcResponse::Error { code: -32601, .. }),
        "{r:?}"
    );
    let r = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: json!({ "code": 5 }),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(
        matches!(r, RpcResponse::Error { code: -32602, .. }),
        "{r:?}"
    );
    assert!(
        session.is_alive(),
        "protocol errors do not kill the session"
    );
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn info_reset_and_ping() {
    let (_dir, session) = launch(Duration::from_secs(10)).await;
    eval(session.as_ref(), "y = 1").await;
    let call = |m: &str| RpcRequest {
        method: m.into(),
        params: json!({}),
    };
    match session
        .call(call("info"), CancellationToken::new())
        .await
        .unwrap()
    {
        RpcResponse::Result(v) => {
            assert!(v["python"].as_str().unwrap().starts_with('3'));
            assert!(v["cwd"].is_string());
            assert_eq!(v["variables"], json!(["y"]));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        session.call(call("ping"), CancellationToken::new()).await.unwrap(),
        RpcResponse::Result(Value::String(s)) if s == "pong"
    ));
    session
        .call(call("reset"), CancellationToken::new())
        .await
        .unwrap();
    let r = eval(session.as_ref(), "y").await;
    assert_eq!(r["error"]["type"], json!("NameError"));
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn cancel_kills_the_process_and_a_relaunch_is_fresh() {
    let (_dir, session) = launch(Duration::from_secs(30)).await;
    eval(session.as_ref(), "x = 1").await;
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let start = Instant::now();
    let err = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: json!({ "code": "while True: pass" }),
            },
            cancel,
        )
        .await
        .expect_err("cancelled");
    assert!(matches!(err, SandboxError::Cancelled), "{err}");
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!session.is_alive());
    let err = session
        .call(
            RpcRequest {
                method: "ping".into(),
                params: json!({}),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("dead session");
    assert!(
        matches!(err, SandboxError::Exited | SandboxError::Rpc(_)),
        "{err}"
    );
    session.terminate().await.expect("terminate is idempotent");

    let (_dir2, fresh) = launch(Duration::from_secs(10)).await;
    let r = eval(fresh.as_ref(), "x").await;
    assert_eq!(
        r["error"]["type"],
        json!("NameError"),
        "namespace is fresh after relaunch"
    );
    fresh.terminate().await.unwrap();
}

#[tokio::test]
async fn per_call_timeout_kills_the_process() {
    let (_dir, session) = launch(Duration::from_secs(1)).await;
    let start = Instant::now();
    let err = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: json!({ "code": "import time; time.sleep(30)" }),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("timeout");
    assert!(
        matches!(err, SandboxError::Timeout(d) if d == Duration::from_secs(1)),
        "{err}"
    );
    assert!(start.elapsed() < Duration::from_secs(5));
    assert!(!session.is_alive());
}

#[tokio::test]
async fn sigterm_is_honored_before_the_grace_expires() {
    // Direct spawn so the grace is observable: with a 5 s grace, a process that ignored SIGTERM
    // would take >= 5 s to die; the REPL's handler makes it exit at once.
    let session = JsonRpcSession::spawn(SessionSpec {
        program: "python3".into(),
        args: vec!["-c".into(), REPL_SERVER_SOURCE.into()],
        cwd: Some(PathBuf::from("/tmp")),
        env: support::env_map(&[("PATH", &std::env::var("PATH").unwrap()), ("HOME", "/tmp")]),
        timeout: Duration::from_millis(300),
        grace: Duration::from_secs(5),
    })
    .await
    .expect("spawn");
    let start = Instant::now();
    let err = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: json!({ "code": "import time; time.sleep(30)" }),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("timeout");
    assert!(matches!(err, SandboxError::Timeout(_)));
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "SIGTERM was not honored promptly: {:?}",
        start.elapsed()
    );
    assert!(!session.is_alive());
    assert!(support::wait_gone(session.pid(), Duration::from_secs(2)).await);
}

#[tokio::test]
async fn stray_stdout_cannot_corrupt_the_protocol() {
    let session = JsonRpcSession::spawn(SessionSpec {
        program: "python3".into(),
        args: vec!["-c".into(), REPL_SERVER_SOURCE.into()],
        cwd: Some(PathBuf::from("/tmp")),
        env: support::env_map(&[("PATH", &std::env::var("PATH").unwrap()), ("HOME", "/tmp")]),
        timeout: Duration::from_secs(10),
        grace: Duration::from_millis(500),
    })
    .await
    .expect("spawn");
    // Writes to fd 1 outside the capture, and a subprocess inheriting fd 1.
    let r = eval(
        &session,
        "import os, subprocess; os.write(1, b'stray\\n'); subprocess.run(['echo', 'child-stray'])",
    )
    .await;
    assert_eq!(r["ok"], json!(true), "{r}");
    let r = eval(&session, "2 + 2").await;
    assert_eq!(r["value"], json!("4"), "the protocol stream is intact");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tail = session.stderr_tail();
    assert!(
        tail.contains("stray") && tail.contains("child-stray"),
        "stray output landed on stderr: {tail:?}"
    );
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn process_exit_is_reported_as_exited() {
    let session = JsonRpcSession::spawn(SessionSpec {
        program: "bash".into(),
        args: vec!["-c".into(), "read -r line; exit 0".into()],
        cwd: None,
        env: support::env_map(&[("PATH", &std::env::var("PATH").unwrap())]),
        timeout: Duration::from_secs(5),
        grace: Duration::from_millis(500),
    })
    .await
    .expect("spawn");
    let err = session
        .call(
            RpcRequest {
                method: "ping".into(),
                params: json!({}),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("the process exits without answering");
    assert!(matches!(err, SandboxError::Exited), "{err}");
    assert!(!session.is_alive());
}

#[tokio::test]
async fn malformed_response_is_an_rpc_error() {
    let session = JsonRpcSession::spawn(SessionSpec {
        program: "bash".into(),
        args: vec!["-c".into(), "read -r line; echo 'not json'; sleep 5".into()],
        cwd: None,
        env: support::env_map(&[("PATH", &std::env::var("PATH").unwrap())]),
        timeout: Duration::from_secs(5),
        grace: Duration::from_millis(500),
    })
    .await
    .expect("spawn");
    let err = session
        .call(
            RpcRequest {
                method: "ping".into(),
                params: json!({}),
            },
            CancellationToken::new(),
        )
        .await
        .expect_err("malformed");
    assert!(
        matches!(err, SandboxError::Rpc(ref m) if m.contains("malformed")),
        "{err}"
    );
    session.terminate().await.unwrap();
}

#[tokio::test]
async fn parse_error_from_the_repl_is_minus_32700() {
    // Drive the script directly with an invalid line; the session type never sends one.
    let out = tokio::process::Command::new("python3")
        .args(["-c", REPL_SERVER_SOURCE])
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut child = out;
    {
        use tokio::io::AsyncWriteExt;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(b"not json\n[1,2]\n").await.unwrap();
        stdin.shutdown().await.unwrap();
    }
    let out = child.wait_with_output().await.unwrap();
    let lines: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 2, "{out:?}");
    assert_eq!(lines[0]["error"]["code"], json!(-32700));
    assert_eq!(lines[0]["id"], Value::Null);
    assert_eq!(lines[1]["error"]["code"], json!(-32600));
}

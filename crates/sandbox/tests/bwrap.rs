//! `BwrapBackend` end to end. These need a host with `bwrap` on `PATH` (and permission to create
//! user namespaces); without it every test prints `skipping: bwrap not available` and passes,
//! unless `GRIST_REQUIRE_BWRAP=1` (what the CI `test` job sets), in which case a missing `bwrap`
//! fails the test instead of skipping it.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::cancel::CancellationToken;
use kernel::capability::FsMode;
use kernel::host::Command;
use kernel::sandbox::{SandboxBackend, SandboxError};
use sandbox::BwrapBackend;
use sandbox::bwrap::program_available;

fn skip() -> bool {
    if program_available("bwrap") {
        false
    } else if std::env::var("GRIST_REQUIRE_BWRAP").as_deref() == Ok("1") {
        panic!("bwrap not on PATH but GRIST_REQUIRE_BWRAP=1 (the bwrap tests must run in CI)")
    } else {
        println!("skipping: bwrap not available");
        true
    }
}

fn backend() -> BwrapBackend {
    BwrapBackend::new(Arc::new(support::FsHost)).with_grace(Duration::from_millis(500))
}

fn bash(script: &str, cwd: Option<&Path>) -> Command {
    Command {
        program: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: cwd.map(Path::to_path_buf),
        env: Default::default(),
        stdin: None,
    }
}

#[test]
fn name_is_bwrap() {
    assert_eq!(backend().name(), "bwrap");
}

#[tokio::test]
async fn fs_ro_mount_refuses_a_write_and_rw_allows_it() {
    if skip() {
        return;
    }
    let (_dir, work) = support::workdir();
    let ro = support::policy(&[(&work, FsMode::Ro)], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(
            &ro,
            bash("touch probe", Some(&work)),
            CancellationToken::new(),
        )
        .await
        .expect("bwrap ran");
    assert_ne!(
        out.exit_code,
        Some(0),
        "write under fs.ro must fail: {out:?}"
    );
    assert!(!work.join("probe").exists());

    let rw = support::policy(&[(&work, FsMode::Rw)], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(
            &rw,
            bash("touch probe", Some(&work)),
            CancellationToken::new(),
        )
        .await
        .expect("bwrap ran");
    assert_eq!(out.exit_code, Some(0), "{out:?}");
    assert!(work.join("probe").exists());

    // Everything else is read-only base image.
    let out = backend()
        .launch_stateless(
            &rw,
            bash("touch /usr/probe", None),
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_ne!(out.exit_code, Some(0));
}

#[tokio::test]
async fn network_off_cannot_open_a_socket() {
    if skip() {
        return;
    }
    let policy = support::policy(&[], Duration::from_secs(10));
    let mut cmd = Command::new("python3");
    cmd.args = vec![
        "-c".into(),
        "import socket; socket.create_connection(('1.1.1.1', 53), 2)".into(),
    ];
    let out = backend()
        .launch_stateless(&policy, cmd, CancellationToken::new())
        .await
        .expect("bwrap ran");
    assert_ne!(out.exit_code, Some(0), "socket must fail: {out:?}");
}

#[tokio::test]
async fn pid_namespace_and_scrubbed_env_and_tmpfs() {
    if skip() {
        return;
    }
    let policy = support::policy(&[], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(
            &policy,
            bash(
                "echo $$; env | sort; pwd; touch /tmp/x && echo tmp-ok",
                None,
            ),
            CancellationToken::new(),
        )
        .await
        .expect("bwrap ran");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(
        out.exit_code,
        Some(0),
        "{text}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let first: u32 = text.lines().next().unwrap().trim().parse().unwrap();
    assert!(first <= 3, "own pid namespace: {text}");
    assert!(text.contains("HOME=/tmp\n"));
    assert!(!text.contains("KEY") && !text.contains("TOKEN"));
    assert!(text.contains("\n/tmp\n"), "default cwd is /tmp: {text}");
    assert!(text.contains("tmp-ok"));
}

#[tokio::test]
async fn timeout_is_enforced_under_bwrap() {
    if skip() {
        return;
    }
    let policy = support::policy(&[], Duration::from_secs(1));
    let start = Instant::now();
    let err = backend()
        .launch_stateless(&policy, bash("sleep 30", None), CancellationToken::new())
        .await
        .expect_err("times out");
    assert!(matches!(err, SandboxError::Timeout(_)), "{err}");
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn python_session_under_bwrap() {
    if skip() {
        return;
    }
    use kernel::sandbox::{RpcRequest, RpcResponse};
    use kernel::tool::Tool;
    let (_dir, work) = support::workdir();
    let policy = support::policy(&[(&work, FsMode::Rw)], Duration::from_secs(10));
    let tool = sandbox::tools::PythonTool::new(&work);
    let session = backend()
        .launch_session(&policy, tool.session_command().unwrap())
        .await
        .expect("launch");
    let r = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: serde_json::json!({"code": "import os; os.getpid()"}),
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    match r {
        RpcResponse::Result(v) => {
            let pid: u32 = v["value"].as_str().unwrap().parse().unwrap();
            assert!(pid <= 3, "PID 1-ish inside the namespace: {v}");
        }
        other => panic!("{other:?}"),
    }
    // SIGTERM → SIGKILL escalation on cancel must restore control even for PID 1.
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let err = session
        .call(
            RpcRequest {
                method: "eval".into(),
                params: serde_json::json!({"code": "while True: pass"}),
            },
            cancel,
        )
        .await
        .expect_err("cancelled");
    assert!(matches!(err, SandboxError::Cancelled));
    assert!(!session.is_alive());
}

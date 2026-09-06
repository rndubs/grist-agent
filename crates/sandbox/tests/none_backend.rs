//! `NoneBackend` (feature `dev-sandbox-none`): scrubbed env, timeout, cancel, stdin, exit
//! codes/signals, program allowlist. Runs on any Linux host with `bash`.
#![cfg(feature = "dev-sandbox-none")]

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::cancel::CancellationToken;
use kernel::host::Command;
use kernel::sandbox::{SandboxBackend, SandboxError};
use sandbox::NoneBackend;

fn backend() -> NoneBackend {
    NoneBackend::new(Arc::new(support::FsHost)).with_grace(Duration::from_millis(300))
}

fn bash(script: &str) -> Command {
    Command {
        program: "bash".into(),
        args: vec!["-c".into(), script.into()],
        cwd: None,
        env: Default::default(),
        stdin: None,
    }
}

#[test]
fn name_is_none() {
    assert_eq!(backend().name(), "none");
}

/// Outer half of the env test: re-runs this test binary with secret-looking variables set, so the
/// inner half runs in a process that really has them (edition 2024 makes `set_var` unsafe).
#[tokio::test]
async fn none_env_scrubbed() {
    let exe = std::env::current_exe().expect("current_exe");
    let out = tokio::process::Command::new(exe)
        .args([
            "--ignored",
            "--exact",
            "none_env_scrubbed_inner",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("SANDBOX_TEST_INNER", "1")
        .env("MY_API_KEY", "leak-api-key")
        .env("AWS_SECRET_ACCESS_KEY", "leak-aws")
        .env("GITHUB_TOKEN", "leak-token")
        .env("LANG", "C.UTF-8")
        .output()
        .await
        .expect("spawn inner");
    assert!(
        out.status.success(),
        "inner test failed:\n--- stdout ---\n{}\n--- stderr ---\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
#[ignore = "driven by none_env_scrubbed, which sets the secret-looking variables"]
async fn none_env_scrubbed_inner() {
    if std::env::var("SANDBOX_TEST_INNER").is_err() {
        eprintln!("skipping: run through none_env_scrubbed");
        return;
    }
    assert_eq!(
        std::env::var("MY_API_KEY").as_deref(),
        Ok("leak-api-key"),
        "sanity: the secret is in our env"
    );
    let policy = support::policy(&[], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(&policy, bash("env"), CancellationToken::new())
        .await
        .expect("env runs");
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let names: Vec<&str> = text
        .lines()
        .filter_map(|l| l.split_once('=').map(|(k, _)| k))
        .collect();
    for name in &names {
        assert!(
            ["PATH", "HOME", "LANG", "PWD", "SHLVL", "_"].contains(name),
            "unexpected variable `{name}` inside the tool; env was:\n{text}"
        );
        let upper = name.to_ascii_uppercase();
        assert!(!upper.contains("KEY") && !upper.contains("TOKEN") && !upper.contains("SECRET"));
    }
    assert!(
        text.lines().any(|l| l == "HOME=/tmp"),
        "HOME=/tmp is set:\n{text}"
    );
    assert!(
        text.lines().any(|l| l == "LANG=C.UTF-8"),
        "allowlisted LANG passes through:\n{text}"
    );
    assert!(!text.contains("leak"), "no secret value leaks:\n{text}");
}

#[tokio::test]
async fn timeout_is_enforced_and_the_process_is_gone() {
    let (_dir, work) = support::workdir();
    let pidfile = work.join("pid");
    let policy = support::policy(&[], Duration::from_secs(1));
    let mut cmd = bash("echo $$ > \"$0\"; exec sleep 30");
    cmd.args.push(pidfile.to_string_lossy().into_owned());
    let start = Instant::now();
    let err = backend()
        .launch_stateless(&policy, cmd, CancellationToken::new())
        .await
        .expect_err("times out");
    assert!(
        matches!(err, SandboxError::Timeout(d) if d == Duration::from_secs(1)),
        "{err}"
    );
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    assert!(
        support::wait_gone(pid, Duration::from_secs(3)).await,
        "pid {pid} still alive"
    );
}

#[tokio::test]
async fn cancel_mid_run_returns_cancelled() {
    let policy = support::policy(&[], Duration::from_secs(30));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        trigger.cancel();
    });
    let start = Instant::now();
    let err = backend()
        .launch_stateless(&policy, bash("sleep 30"), cancel)
        .await
        .expect_err("cancelled");
    assert!(matches!(err, SandboxError::Cancelled), "{err}");
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn stdin_is_delivered() {
    let policy = support::policy(&[], Duration::from_secs(10));
    let mut cmd = Command::new("cat");
    cmd.stdin = Some(b"hello from stdin".to_vec());
    let out = backend()
        .launch_stateless(&policy, cmd, CancellationToken::new())
        .await
        .expect("cat");
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, b"hello from stdin");
}

#[tokio::test]
async fn exit_codes_and_signals_are_reported() {
    let policy = support::policy(&[], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(
            &policy,
            bash("echo out; echo err >&2; exit 3"),
            CancellationToken::new(),
        )
        .await
        .expect("runs");
    assert_eq!(out.exit_code, Some(3));
    assert_eq!(out.signal, None);
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
    assert!(!out.timed_out);

    let out = backend()
        .launch_stateless(&policy, bash("kill -9 $$"), CancellationToken::new())
        .await
        .expect("runs");
    assert_eq!(out.exit_code, None);
    assert_eq!(out.signal, Some(9));
}

#[tokio::test]
async fn program_allowlist_is_enforced() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.programs = ["python3"].into_iter().map(str::to_owned).collect();
    let err = backend()
        .launch_stateless(&policy, bash("true"), CancellationToken::new())
        .await
        .expect_err("bash is not permitted");
    assert!(
        matches!(err, SandboxError::Launch(ref m) if m.contains("program not permitted")),
        "{err}"
    );

    let out = backend()
        .launch_stateless(&policy, Command::new("python3"), CancellationToken::new())
        .await;
    // python3 with no args and no stdin exits 0 at EOF; the point is that it was permitted.
    assert!(out.is_ok(), "{out:?}");
}

#[tokio::test]
async fn cwd_defaults_to_tmp_and_home_is_tmp() {
    let policy = support::policy(&[], Duration::from_secs(10));
    let out = backend()
        .launch_stateless(&policy, bash("pwd; echo $HOME"), CancellationToken::new())
        .await
        .expect("runs");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/tmp\n/tmp\n");
}

#[tokio::test]
async fn dropping_the_launch_terminates_the_child() {
    let (_dir, work) = support::workdir();
    let pidfile = work.join("pid");
    let policy = support::policy(&[], Duration::from_secs(30));
    let mut cmd = bash("echo $$ > \"$0\"; exec sleep 30");
    cmd.args.push(pidfile.to_string_lossy().into_owned());
    let backend = backend();
    let fut = backend.launch_stateless(&policy, cmd, CancellationToken::new());
    let fut = tokio::time::timeout(Duration::from_millis(500), fut);
    // The 500 ms timeout drops the inner launch future mid-run.
    assert!(fut.await.is_err(), "the launch was still running");
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .expect("pidfile")
        .trim()
        .parse()
        .expect("pid");
    assert!(
        support::wait_gone(pid, Duration::from_secs(3)).await,
        "pid {pid} still alive"
    );
}

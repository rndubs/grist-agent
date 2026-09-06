//! `Host::spawn`: exact environment (D10), stdin delivery, timeout with SIGTERM/SIGKILL
//! escalation, `terminate`, and the program allowlist.

mod common;

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use common::{host, proc_policy};
use kernel::{Command, Host, HostError, PolicyError};

fn env_cmd() -> Command {
    let mut cmd = Command::new("env");
    cmd.env.insert(
        "PATH".to_owned(),
        std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".to_owned()),
    );
    cmd
}

#[tokio::test]
async fn child_env_is_exactly_cmd_env() {
    // Cargo sets these in every test process; they must not leak into the child.
    assert!(std::env::var("CARGO_MANIFEST_DIR").is_ok());
    assert!(std::env::var("CARGO_PKG_NAME").is_ok());

    let mut cmd = env_cmd();
    cmd.env
        .insert("FAKE_API_KEY".to_owned(), "only-if-passed".to_owned());
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(10)), cmd)
        .await
        .unwrap();
    let out = child.wait().await.unwrap();
    assert_eq!(out.exit_code, Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let vars: BTreeMap<&str, &str> = stdout.lines().filter_map(|l| l.split_once('=')).collect();
    assert_eq!(
        vars.keys().copied().collect::<Vec<_>>(),
        vec!["FAKE_API_KEY", "PATH"],
        "child env must be exactly cmd.env: {stdout}"
    );
    assert!(!stdout.contains("CARGO_MANIFEST_DIR"));
    assert!(!stdout.contains("CARGO_PKG_NAME"));
}

#[tokio::test]
async fn parent_only_vars_are_absent_from_child() {
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(10)), env_cmd())
        .await
        .unwrap();
    let out = child.wait().await.unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.starts_with("PATH="), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
}

#[tokio::test]
async fn stdin_is_delivered_and_closed() {
    let mut cmd = Command::new("cat");
    cmd.stdin = Some(b"hello from stdin".to_vec());
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(10)), cmd)
        .await
        .unwrap();
    assert!(child.pid().is_some());
    let out = child.wait().await.unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert_eq!(out.stdout, b"hello from stdin");
    assert!(out.stderr.is_empty());
    assert!(!out.timed_out);
}

#[tokio::test]
async fn no_stdin_means_eof() {
    let child = host()
        .spawn(
            &proc_policy(&[], Duration::from_secs(10)),
            Command::new("cat"),
        )
        .await
        .unwrap();
    let out = child.wait().await.unwrap();
    assert_eq!(out.exit_code, Some(0));
    assert!(out.stdout.is_empty());
}

#[tokio::test]
async fn stderr_exit_code_cwd_and_args() {
    let tmp = tempfile::tempdir().unwrap();
    let mut cmd = Command::new("sh");
    cmd.args = vec!["-c".to_owned(), "pwd; echo oops >&2; exit 3".to_owned()];
    cmd.cwd = Some(tmp.path().to_path_buf());
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(10)), cmd)
        .await
        .unwrap();
    let out = child.wait().await.unwrap();
    assert_eq!(out.exit_code, Some(3));
    assert_eq!(out.signal, None);
    assert_eq!(
        String::from_utf8(out.stdout).unwrap().trim(),
        tmp.path().canonicalize().unwrap().to_string_lossy()
    );
    assert_eq!(out.stderr, b"oops\n");
}

#[tokio::test]
async fn timeout_terminates_the_child() {
    let mut cmd = Command::new("sleep");
    cmd.args = vec!["30".to_owned()];
    let started = Instant::now();
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_millis(300)), cmd)
        .await
        .unwrap();
    let pid = child.pid().unwrap();
    let out = child.wait().await.unwrap();
    assert!(out.timed_out);
    assert_eq!(out.exit_code, None);
    assert_eq!(out.signal, Some(nix::sys::signal::Signal::SIGTERM as i32));
    assert!(out.duration >= Duration::from_millis(300));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "escalation took too long"
    );
    // Reaped: the pid no longer answers.
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
}

#[tokio::test]
async fn timeout_escalates_to_sigkill_when_sigterm_is_ignored() {
    let mut cmd = Command::new("sh");
    cmd.args = vec!["-c".to_owned(), "trap '' TERM; sleep 30".to_owned()];
    let started = Instant::now();
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_millis(300)), cmd)
        .await
        .unwrap();
    let out = child.wait().await.unwrap();
    assert!(out.timed_out);
    assert_eq!(out.signal, Some(nix::sys::signal::Signal::SIGKILL as i32));
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(1300), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(10), "{elapsed:?}");
}

#[tokio::test]
async fn terminate_kills_a_running_child() {
    let mut cmd = Command::new("sleep");
    cmd.args = vec!["30".to_owned()];
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(60)), cmd)
        .await
        .unwrap();
    let pid = child.pid().unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    child.terminate(Duration::from_millis(500)).await.unwrap();
    assert!(t.elapsed() < Duration::from_secs(3));
    let out = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("wait returns after terminate")
        .unwrap();
    assert!(!out.timed_out);
    assert_eq!(out.signal, Some(nix::sys::signal::Signal::SIGTERM as i32));
    assert_eq!(
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), None),
        Err(nix::errno::Errno::ESRCH)
    );
    // A second wait returns the same output.
    assert_eq!(child.wait().await.unwrap(), out);
}

#[tokio::test]
async fn terminate_escalates_to_sigkill() {
    let mut cmd = Command::new("sh");
    cmd.args = vec!["-c".to_owned(), "trap '' TERM; sleep 30".to_owned()];
    let child = host()
        .spawn(&proc_policy(&[], Duration::from_secs(60)), cmd)
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Run wait concurrently so the exit is observed while terminate waits for grace.
    let (out, term) = tokio::join!(child.wait(), child.terminate(Duration::from_millis(300)));
    term.unwrap();
    let out = out.unwrap();
    assert_eq!(out.signal, Some(nix::sys::signal::Signal::SIGKILL as i32));
}

#[tokio::test]
async fn program_allowlist_is_enforced() {
    let policy = proc_policy(&["cat", "/usr/bin/env"], Duration::from_secs(5));
    let h = host();
    let r = h.spawn(&policy, Command::new("sleep")).await;
    assert!(
        matches!(r, Err(HostError::Denied(PolicyError::ProgramDenied(ref p))) if p == "sleep"),
        "{:?}",
        r.map(|_| ())
    );
    // Allowed by basename, by exact path, and by basename of a path.
    let out = h
        .spawn(&policy, Command::new("cat"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    let out = h
        .spawn(&policy, Command::new("/usr/bin/env"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
    let out = h
        .spawn(&policy, Command::new("/usr/bin/cat"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(out.exit_code, Some(0));
}

#[tokio::test]
async fn missing_program_is_not_found() {
    let r = host()
        .spawn(
            &proc_policy(&[], Duration::from_secs(5)),
            Command::new("/definitely/not/here"),
        )
        .await;
    assert!(
        matches!(r, Err(HostError::NotFound(_))),
        "{:?}",
        r.map(|_| ())
    );
}

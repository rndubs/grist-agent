//! Pure tests: `policy_to_args` for every policy field, env scrubbing, and the program check.
//! None of these need `bwrap`, a runtime, or a feature.

mod support;

use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;

use kernel::capability::{FsMode, NetAllow};
use kernel::host::Command;
use kernel::sandbox::SandboxError;
use sandbox::{check_program, policy_to_args, policy_to_args_with_env, scrubbed_env_with};

fn cmd() -> Command {
    Command {
        program: "bash".into(),
        args: vec!["-c".into(), "echo hi".into()],
        cwd: Some("/work/repo".into()),
        env: Default::default(),
        stdin: None,
    }
}

fn lookup(name: &str) -> Option<String> {
    let table = support::env_map(&[
        ("MY_API_KEY", "leak-api-key"),
        ("AWS_SECRET_ACCESS_KEY", "leak-aws"),
        ("HOME", "/home/me"),
        ("PATH", "/usr/bin:/bin"),
        ("LANG", "C.UTF-8"),
    ]);
    table.get(name).cloned()
}

fn args_of(policy: &kernel::sandbox::SandboxPolicy, cmd: &Command) -> Vec<String> {
    policy_to_args_with_env(policy, cmd, lookup).expect("policy_to_args")
}

fn window(args: &[String], needle: &[&str]) -> Option<usize> {
    args.windows(needle.len())
        .position(|w| w.iter().zip(needle).all(|(a, b)| a == b))
}

fn pos(args: &[String], s: &str) -> usize {
    args.iter()
        .position(|a| a == s)
        .unwrap_or_else(|| panic!("{s} not in {args:?}"))
}

#[test]
fn fixed_prefix_and_suffix() {
    let policy = support::policy(&[], Duration::from_secs(10));
    let args = args_of(&policy, &cmd());
    assert!(
        window(
            &args,
            &[
                "--unshare-user",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts"
            ]
        )
        .is_some()
    );
    assert!(window(&args, &["--uid", "1000", "--gid", "1000"]).is_some());
    assert!(window(&args, &["--ro-bind", "/", "/"]).is_some());
    assert!(window(&args, &["--proc", "/proc", "--dev", "/dev"]).is_some());
    let tail = window(
        &args,
        &[
            "--die-with-parent",
            "--new-session",
            "--",
            "bash",
            "-c",
            "echo hi",
        ],
    )
    .expect("tail");
    assert_eq!(
        tail + 6,
        args.len(),
        "nothing follows the program and its args"
    );
}

#[test]
fn mounts_map_to_binds_most_specific_last() {
    let policy = support::policy(
        &[
            (Path::new("/work/repo/sub"), FsMode::Rw),
            (Path::new("/work"), FsMode::Ro),
            (Path::new("/opt/tools"), FsMode::Ro),
        ],
        Duration::from_secs(10),
    );
    let args = args_of(&policy, &cmd());
    let ro_work = window(&args, &["--ro-bind", "/work", "/work"]).expect("ro /work");
    let rw_sub = window(&args, &["--bind", "/work/repo/sub", "/work/repo/sub"]).expect("rw sub");
    let ro_opt = window(&args, &["--ro-bind", "/opt/tools", "/opt/tools"]).expect("ro opt");
    let base = window(&args, &["--ro-bind", "/", "/"]).expect("base");
    assert!(
        base < ro_work && base < ro_opt,
        "the base image is bound first"
    );
    assert!(
        ro_work < rw_sub,
        "the deeper mount comes after its parent so it wins"
    );
    assert!(window(&args, &["--bind", "/work", "/work"]).is_none());
}

#[test]
fn scratch_tmpfs_precedes_the_policy_mounts_so_a_mount_under_tmp_wins() {
    // bwrap applies operations in argv order: a `--tmpfs /tmp` after `--bind /tmp/x /tmp/x`
    // would hide the bind. Found on the first bwrap host, where tempdirs live under /tmp.
    let policy = support::policy(
        &[
            (Path::new("/tmp/work-abc"), FsMode::Rw),
            (Path::new("/opt/tools"), FsMode::Ro),
        ],
        Duration::from_secs(10),
    );
    let args = args_of(&policy, &cmd());
    let tmpfs = window(&args, &["--tmpfs", "/tmp"]).expect("tmpfs");
    let under_tmp = window(&args, &["--bind", "/tmp/work-abc", "/tmp/work-abc"]).expect("bind");
    let other = window(&args, &["--ro-bind", "/opt/tools", "/opt/tools"]).expect("ro opt");
    let base = window(&args, &["--ro-bind", "/", "/"]).expect("base");
    assert!(
        base < tmpfs,
        "the base image is bound before the scratch tmpfs"
    );
    assert!(
        tmpfs < under_tmp,
        "a policy mount under /tmp comes after the tmpfs"
    );
    assert!(tmpfs < other, "every policy mount comes after the tmpfs");
}

#[test]
fn tmpfs_is_sized_in_bytes_before_the_tmpfs_flag() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.scratch_tmpfs_mb = 256;
    let args = args_of(&policy, &cmd());
    assert!(
        window(&args, &["--size", "268435456", "--tmpfs", "/tmp"]).is_some(),
        "{args:?}"
    );
    policy.scratch_tmpfs_mb = 0;
    let args = args_of(&policy, &cmd());
    assert!(
        window(&args, &["--size", "1048576", "--tmpfs", "/tmp"]).is_some(),
        "never 0"
    );
}

#[test]
fn network_off_unshares_net_and_on_shares_it() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    let args = args_of(&policy, &cmd());
    assert!(args.contains(&"--unshare-net".to_string()));
    assert!(!args.contains(&"--share-net".to_string()));

    policy.net.enabled = true;
    policy.net.allow = NetAllow::Any;
    let args = args_of(&policy, &cmd());
    assert!(args.contains(&"--share-net".to_string()));
    assert!(!args.contains(&"--unshare-net".to_string()));
}

#[test]
fn env_is_scrubbed_to_the_allowlist_with_home_forced() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.env_allowlist = ["PATH", "HOME"].into_iter().map(str::to_owned).collect();
    let args = args_of(&policy, &cmd());
    let setenvs: Vec<(&str, &str)> = args
        .windows(3)
        .filter(|w| w[0] == "--setenv")
        .map(|w| (w[1].as_str(), w[2].as_str()))
        .collect();
    assert_eq!(setenvs, vec![("HOME", "/tmp"), ("PATH", "/usr/bin:/bin")]);
    assert!(
        !args.iter().any(|a| a.contains("leak")),
        "no secret value leaks: {args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "LANG"),
        "LANG is set in the env but not allowlisted"
    );
    assert!(pos(&args, "--clearenv") < pos(&args, "--setenv"));
}

#[test]
fn tool_env_is_added_but_cannot_override_home() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.env_allowlist = ["PATH"].into_iter().map(str::to_owned).collect();
    let mut c = cmd();
    c.env = support::env_map(&[("PYTHONDONTWRITEBYTECODE", "1"), ("HOME", "/elsewhere")]);
    let args = args_of(&policy, &c);
    assert!(window(&args, &["--setenv", "PYTHONDONTWRITEBYTECODE", "1"]).is_some());
    assert!(window(&args, &["--setenv", "HOME", "/tmp"]).is_some());
    assert!(window(&args, &["--setenv", "HOME", "/elsewhere"]).is_none());
}

#[test]
fn secret_like_allowlist_name_is_refused() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.env_allowlist.insert("MY_API_KEY".into());
    let err = policy_to_args_with_env(&policy, &cmd(), lookup).expect_err("refused");
    assert!(
        matches!(err, SandboxError::Launch(ref m) if m.contains("MY_API_KEY")),
        "{err}"
    );
    let err = scrubbed_env_with(&policy, &cmd(), lookup).expect_err("refused");
    assert!(matches!(err, SandboxError::Launch(_)));
}

#[test]
fn chdir_uses_cwd_or_tmp() {
    let policy = support::policy(&[], Duration::from_secs(10));
    let args = args_of(&policy, &cmd());
    assert!(window(&args, &["--chdir", "/work/repo"]).is_some());
    let mut c = cmd();
    c.cwd = None;
    let args = args_of(&policy, &c);
    assert!(window(&args, &["--chdir", "/tmp"]).is_some());
}

#[test]
fn real_process_env_is_the_default_lookup() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    policy.env_allowlist = ["PATH"].into_iter().map(str::to_owned).collect();
    let args = policy_to_args(&policy, &cmd()).expect("args");
    let path = std::env::var("PATH").expect("PATH is set in the test process");
    assert!(window(&args, &["--setenv", "PATH", &path]).is_some());
    assert!(window(&args, &["--setenv", "HOME", "/tmp"]).is_some());
}

#[test]
fn program_check() {
    let mut policy = support::policy(&[], Duration::from_secs(10));
    assert!(
        check_program(&policy, &cmd()).is_ok(),
        "empty set permits anything"
    );

    policy.programs = ["bash"].into_iter().map(str::to_owned).collect();
    assert!(check_program(&policy, &cmd()).is_ok());
    let mut abs = cmd();
    abs.program = "/usr/bin/bash".into();
    assert!(check_program(&policy, &abs).is_ok(), "basename matches");
    let mut py = cmd();
    py.program = "python3".into();
    let err = check_program(&policy, &py).expect_err("python3 is not permitted");
    assert!(
        matches!(err, SandboxError::Launch(ref m) if m.contains("program not permitted")),
        "{err}"
    );

    policy.programs = ["/usr/bin/bash"].into_iter().map(str::to_owned).collect();
    assert!(
        check_program(&policy, &abs).is_ok(),
        "verbatim absolute path"
    );
    assert!(
        check_program(&policy, &cmd()).is_err(),
        "a bare name does not match an absolute grant"
    );
    let _: BTreeSet<String> = policy.programs;
}

#[test]
fn derive_policy_is_re_exported() {
    let caps = ["fs.ro:/work".parse::<kernel::Capability>().unwrap()];
    let grants = ["fs.rw:/work".parse::<kernel::Capability>().unwrap()];
    let policy = sandbox::derive_policy(&caps, &grants).expect("derive");
    assert_eq!(policy.mounts.len(), 1);
    assert_eq!(policy.mounts[0].mode, FsMode::Ro);
}

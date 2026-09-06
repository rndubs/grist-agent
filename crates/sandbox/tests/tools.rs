//! The six base tools against the real-filesystem test host. `read`/`write`/`edit` and the
//! declaration helpers run in every build; `bash`, `run_script` and `python` need a backend and
//! run under `dev-sandbox-none`.

mod support;

use std::path::Path;
use std::time::Duration;

use kernel::capability::{Capability, FsMode};
use kernel::sandbox::SandboxLimits;
use kernel::tool::{Tool, ToolError, ToolKind, ToolResult};
use sandbox::tools::{EditTool, ReadTool, WriteTool};
use sandbox::{derive_policy_with, tool_decls};
use serde_json::{Value, json};

fn value(r: Result<ToolResult, ToolError>) -> Value {
    match r {
        Ok(ToolResult::Value(v)) => v,
        other => panic!("expected a Value: {other:?}"),
    }
}

fn grants(work: &Path) -> Vec<Capability> {
    vec![
        Capability::Fs {
            path: work.to_path_buf(),
            mode: FsMode::Rw,
        },
        Capability::Proc {
            program: "bash".into(),
        },
        Capability::Proc {
            program: "python3".into(),
        },
    ]
}

/// The policy the kernel would derive for `tool` under the default agent's grants.
fn derived(tool: &dyn Tool, work: &Path) -> kernel::sandbox::SandboxPolicy {
    let limits = SandboxLimits {
        timeout: Duration::from_secs(10),
        ..SandboxLimits::default()
    };
    derive_policy_with(&tool.capabilities(), &grants(work), &limits).expect("covered by grants")
}

#[test]
fn tool_decls_match_the_profile_schema_table() {
    let decls = tool_decls("/work/repo");
    let rw = Capability::Fs {
        path: "/work/repo".into(),
        mode: FsMode::Rw,
    };
    let ro = Capability::Fs {
        path: "/work/repo".into(),
        mode: FsMode::Ro,
    };
    let bash = Capability::Proc {
        program: "bash".into(),
    };
    let py = Capability::Proc {
        program: "python3".into(),
    };
    let expected = vec![
        ("read", ToolKind::Stateless, vec![ro]),
        ("write", ToolKind::Stateless, vec![rw.clone()]),
        ("edit", ToolKind::Stateless, vec![rw.clone()]),
        ("bash", ToolKind::Stateless, vec![rw.clone(), bash.clone()]),
        ("run_script", ToolKind::Stateless, vec![rw.clone(), bash]),
        ("python", ToolKind::Session, vec![rw, py]),
    ];
    let got: Vec<(&str, ToolKind, Vec<Capability>)> = decls
        .iter()
        .map(|(n, k, c)| (n.as_str(), *k, c.clone()))
        .collect();
    assert_eq!(got, expected);
    for (_, _, caps) in &decls {
        derive_policy_with(
            caps,
            &grants(Path::new("/work/repo")),
            &SandboxLimits::default(),
        )
        .expect("every base tool is covered by the default agent's grants");
    }
}

#[test]
fn base_tools_and_schemas() {
    let tools = sandbox::base_tools("/work/repo", std::sync::Arc::new(support::StubBackend));
    let names: Vec<&str> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(
        names,
        ["read", "write", "edit", "bash", "run_script", "python"]
    );
    for t in &tools {
        let schema = t.schema();
        assert_eq!(schema["type"], json!("object"), "{}", t.name());
        assert!(schema["required"].is_array(), "{}", t.name());
        assert!(!t.description().is_empty());
        assert!(kernel::tool::is_valid_tool_name(t.name()));
        match t.kind() {
            ToolKind::Session => assert!(t.session_command().is_some()),
            ToolKind::Stateless => assert!(t.session_command().is_none()),
        }
        let def = t.definition();
        assert_eq!(def.name, t.name());
    }
    let python = &tools[5];
    let cmd = python.session_command().unwrap();
    assert_eq!(cmd.program, "python3");
    assert_eq!(cmd.args[0], "-c");
    assert!(cmd.args[1].contains("jsonrpc"));
    assert_eq!(cmd.cwd.as_deref(), Some(Path::new("/work/repo")));
    assert!(cmd.env.is_empty());
}

#[test]
fn relative_workdir_becomes_absolute() {
    let decls = tool_decls("rel/dir");
    match &decls[0].2[0] {
        Capability::Fs { path, .. } => assert!(path.is_absolute()),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn read_returns_numbered_lines_and_pages() {
    let (_dir, work) = support::workdir();
    std::fs::write(work.join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
    let tool = ReadTool::new(&work);
    let fx = support::Fixture::new(derived(&tool, &work), Box::new(support::StubBackend));
    let ctx = fx.ctx(None);

    let v = value(tool.invoke(&ctx, json!({"path": "f.txt"})).await);
    assert_eq!(v["lines"], json!(5));
    assert_eq!(v["truncated"], json!(false));
    assert_eq!(
        v["content"],
        json!("     1\ta\n     2\tb\n     3\tc\n     4\td\n     5\te\n")
    );

    let v = value(
        tool.invoke(&ctx, json!({"path": "f.txt", "offset": 2, "limit": 2}))
            .await,
    );
    assert_eq!(v["content"], json!("     2\tb\n     3\tc\n"));
    assert_eq!(v["truncated"], json!(true));

    let abs = work.join("f.txt").to_string_lossy().into_owned();
    let v = value(tool.invoke(&ctx, json!({"path": abs, "offset": 5})).await);
    assert_eq!(v["content"], json!("     5\te\n"));

    let err = tool
        .invoke(&ctx, json!({"path": "missing.txt"}))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Failed(_)), "{err}");
    let err = tool.invoke(&ctx, json!({"path": 3})).await.err().unwrap();
    assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
}

#[tokio::test]
async fn read_outside_the_policy_is_denied() {
    let (_dir, work) = support::workdir();
    let (_outside_dir, outside) = support::workdir();
    std::fs::write(outside.join("secret.txt"), "nope").unwrap();
    let tool = ReadTool::new(&work);
    let fx = support::Fixture::new(derived(&tool, &work), Box::new(support::StubBackend));
    let ctx = fx.ctx(None);
    let path = outside.join("secret.txt").to_string_lossy().into_owned();
    let err = tool
        .invoke(&ctx, json!({"path": path}))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Denied(_)), "{err}");
    // `..` escapes are lexically collapsed and then denied by the Host check.
    let err = tool
        .invoke(&ctx, json!({"path": format!("../{}/secret.txt", outside.file_name().unwrap().to_str().unwrap())}))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Denied(_)), "{err}");
}

#[tokio::test]
async fn read_non_utf8_fails() {
    let (_dir, work) = support::workdir();
    std::fs::write(work.join("bin"), [0xff, 0xfe, 0x00, 0x80]).unwrap();
    let tool = ReadTool::new(&work);
    let fx = support::Fixture::new(derived(&tool, &work), Box::new(support::StubBackend));
    let err = tool
        .invoke(&fx.ctx(None), json!({"path": "bin"}))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, ToolError::Failed(ref m) if m.contains("UTF-8")),
        "{err}"
    );
}

#[tokio::test]
async fn write_under_a_read_only_policy_is_denied() {
    let (_dir, work) = support::workdir();
    // The policy derived for `read` (fs.ro) cannot be used to write.
    let ro_policy = derived(&ReadTool::new(&work), &work);
    assert_eq!(ro_policy.mounts[0].mode, FsMode::Ro);
    let tool = WriteTool::new(&work);
    let fx = support::Fixture::new(ro_policy, Box::new(support::StubBackend));
    let err = tool
        .invoke(&fx.ctx(None), json!({"path": "new.txt", "content": "x"}))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Denied(_)), "{err}");
    assert!(!work.join("new.txt").exists());
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let (_dir, work) = support::workdir();
    let write = WriteTool::new(&work);
    let fx = support::Fixture::new(derived(&write, &work), Box::new(support::StubBackend));
    let ctx = fx.ctx(None);
    let v = value(
        write
            .invoke(&ctx, json!({"path": "out.txt", "content": "héllo\n"}))
            .await,
    );
    assert_eq!(v["bytes"], json!(7));
    assert_eq!(
        std::fs::read_to_string(work.join("out.txt")).unwrap(),
        "héllo\n"
    );
    let v = value(
        write
            .invoke(&ctx, json!({"path": "out.txt", "content": ""}))
            .await,
    );
    assert_eq!(v["bytes"], json!(0));
    assert_eq!(std::fs::read_to_string(work.join("out.txt")).unwrap(), "");

    let (_o, outside) = support::workdir();
    let path = outside.join("x").to_string_lossy().into_owned();
    let err = write
        .invoke(&ctx, json!({"path": path, "content": "x"}))
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Denied(_)));
}

#[tokio::test]
async fn edit_requires_a_unique_match_unless_replace_all() {
    let (_dir, work) = support::workdir();
    let f = work.join("e.txt");
    std::fs::write(&f, "foo bar foo\n").unwrap();
    let tool = EditTool::new(&work);
    let fx = support::Fixture::new(derived(&tool, &work), Box::new(support::StubBackend));
    let ctx = fx.ctx(None);

    let err = tool
        .invoke(
            &ctx,
            json!({"path": "e.txt", "old_string": "foo", "new_string": "baz"}),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, ToolError::InvalidInput(ref m) if m.contains("2 times")),
        "{err}"
    );
    assert_eq!(
        std::fs::read_to_string(&f).unwrap(),
        "foo bar foo\n",
        "untouched"
    );

    let err = tool
        .invoke(
            &ctx,
            json!({"path": "e.txt", "old_string": "zzz", "new_string": "baz"}),
        )
        .await
        .err()
        .unwrap();
    assert!(
        matches!(err, ToolError::InvalidInput(ref m) if m.contains("not found")),
        "{err}"
    );

    let err = tool
        .invoke(
            &ctx,
            json!({"path": "e.txt", "old_string": "", "new_string": "baz"}),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::InvalidInput(_)));

    let v = value(
        tool.invoke(
            &ctx,
            json!({"path": "e.txt", "old_string": "bar", "new_string": "qux"}),
        )
        .await,
    );
    assert_eq!(v["replacements"], json!(1));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "foo qux foo\n");

    let v = value(
        tool.invoke(
            &ctx,
            json!({"path": "e.txt", "old_string": "foo", "new_string": "baz", "replace_all": true}),
        )
        .await,
    );
    assert_eq!(v["replacements"], json!(2));
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "baz qux baz\n");
}

#[tokio::test]
async fn edit_under_a_read_only_policy_is_denied() {
    let (_dir, work) = support::workdir();
    std::fs::write(work.join("e.txt"), "abc").unwrap();
    let tool = EditTool::new(&work);
    let fx = support::Fixture::new(
        derived(&ReadTool::new(&work), &work),
        Box::new(support::StubBackend),
    );
    let err = tool
        .invoke(
            &fx.ctx(None),
            json!({"path": "e.txt", "old_string": "b", "new_string": "x"}),
        )
        .await
        .err()
        .unwrap();
    assert!(matches!(err, ToolError::Denied(_)), "{err}");
    assert_eq!(std::fs::read_to_string(work.join("e.txt")).unwrap(), "abc");
}

#[cfg(feature = "dev-sandbox-none")]
mod sandboxed {
    use super::*;
    use std::sync::Arc;
    use std::time::Instant;

    use kernel::sandbox::SandboxBackend;
    use kernel::task::{TaskId, TaskStatus};
    use sandbox::NoneBackend;
    use sandbox::tools::{BashTool, PythonTool, RunScriptTool};

    fn none() -> Arc<dyn SandboxBackend> {
        Arc::new(NoneBackend::new(Arc::new(support::FsHost)).with_grace(Duration::from_millis(300)))
    }

    fn none_box() -> Box<dyn SandboxBackend> {
        Box::new(NoneBackend::new(Arc::new(support::FsHost)).with_grace(Duration::from_millis(300)))
    }

    #[tokio::test]
    async fn bash_returns_stdout_and_runs_in_the_workdir() {
        let (_dir, work) = support::workdir();
        let tool = BashTool::new(&work);
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        let ctx = fx.ctx(None);
        let v = value(
            tool.invoke(&ctx, json!({"command": "echo hi; pwd; echo $HOME; exit 2"}))
                .await,
        );
        assert_eq!(v["exit_code"], json!(2));
        assert_eq!(
            v["stdout"],
            json!(format!("hi\n{}\n/tmp\n", work.display()))
        );
        assert_eq!(v["stderr"], json!(""));
        assert_eq!(v["signal"], Value::Null);
        assert_eq!(v["timed_out"], json!(false));
        assert!(v["duration_ms"].is_u64());

        std::fs::create_dir(work.join("sub")).unwrap();
        let v = value(
            tool.invoke(&ctx, json!({"command": "pwd", "cwd": "sub"}))
                .await,
        );
        assert_eq!(
            v["stdout"],
            json!(format!("{}\n", work.join("sub").display()))
        );
    }

    #[tokio::test]
    async fn bash_timeout_secs_may_only_lower_the_policy_timeout() {
        let (_dir, work) = support::workdir();
        let tool = BashTool::new(&work);
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        let ctx = fx.ctx(None);
        let err = tool
            .invoke(&ctx, json!({"command": "true", "timeout_secs": 3600}))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ToolError::InvalidInput(_)), "{err}");
        let start = Instant::now();
        let err = tool
            .invoke(&ctx, json!({"command": "sleep 30", "timeout_secs": 1}))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, ToolError::Timeout(d) if d == Duration::from_secs(1)),
            "{err}"
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn bash_cancel_is_reported_as_cancelled() {
        let (_dir, work) = support::workdir();
        let tool = BashTool::new(&work);
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        let ctx = fx.ctx(None);
        let trigger = fx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            trigger.cancel();
        });
        let err = tool
            .invoke(&ctx, json!({"command": "sleep 30"}))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ToolError::Cancelled), "{err}");
    }

    #[tokio::test]
    async fn run_script_returns_a_task_and_the_future_yields_the_exit_outcome() {
        let (_dir, work) = support::workdir();
        std::fs::write(
            work.join("job.sh"),
            "echo \"args: $*\"; echo oops >&2; exit 4\n",
        )
        .unwrap();
        let tool = RunScriptTool::new(&work, none());
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        let ctx = fx.ctx(None);
        let result = tool
            .invoke(&ctx, json!({"path": "job.sh", "args": ["a", "b"]}))
            .await
            .expect("task started");
        let handle = match result {
            ToolResult::Task(h) => h,
            other => panic!("expected a Task: {other:?}"),
        };
        assert_eq!(handle.id, TaskId("t3-tu1".into()), "deterministic id");
        assert_eq!(handle.id, ctx.task_id());
        assert_eq!(handle.status, TaskStatus::Running);
        assert_eq!(handle.eta, None);
        let script = work.join("job.sh").to_string_lossy().into_owned();
        assert_eq!(
            handle.description.as_deref(),
            Some(format!("{script} a b").as_str())
        );
        assert_eq!(handle.check_hint.as_ref().unwrap()["path"], json!(script));

        let mut futures = fx.registrar.take();
        assert_eq!(futures.len(), 1, "exactly one future registered");
        let (id, done) = futures.pop().unwrap();
        assert_eq!(id, handle.id);
        let outcome = done.await;
        assert!(outcome.is_error, "exit 4 is an error");
        assert!(outcome.artifact_handles.is_empty());
        let content = match outcome.content {
            kernel::content::ToolResultContent::Json(v) => v,
            other => panic!("{other:?}"),
        };
        assert_eq!(content["exit_code"], json!(4));
        assert_eq!(content["stdout"], json!("args: a b\n"));
        assert_eq!(content["stderr"], json!("oops\n"));
        assert_eq!(content["timed_out"], json!(false));
    }

    #[tokio::test]
    async fn run_script_success_is_not_an_error() {
        let (_dir, work) = support::workdir();
        std::fs::write(work.join("ok.sh"), "echo done\n").unwrap();
        let tool = RunScriptTool::new(&work, none());
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        tool.invoke(&fx.ctx(None), json!({"path": "ok.sh"}))
            .await
            .unwrap();
        let (_, done) = fx.registrar.take().pop().unwrap();
        let outcome = done.await;
        assert!(!outcome.is_error);
    }

    #[tokio::test]
    async fn run_script_timeout_is_an_error_outcome() {
        let (_dir, work) = support::workdir();
        std::fs::write(work.join("slow.sh"), "sleep 30\n").unwrap();
        let tool = RunScriptTool::new(&work, none());
        let mut policy = derived(&tool, &work);
        policy.timeout = Duration::from_secs(1);
        let fx = support::Fixture::new(policy, none_box());
        tool.invoke(&fx.ctx(None), json!({"path": "slow.sh"}))
            .await
            .unwrap();
        let (_, done) = fx.registrar.take().pop().unwrap();
        let outcome = done.await;
        assert!(outcome.is_error);
        let content = match outcome.content {
            kernel::content::ToolResultContent::Json(v) => v,
            other => panic!("{other:?}"),
        };
        assert_eq!(content["timed_out"], json!(true));
    }

    #[tokio::test]
    async fn dropping_the_run_script_future_terminates_the_process() {
        let (_dir, work) = support::workdir();
        let pidfile = work.join("pid");
        std::fs::write(
            work.join("bg.sh"),
            format!("echo $$ > {}; exec sleep 30\n", pidfile.display()),
        )
        .unwrap();
        let tool = RunScriptTool::new(&work, none());
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        tool.invoke(&fx.ctx(None), json!({"path": "bg.sh"}))
            .await
            .unwrap();
        let mut pid = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pidfile)
                && let Ok(p) = s.trim().parse::<u32>()
            {
                pid = Some(p);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let pid = pid.expect("the script started and wrote its pid");
        assert!(!support::process_gone(pid), "running before the drop");
        drop(fx.registrar.take()); // CancelScope::Task: the kernel drops the future.
        assert!(
            support::wait_gone(pid, Duration::from_secs(3)).await,
            "pid {pid} still alive"
        );
    }

    #[tokio::test]
    async fn run_script_refuses_a_registrar_that_cannot_watch() {
        let (_dir, work) = support::workdir();
        std::fs::write(work.join("x.sh"), "true\n").unwrap();
        let tool = RunScriptTool::new(&work, none());
        let fx = support::Fixture::new(derived(&tool, &work), none_box());
        let ctx = kernel::tool::ToolContext::new(
            &fx.host,
            fx.cancel.clone(),
            &fx.session_id,
            fx.turn,
            &fx.tool_use_id,
            &fx.policy,
            fx.sandbox.as_ref(),
            &fx.artifacts,
            None,
            &kernel::tool::NoTaskRegistrar,
        );
        let err = tool
            .invoke(&ctx, json!({"path": "x.sh"}))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ToolError::InvalidTaskHandle(_)), "{err}");
    }

    #[tokio::test]
    async fn run_script_checks_the_backend_matches() {
        let (_dir, work) = support::workdir();
        let tool = RunScriptTool::new(&work, none());
        let fx = support::Fixture::new(derived(&tool, &work), Box::new(support::StubBackend));
        let err = tool
            .invoke(&fx.ctx(None), json!({"path": "x.sh"}))
            .await
            .err()
            .unwrap();
        assert!(
            matches!(err, ToolError::Failed(ref m) if m.contains("stub")),
            "{err}"
        );
    }

    #[tokio::test]
    async fn python_tool_through_a_none_session() {
        let (_dir, work) = support::workdir();
        let tool = PythonTool::new(&work);
        let policy = derived(&tool, &work);
        let backend = none();
        let session = backend
            .launch_session(&policy, tool.session_command().unwrap())
            .await
            .expect("launch");
        let fx = support::Fixture::new(policy, none_box());
        let ctx = fx.ctx(Some(session.as_ref()));

        let v = value(tool.invoke(&ctx, json!({"code": "x = 41"})).await);
        assert_eq!(v["ok"], json!(true));
        let v = value(tool.invoke(&ctx, json!({"code": "x + 1"})).await);
        assert_eq!(v["value"], json!("42"));
        let v = value(
            tool.invoke(&ctx, json!({"code": "import os; os.getcwd()"}))
                .await,
        );
        assert_eq!(
            v["value"],
            json!(format!("'{}'", work.display())),
            "python repr"
        );
        let v = value(tool.invoke(&ctx, json!({"code": "1/0"})).await);
        assert_eq!(v["ok"], json!(false), "user-code errors are Ok(Value)");
        assert_eq!(v["error"]["type"], json!("ZeroDivisionError"));

        let stateless = fx.ctx(None);
        let err = tool
            .invoke(&stateless, json!({"code": "1"}))
            .await
            .err()
            .unwrap();
        assert!(matches!(err, ToolError::Failed(_)), "{err}");
        session.terminate().await.unwrap();

        let err = tool.invoke(&ctx, json!({"code": "1"})).await.err().unwrap();
        assert!(matches!(err, ToolError::Failed(_)), "dead session: {err}");
    }
}

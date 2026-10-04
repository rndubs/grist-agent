//! Out-of-process tools invoked through the existing launchers (the `None` backend, D14): the
//! Stateless path (a process per call, request on stdin) and the Session path (one process, RPC
//! over `JsonRpcSession`), plus the `initialize` / `tools/list` probe. Needs `python3`. The last
//! test repeats both paths under `BwrapBackend`; it skips without `bwrap` unless
//! `GRIST_REQUIRE_BWRAP=1` (what the CI `test` job sets).

mod support;

use std::sync::Arc;
use std::time::Duration;

use ext::{ExtError, Manifest, ProcessTool, admit, policy_for, probe};
use kernel::SessionId;
use kernel::artifact::NoopArtifactStore;
use kernel::cancel::CancellationToken;
use kernel::content::ContentBlock;
use kernel::sandbox::{SandboxBackend, SandboxLimits, SandboxPolicy, SessionProcess};
use kernel::tool::{NoTaskRegistrar, Tool, ToolContext, ToolError, ToolKind, ToolResult};
use sandbox::bwrap::program_available;
use sandbox::{BwrapBackend, NoneBackend};
use serde_json::{Value, json};
use support::Fixture;

fn backend() -> NoneBackend {
    let host = Arc::new(host::NativeHost::new(
        Arc::new(kernel::Redactor::new()),
        Arc::new(host::MapSecretSource::empty()),
    ));
    NoneBackend::new(host).with_grace(Duration::from_millis(500))
}

fn limits() -> SandboxLimits {
    SandboxLimits {
        timeout: Duration::from_secs(20),
        ..SandboxLimits::default()
    }
}

/// Invoke `tool` once under `policy`, with `session` for Session tools.
async fn invoke(
    tool: &ProcessTool,
    backend: &dyn SandboxBackend,
    policy: &SandboxPolicy,
    session: Option<&dyn SessionProcess>,
    input: Value,
) -> Result<ToolResult, ToolError> {
    let host = host::NativeHost::new(
        Arc::new(kernel::Redactor::new()),
        Arc::new(host::MapSecretSource::empty()),
    );
    let sid = SessionId("s_ext".into());
    let ctx = ToolContext::new(
        &host,
        CancellationToken::new(),
        &sid,
        1,
        "c1",
        policy,
        backend,
        &NoopArtifactStore,
        session,
        &NoTaskRegistrar,
    );
    tool.invoke(&ctx, input).await
}

fn structured(r: Result<ToolResult, ToolError>) -> Value {
    match r {
        Ok(ToolResult::Value(v)) => v,
        other => panic!("expected a structured value, got {other:?}"),
    }
}

#[tokio::test]
async fn stateless_tools_run_a_process_per_call() {
    let fx = Fixture::new();
    std::fs::write(
        fx.workdir.join("notes.txt"),
        "one two two\nthree three three\n",
    )
    .unwrap();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let policy = policy_for(&m, &fx.grants(), &limits()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    let b = backend();

    let counts =
        structured(invoke(&tools[0], &b, &policy, None, json!({"path": "notes.txt"})).await);
    assert_eq!(counts, json!({"lines": 2, "words": 6, "chars": 30}));
    let top = structured(
        invoke(
            &tools[1],
            &b,
            &policy,
            None,
            json!({"path": "notes.txt", "limit": 2}),
        )
        .await,
    );
    assert_eq!(
        top,
        json!({"words": [{"word": "three", "count": 3}, {"word": "two", "count": 2}]})
    );

    // A failure in the tool's own code is an error result the model sees, not a crash.
    let missing = invoke(&tools[0], &b, &policy, None, json!({"path": "nope.txt"})).await;
    assert!(
        matches!(&missing, Err(ToolError::Failed(m)) if m.contains("cannot read nope.txt")),
        "{missing:?}"
    );
}

#[tokio::test]
async fn the_launcher_refuses_a_program_outside_the_policy() {
    let fx = Fixture::new();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    // A policy derived for some other tool (bash only) does not let this one run python3.
    let other = kernel::sandbox::derive_policy_with(
        &support::caps(&["proc:bash"]),
        &support::caps(&["proc:bash"]),
        &limits(),
    )
    .unwrap();
    let r = invoke(&tools[0], &backend(), &other, None, json!({"path": "x"})).await;
    assert!(matches!(r, Err(ToolError::Denied(_))), "{r:?}");
}

#[tokio::test]
async fn session_tools_keep_one_process_and_speak_rpc() {
    let fx = Fixture::new();
    std::fs::write(fx.workdir.join("a.txt"), "alpha beta\n").unwrap();
    fx.edit_manifest(|t| t.replace("kind = \"stateless\"", "kind = \"session\""));
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    assert_eq!(m.kind, ToolKind::Session);
    let policy = policy_for(&m, &fx.grants(), &limits()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    let cmd = tools[0]
        .session_command()
        .expect("session tools have a command");
    assert_eq!(cmd.program, "python3");
    assert_eq!(cmd.cwd.as_deref(), Some(fx.workdir.as_path()));

    let b = backend();
    // What the kernel does on first invoke (§7.9): launch under the tool's derived policy.
    let session = b.launch_session(&policy, cmd).await.unwrap();
    for _ in 0..3 {
        let v = structured(
            invoke(
                &tools[0],
                &b,
                &policy,
                Some(session.as_ref()),
                json!({"path": "a.txt"}),
            )
            .await,
        );
        assert_eq!(v["words"], json!(2));
    }
    assert!(session.is_alive(), "one process served every call");

    // A stateless context for a session tool is a tool failure, not a panic.
    let r = invoke(&tools[0], &b, &policy, None, json!({"path": "a.txt"})).await;
    assert!(matches!(r, Err(ToolError::Failed(_))));
    session.terminate().await.unwrap();
    assert!(!session.is_alive());
}

#[tokio::test]
async fn text_only_results_become_text_blocks() {
    let fx = Fixture::new();
    // A server that answers with plain text content and no structured content.
    std::fs::write(
        fx.ext.join("server.py"),
        r#"import json, sys
for line in sys.stdin:
    req = json.loads(line)
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"],
                      "result": {"content": [{"type": "text", "text": "plain"}]}}), flush=True)
"#,
    )
    .unwrap();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let policy = policy_for(&m, &fx.grants(), &limits()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    let r = invoke(&tools[0], &backend(), &policy, None, json!({"path": "x"})).await;
    assert_eq!(
        r.unwrap(),
        ToolResult::Blocks(vec![ContentBlock::Text {
            text: "plain".into()
        }])
    );

    // A server that prints nothing: the failure carries its stderr.
    std::fs::write(
        fx.ext.join("server.py"),
        "import sys; sys.stderr.write('boom')\n",
    )
    .unwrap();
    let r = invoke(&tools[0], &backend(), &policy, None, json!({"path": "x"})).await;
    assert!(
        matches!(&r, Err(ToolError::Failed(m)) if m.contains("no response") && m.contains("boom")),
        "{r:?}"
    );
}

#[tokio::test]
async fn probe_checks_the_manifest_against_tools_list() {
    let fx = Fixture::new();
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let report = probe(&m, &backend(), &fx.grants(), &limits())
        .await
        .unwrap();
    assert_eq!(report.tools, ["count", "top_words"]);
    assert_eq!(report.server_info.unwrap()["name"], json!("text_stats"));

    // A manifest tool the server does not serve fails the probe.
    fx.edit_manifest(|t| {
        format!("{t}\n[[tools]]\nname = \"ghost\"\ndescription = \"x\"\ninput_schema = {{ type = \"object\" }}\n")
    });
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    assert!(matches!(
        probe(&m, &backend(), &fx.grants(), &limits()).await,
        Err(ExtError::Probe { message, .. }) if message.contains("ghost")
    ));
    // And the probe never starts an extension the grants do not cover.
    assert!(matches!(
        probe(&m, &backend(), &support::caps(&["proc:python3"]), &limits()).await,
        Err(ExtError::ExceedsGrants { .. })
    ));
}

#[tokio::test]
async fn both_kinds_run_under_bwrap() {
    if !program_available("bwrap") {
        assert_ne!(
            std::env::var("GRIST_REQUIRE_BWRAP").as_deref(),
            Ok("1"),
            "bwrap not on PATH but GRIST_REQUIRE_BWRAP=1"
        );
        println!("skipping: bwrap not available");
        return;
    }
    let host = Arc::new(host::NativeHost::new(
        Arc::new(kernel::Redactor::new()),
        Arc::new(host::MapSecretSource::empty()),
    ));
    let b = BwrapBackend::new(host).with_grace(Duration::from_millis(500));
    let fx = Fixture::new();
    std::fs::write(fx.workdir.join("w.txt"), "a b c\n").unwrap();

    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let policy = policy_for(&m, &fx.grants(), &limits()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    let v = structured(invoke(&tools[0], &b, &policy, None, json!({"path": "w.txt"})).await);
    assert_eq!(v["words"], json!(3));

    fx.edit_manifest(|t| t.replace("kind = \"stateless\"", "kind = \"session\""));
    let m = Manifest::load(&fx.ext, &fx.placeholders()).unwrap();
    let tools = admit(&m, &fx.grants(), &limits()).unwrap();
    let session = b
        .launch_session(&policy, tools[0].session_command().unwrap())
        .await
        .unwrap();
    for _ in 0..2 {
        let v = structured(
            invoke(
                &tools[0],
                &b,
                &policy,
                Some(session.as_ref()),
                json!({"path": "w.txt"}),
            )
            .await,
        );
        assert_eq!(v["chars"], json!(6));
    }
    session.terminate().await.unwrap();
}

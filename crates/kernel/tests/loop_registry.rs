//! P1.2: tool registry validation at construction (§7.7) and start-up warnings.

mod support;

use std::path::PathBuf;
use std::sync::Arc;

use kernel::loop_::{Kernel, KernelError};
use kernel::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn construction_rejects_bad_duplicate_ungranted_and_commandless_tools() {
    let setup =
        Setup::new(FakeProvider::responses(vec![])).tool(ValueTool::new("Bad Name", json!(1)));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::InvalidToolName(n)) if n == "Bad Name"
    ));
    let setup = Setup::new(FakeProvider::responses(vec![]))
        .tool(ValueTool::new("dup", json!(1)))
        .tool(ValueTool::new("dup", json!(2)));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::DuplicateTool(n)) if n == "dup"
    ));
    let fs_rw: Capability = "fs.rw:/work".parse().unwrap();
    let setup = Setup::new(FakeProvider::responses(vec![]))
        .tool(ValueTool::with_caps("writer", vec![fs_rw.clone()]));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::ToolExceedsGrants { tool, source: PolicyError::Exceeds { .. } }) if tool == "writer"
    ));
    let setup = Setup::new(FakeProvider::responses(vec![])).tool(Arc::new(BadSessionTool));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::MissingSessionCommand(n)) if n == "bad_repl"
    ));
}

#[tokio::test]
async fn granted_tools_get_their_own_policy_and_the_envelope_hash_is_recorded() {
    let fs_rw: Capability = "fs.rw:/work".parse().unwrap();
    let fs_ro: Capability = "fs.ro:/work/data".parse().unwrap();
    let secret: Capability = "secret:api_key".parse().unwrap();
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "reader", json!({}))]),
        text_response("done"),
    ]);
    let mut setup = Setup::new(provider).tool(ValueTool::with_caps("reader", vec![fs_ro.clone()]));
    setup.grants = vec![fs_rw.clone(), secret.clone()];
    let mut k = setup.create().await;
    let envelope =
        derive_policy_with(std::slice::from_ref(&fs_rw), &setup.grants, &setup.limits).unwrap();
    assert_eq!(k.state().sandbox_policy_hash, envelope.hash().unwrap());
    let events = setup.events();
    let created: Vec<&SessionCreatedPayload> = find_all(&events, |b| match b {
        EventBody::SessionCreated(p) => Some(p),
        _ => None,
    });
    assert_eq!(created[0].sandbox_policy_hash, envelope.hash().unwrap());
    assert_eq!(created[0].tools, ["reader"]);
    assert_eq!(created[0].grants, vec![fs_rw.clone(), secret]);
    assert_eq!(created[0].sandbox_backend, "fake");
    assert_eq!(created[0].provider, "fake");
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let call = tool_calls(&setup.events())[0].clone();
    let per_tool =
        derive_policy_with(std::slice::from_ref(&fs_ro), &setup.grants, &setup.limits).unwrap();
    assert_eq!(call.policy_hash, Some(per_tool.hash().unwrap()));
    assert_eq!(call.capabilities, vec![fs_ro]);
    assert_eq!(
        per_tool.mounts,
        vec![Mount {
            path: PathBuf::from("/work/data"),
            mode: FsMode::Ro
        }]
    );
}

#[tokio::test]
async fn sandbox_backend_none_is_warned_at_create_and_open() {
    let mut setup = Setup::new(FakeProvider::responses(vec![text_response("x")]));
    setup.sandbox = FakeSandbox::named("none");
    let mut k = setup.create().await;
    assert_eq!(
        warning_classes(&setup.events()),
        ["sandbox_backend_none", "artifact_store_noop", "memory_noop"]
    );
    assert_eq!(k.state().sandbox_backend, "none");
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let log = Arc::new(MemoryEventLog::from_events(
        setup.session_id.clone(),
        setup.redactor.clone(),
        setup.events(),
    ));
    let mut setup2 = Setup::new(FakeProvider::responses(vec![])).with_log(log.clone());
    setup2.sandbox = FakeSandbox::named("none");
    setup2.artifact_store = Arc::new(FailingArtifactStore);
    let _k = Kernel::open(setup2.config(), ResumeCause::Operator)
        .await
        .unwrap();
    let classes = warning_classes(&log.events());
    assert_eq!(
        classes
            .iter()
            .filter(|c| *c == "sandbox_backend_none")
            .count(),
        2
    );
    assert_eq!(
        classes
            .iter()
            .filter(|c| *c == "artifact_store_noop")
            .count(),
        1
    );
}

#[tokio::test]
async fn log_for_another_session_is_refused() {
    let setup = Setup::new(FakeProvider::responses(vec![]));
    let mut init = setup.init();
    init.session_id = SessionId("s_other".into());
    assert!(matches!(
        Kernel::create(setup.config(), init).await,
        Err(KernelError::Log(LogError::WrongSession { .. }))
    ));
}

//! `ask_user` (D17, §7.10): the tool builds the request, the host routes it through the prompter.

mod common;

use std::sync::Arc;

use common::{FakeSandbox, host, sandbox_policy};
use host::{AskUserTool, ChannelPrompter, NoUserPrompter, UserPrompter};
use kernel::{
    AskUserRequest, CancellationToken, Host, HostError, NoTaskRegistrar, NoopArtifactStore,
    SessionId, Tool, ToolContext, ToolError, ToolKind, ToolResult,
};
use serde_json::{Value, json};

async fn invoke(
    h: &dyn Host,
    turn: u64,
    tool_use_id: &str,
    input: Value,
) -> Result<Value, ToolError> {
    let session = SessionId("s1".to_owned());
    let policy = sandbox_policy();
    let ctx = ToolContext::new(
        h,
        CancellationToken::new(),
        &session,
        turn,
        tool_use_id,
        &policy,
        &FakeSandbox,
        &NoopArtifactStore,
        None,
        &NoTaskRegistrar,
    );
    match AskUserTool::new().invoke(&ctx, input).await? {
        ToolResult::Value(v) => Ok(v),
        other => panic!("expected a value, got {other:?}"),
    }
}

#[test]
fn tool_shape() {
    let t = AskUserTool::new();
    assert_eq!(t.name(), "ask_user");
    assert_eq!(t.kind(), ToolKind::Stateless);
    assert!(t.capabilities().is_empty());
    assert!(t.session_command().is_none());
    let schema = t.schema();
    assert_eq!(schema["required"], json!(["question"]));
    assert_eq!(
        schema["properties"]["allow_free_text"]["default"],
        json!(true)
    );
    assert_eq!(t.definition().name, "ask_user");
}

#[tokio::test]
async fn channel_prompter_round_trip_through_the_tool() {
    let (prompter, mut rx) = ChannelPrompter::new(4);
    let h = host().with_prompter(Arc::new(prompter));
    let client = tokio::spawn(async move {
        let q = rx.recv().await.expect("question");
        let req = q.request().clone();
        assert!(q.answer("blue"));
        req
    });
    let out = invoke(
        &h,
        7,
        "toolu_42",
        json!({"question": "Which color?", "options": ["red", "blue"]}),
    )
    .await
    .unwrap();
    let req = client.await.unwrap();
    assert_eq!(
        req,
        AskUserRequest {
            question_id: "q7-toolu_42".to_owned(),
            question: "Which color?".to_owned(),
            options: vec!["red".to_owned(), "blue".to_owned()],
            allow_free_text: true,
        }
    );
    assert_eq!(
        out,
        json!({"question_id": "q7-toolu_42", "answer": "blue", "declined": false})
    );
}

#[tokio::test]
async fn declined_answer() {
    let (prompter, mut rx) = ChannelPrompter::new(1);
    let h = host().with_prompter(Arc::new(prompter));
    tokio::spawn(async move {
        let q = rx.recv().await.expect("question");
        assert!(!q.request().allow_free_text);
        assert!(q.decline());
    });
    let out = invoke(
        &h,
        1,
        "t1",
        json!({"question": "Proceed?", "options": ["yes", "no"], "allow_free_text": false}),
    )
    .await
    .unwrap();
    assert_eq!(
        out,
        json!({"question_id": "q1-t1", "answer": null, "declined": true})
    );
}

#[tokio::test]
async fn dropped_question_and_disconnected_client_are_no_user() {
    let (prompter, mut rx) = ChannelPrompter::new(1);
    let h = host().with_prompter(Arc::new(prompter));
    tokio::spawn(async move {
        let q = rx.recv().await.expect("question");
        drop(q);
        drop(rx);
    });
    let r = invoke(&h, 1, "t1", json!({"question": "?"})).await;
    assert!(
        matches!(r, Err(ToolError::Failed(ref m)) if m.contains("without answering")),
        "{r:?}"
    );
    // The receiver is gone now: sending fails with NoUser too.
    let r = h
        .ask_user(AskUserRequest {
            question_id: "q1-t2".to_owned(),
            question: "?".to_owned(),
            options: vec![],
            allow_free_text: true,
        })
        .await;
    assert!(matches!(r, Err(HostError::NoUser(_))), "{r:?}");
}

#[tokio::test]
async fn no_user_prompter_is_no_user() {
    let r = NoUserPrompter
        .ask(AskUserRequest {
            question_id: "q1-t1".to_owned(),
            question: "?".to_owned(),
            options: vec![],
            allow_free_text: true,
        })
        .await;
    assert!(matches!(r, Err(HostError::NoUser(_))), "{r:?}");
    // The default host has no user; through the tool that is a `Failed` result.
    let r = invoke(&host(), 1, "t1", json!({"question": "?"})).await;
    assert!(
        matches!(r, Err(ToolError::Failed(ref m)) if m.contains("user interaction unavailable")),
        "{r:?}"
    );
}

#[tokio::test]
async fn invalid_input_is_rejected_before_asking() {
    let (prompter, rx) = ChannelPrompter::new(1);
    let h = host().with_prompter(Arc::new(prompter));
    for input in [
        json!({}),
        json!({"question": 3}),
        json!({"question": "  "}),
        json!({"question": "?", "options": "a"}),
        json!({"question": "?", "options": [1]}),
        json!({"question": "?", "allow_free_text": "no"}),
        json!({"question": "?", "allow_free_text": false}),
        json!("not an object"),
    ] {
        let r = invoke(&h, 1, "t1", input.clone()).await;
        assert!(
            matches!(r, Err(ToolError::InvalidInput(_))),
            "{input}: {r:?}"
        );
    }
    drop(rx);
}

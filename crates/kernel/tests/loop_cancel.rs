//! P1.2: cancellation (§7.1) — mid-tool, tool scope, before the model call, during the model call.

mod support;

use std::sync::Arc;

use kernel::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn cancel_turn_mid_tool_gives_synthetic_results_and_goes_idle() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "echo", json!({})),
            ("c2", "canceller", json!({})),
            ("c3", "echo", json!({})),
        ]),
        text_response("after"),
    ]);
    let echo = ValueTool::new("echo", json!("ok"));
    let canceller = HandleTool::new("canceller", false, |h| h.cancel(CancelScope::Turn));
    let setup = Setup::new(provider.clone())
        .tool(echo.clone())
        .tool(canceller.clone());
    let mut k = setup.create().await;
    canceller.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.status(), SessionStatus::Idle);
    assert_eq!(echo.call_count(), 1, "c3 was never started");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

    let events = setup.events();
    let c = cancelled_events(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].scope, CancelScopeKind::Turn);
    assert_eq!(c[0].phase, LoopPhase::Tool);
    assert_eq!(c[0].tool_use_id.as_deref(), Some("c2"));
    assert_eq!(c[0].skipped_tool_use_ids, ["c3"]);
    let res = tool_results(&events);
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].origin, ToolOutputOrigin::Invoke);
    assert_eq!(res[1].origin, ToolOutputOrigin::Cancelled);
    assert_eq!(res[2].origin, ToolOutputOrigin::Cancelled);
    assert_eq!(
        res[1].content,
        ToolResultContent::Json(json!({"cancelled": true}))
    );
    assert!(res[1].is_error && res[2].is_error);
    let ck = checkpoints(&events);
    let last = ck.last().unwrap();
    assert_eq!(last.reason, CheckpointReason::Cancel);
    assert_eq!(last.session_status, SessionStatus::Idle);
    // The assistant message stays; invariant 2 holds.
    assert_eq!(k.state().messages[1].role, Role::Assistant);
    assert_invariant_2(k.state());
    assert!(kernel::log::reader::ends_cleanly(&events));
    // A later cancel(Turn) while Idle is a no-op.
    k.handle().cancel(CancelScope::Turn);
    k.handle()
        .enqueue_user_message(Message::user_text("again"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(cancelled_events(&setup.events()).len(), 1);
    assert_invariant_1(&setup.events());
}

#[tokio::test]
async fn cancel_turn_drops_a_non_cooperative_tool_future() {
    let provider =
        FakeProvider::responses(vec![tool_use_response(vec![("c1", "sleep", json!({}))])]);
    let setup = Setup::new(provider).tool(Arc::new(SleepTool));
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.cancel(CancelScope::Turn);
    });
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), k.run_turn())
        .await
        .expect("the sleeping tool was dropped")
        .unwrap();
    canceller.await.unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);
    let events = setup.events();
    let c = cancelled_events(&events);
    assert_eq!(c[0].phase, LoopPhase::Tool);
    assert_eq!(c[0].tool_use_id.as_deref(), Some("c1"));
}

#[tokio::test]
async fn cancel_tool_scope_continues_the_turn() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "canceller", json!({})),
            ("c2", "echo", json!({})),
        ]),
        text_response("after"),
    ]);
    let echo = ValueTool::new("echo", json!("ok"));
    let canceller = HandleTool::new("canceller", false, |h| {
        h.cancel(CancelScope::Tool {
            tool_use_id: "c1".into(),
        })
    });
    let setup = Setup::new(provider.clone())
        .tool(echo.clone())
        .tool(canceller.clone());
    let mut k = setup.create().await;
    canceller.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(echo.call_count(), 1, "the turn continued with c2");
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    let events = setup.events();
    let c = cancelled_events(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].scope, CancelScopeKind::Tool);
    assert_eq!(c[0].tool_use_id.as_deref(), Some("c1"));
    assert!(c[0].skipped_tool_use_ids.is_empty());
    let res = tool_results(&events);
    assert_eq!(res[0].origin, ToolOutputOrigin::Cancelled);
    assert_eq!(res[1].origin, ToolOutputOrigin::Invoke);
    let ck = checkpoints(&events);
    assert!(ck.iter().all(|c| c.reason != CheckpointReason::Cancel));
    assert_invariant_2(k.state());
}

#[tokio::test]
async fn cancel_before_model_appends_nothing() {
    let provider = FakeProvider::responses(vec![text_response("never")]);
    let setup = Setup::new(provider.clone());
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    h.cancel(CancelScope::Turn);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Cancelled);
    assert_eq!(k.status(), SessionStatus::Idle);
    assert_eq!(provider.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(k.state().messages.len(), 1);
    assert_eq!(k.state().turn, 1);
    let events = setup.events();
    let c = cancelled_events(&events);
    assert_eq!(c[0].phase, LoopPhase::BeforeModel);
    assert!(c[0].tool_use_id.is_none());
    assert!(model_requests(&events).is_empty());
    assert_eq!(
        checkpoints(&events).last().unwrap().reason,
        CheckpointReason::Cancel
    );
}

#[tokio::test]
async fn cancel_during_model_call_aborts_the_in_flight_request() {
    let setup = Setup::new(Arc::new(HangingProvider));
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        h.cancel(CancelScope::Turn);
    });
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), k.run_turn())
        .await
        .expect("model call was dropped")
        .unwrap();
    canceller.await.unwrap();
    assert_eq!(outcome, TurnOutcome::Cancelled);
    assert_eq!(k.state().messages.len(), 1, "no partial assistant message");
    let events = setup.events();
    assert_eq!(cancelled_events(&events)[0].phase, LoopPhase::ModelCall);
    assert!(model_responses(&events).is_empty());
    assert_eq!(model_requests(&events).len(), 1);
}

#[tokio::test]
async fn cancel_turn_drains_queued_input_and_leaves_tasks_open() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("x"),
    ]);
    let job = TaskTool::external("job");
    let setup = Setup::new(provider).tool(job);
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    h.enqueue_user_message(Message::user_text("queued"))
        .unwrap();
    h.cancel(CancelScope::Turn);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Cancelled);
    assert_eq!(k.status(), SessionStatus::Idle);
    assert_eq!(k.state().open_tasks().count(), 1, "open tasks untouched");
    assert_eq!(
        k.state().messages.last().unwrap(),
        &Message::user_text("queued")
    );
}

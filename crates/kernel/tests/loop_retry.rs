//! P1.2: provider retry and turn failure (§7.2), streaming via the delta sink.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kernel::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn retry_then_success_logs_provider_retry_with_same_request_hash() {
    let provider = FakeProvider::new(vec![
        Err(ProviderError::RateLimited {
            retry_after: Some(Duration::from_millis(3)),
        }),
        Err(ProviderError::Server {
            status: 503,
            message: "busy".into(),
        }),
        Ok(text_response("finally")),
    ]);
    let mut setup = Setup::new(provider.clone());
    setup.retry = fast_retry(5);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);
    let hashes: Vec<Hash> = reqs.iter().map(|r| r.request_hash().unwrap()).collect();
    assert!(
        hashes.iter().all(|h| *h == hashes[0]),
        "every attempt has the same request_hash"
    );
    assert_eq!(
        reqs.iter().map(|r| r.trace.attempt).collect::<Vec<_>>(),
        [1, 2, 3]
    );
    assert_ne!(reqs[0].trace.request_id, reqs[1].trace.request_id);
    let events = setup.events();
    let retries: Vec<&ProviderRetryPayload> = find_all(&events, |b| match b {
        EventBody::ProviderRetry(p) => Some(p),
        _ => None,
    });
    assert_eq!(retries.len(), 2);
    assert_eq!(retries[0].attempt, 1);
    assert_eq!(retries[0].error_class, "provider_rate_limited");
    assert_eq!(
        retries[0].delay_ms, 3,
        "retry_after is a lower bound over the 1 ms backoff"
    );
    assert_eq!(retries[1].attempt, 2);
    assert_eq!(retries[1].error_class, "provider_server");
    assert_eq!(retries[1].delay_ms, 2, "base 1 ms × 2^(2−1)");
    assert_eq!(model_responses(&events)[0].attempts, 3);
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &[
            "model_request",
            "provider_retry",
            "provider_retry",
            "model_response",
        ],
    );
}

#[tokio::test]
async fn retry_exhaustion_fails_the_turn_with_checkpoint_intact_and_resume_retries() {
    let provider = FakeProvider::new(vec![
        Err(ProviderError::Transport("a".into())),
        Err(ProviderError::Timeout(Duration::from_secs(1))),
        Err(ProviderError::Server {
            status: 500,
            message: "c".into(),
        }),
        Ok(text_response("after resume")),
    ]);
    let mut setup = Setup::new(provider.clone());
    setup.retry = fast_retry(3);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "provider_server".into()
        }
    );
    assert_eq!(k.status(), SessionStatus::Failed);
    assert_eq!(
        k.state().messages.len(),
        1,
        "nothing appended for the failed turn"
    );
    let events = setup.events();
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &[
            "model_request",
            "provider_retry",
            "provider_retry",
            "turn_failed",
            "checkpoint",
            "session_failed",
        ],
    );
    assert!(kernel::log::reader::ends_cleanly(&events));
    let tf: Vec<&TurnFailedPayload> = find_all(&events, |b| match b {
        EventBody::TurnFailed(p) => Some(p),
        _ => None,
    });
    assert_eq!(tf[0].error_class, "provider_server");
    assert_eq!(tf[0].attempts, 3);
    assert_eq!(tf[0].turn, 1);
    let sf: Vec<&SessionFailedPayload> = find_all(&events, |b| match b {
        EventBody::SessionFailed(p) => Some(p),
        _ => None,
    });
    let cks = checkpoints(&events);
    let failure_ck = cks.last().unwrap();
    assert_eq!(failure_ck.reason, CheckpointReason::Failure);
    assert_eq!(failure_ck.session_status, SessionStatus::Failed);
    assert_eq!(
        failure_ck.state.messages, cks[0].state.messages,
        "last good state"
    );
    assert_eq!(sf[0].checkpoint_hash, failure_ck.state_hash);
    assert!(sf[0].resumable);
    let tf_seq = events
        .iter()
        .find(|e| e.body.kind() == "turn_failed")
        .unwrap()
        .seq;
    assert_eq!(sf[0].cause_seq, tf_seq);

    // Operator resume retries the turn from the checkpoint.
    k.resume(ResumeCause::Operator).await.unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().messages.len(), 2);
    assert_eq!(k.state().turn, 2);
    let events = setup.events();
    assert_invariant_1(&events);
    let res: Vec<&ResumedPayload> = find_all(&events, |b| match b {
        EventBody::Resumed(p) => Some(p),
        _ => None,
    });
    assert_eq!(res[0].from_status, SessionStatus::Failed);
    assert_eq!(res[0].cause, ResumeCauseKind::Operator);
}

#[tokio::test]
async fn non_retryable_error_fails_at_once_and_queued_messages_survive() {
    let provider = FakeProvider::new(vec![
        Ok(tool_use_response(vec![("c1", "poke", json!({}))])),
        Err(ProviderError::ContextTooLong("too long".into())),
        Ok(text_response("ok")),
    ]);
    let poke = HandleTool::new("poke", true, |h| {
        // Delivered while Running: queued; the failing turn drains it.
        h.enqueue_user_message(Message::user_text("queued during failure"))
            .unwrap();
    });
    let setup = Setup::new(provider.clone()).tool(poke.clone());
    let mut k = setup.create().await;
    poke.attach(k.handle());
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    // The poke tool ran in turn 1 and queued a message, which turn 1's boundary drained.
    // Queue another one for the failing turn.
    h.enqueue_user_message(Message::user_text("second"))
        .unwrap();
    assert_eq!(
        k.run_turn().await.unwrap(),
        TurnOutcome::Failed {
            error_class: "provider_context_too_long".into()
        }
    );
    assert_eq!(
        provider.calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "no retry"
    );
    let events = setup.events();
    let tf: Vec<&TurnFailedPayload> = find_all(&events, |b| match b {
        EventBody::TurnFailed(p) => Some(p),
        _ => None,
    });
    assert_eq!(tf[0].attempts, 1);
    assert!(
        find_all(&events, |b| match b {
            EventBody::ProviderRetry(p) => Some(p),
            _ => None,
        })
        .is_empty()
    );
    assert_eq!(
        k.state().messages.last().unwrap(),
        &Message::user_text("second")
    );
    let cks = checkpoints(&events);
    assert!(
        cks.last()
            .unwrap()
            .state
            .messages
            .contains(&Message::user_text("second"))
    );
    // A user message resumes a Failed session through `run`.
    h.enqueue_user_message(Message::user_text("third")).unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ums: Vec<&UserMessagePayload> = find_all(&events, |b| match b {
        EventBody::UserMessage(u) => Some(u),
        _ => None,
    });
    assert_eq!(ums.last().unwrap().applied.at, AppliedPoint::Failed);
}

#[tokio::test]
async fn per_attempt_request_timeout_is_retryable() {
    let provider: Arc<dyn Provider> = Arc::new(HangingProvider);
    let mut setup = Setup::new(provider);
    setup.retry = RetryPolicy {
        max_attempts: 2,
        request_timeout: Duration::from_millis(50),
        ..fast_retry(2)
    };
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "provider_timeout".into()
        }
    );
    let events = setup.events();
    let retries: Vec<&ProviderRetryPayload> = find_all(&events, |b| match b {
        EventBody::ProviderRetry(p) => Some(p),
        _ => None,
    });
    assert_eq!(retries.len(), 1);
    assert_eq!(retries[0].error_class, "provider_timeout");
}

#[tokio::test]
async fn delta_sink_uses_complete_stream_and_forwards_deltas() {
    let provider = FakeProvider::streaming(vec![
        Err(ProviderError::Transport("flaky".into())),
        Ok(text_response("streamed")),
    ]);
    let (tx, mut rx) = tokio::sync::broadcast::channel(16);
    let mut setup = Setup::new(provider.clone());
    setup.delta_sink = Some(tx);
    let mut k = setup.create().await;
    assert!(k.handle().subscribe_deltas().is_some());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let mut got = Vec::new();
    while let Ok(d) = rx.try_recv() {
        got.push(d);
    }
    assert_eq!(got.len(), 2);
    assert_eq!(
        got[0],
        ModelDelta::TextDelta {
            index: 0,
            text: "streamed".into()
        }
    );
    assert!(matches!(got[1], ModelDelta::Complete(_)));
    let events = setup.events();
    assert_eq!(model_responses(&events)[0].attempts, 2);
    assert_eq!(
        model_responses(&events)[0].content,
        text_response("streamed").content
    );
}

#[tokio::test]
async fn middleware_error_fails_the_turn_with_class_middleware() {
    let provider = FakeProvider::responses(vec![text_response("x")]);
    let mut mw = FnMiddleware::empty();
    mw.after_model = Some(Box::new(|_, _, _| {
        Err(MiddlewareError::new("boom", "after_model", "nope"))
    }));
    let setup = Setup::new(provider).middleware(FnMiddleware::entry("boom", 300, mw));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "middleware".into()
        }
    );
    let events = setup.events();
    let tf: Vec<&TurnFailedPayload> = find_all(&events, |b| match b {
        EventBody::TurnFailed(p) => Some(p),
        _ => None,
    });
    assert_eq!(tf[0].middleware.as_deref(), Some("boom"));
    assert_eq!(tf[0].hook.as_deref(), Some("after_model"));
    assert!(
        model_responses(&events).is_empty(),
        "no model_response for the discarded turn"
    );
    assert_eq!(k.state().messages.len(), 1);
}

#[tokio::test]
async fn replay_miss_from_a_tool_fails_the_turn() {
    let provider = FakeProvider::responses(vec![tool_use_response(vec![("c1", "t", json!({}))])]);
    let tool = ValueTool::with_result(
        "t",
        Err(ToolError::ReplayMiss {
            tool: "t".into(),
            request_hash: h("aa"),
        }),
    );
    let setup = Setup::new(provider).tool(tool);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "replay_miss".into()
        }
    );
    assert_eq!(k.state().messages.len(), 1);
}

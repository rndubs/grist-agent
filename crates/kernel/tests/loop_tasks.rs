//! P1.2: the task state machine (§4) — suspension rule, injection while Running / Suspended /
//! Idle / Failed, ignored updates, `cancel(Task)`, the in-process waker, invariant 3.

mod support;

use kernel::loop_::KernelError;
use kernel::*;
use serde_json::json;
use support::*;

fn outcome(v: serde_json::Value, is_error: bool) -> TaskOutcome {
    TaskOutcome {
        content: ToolResultContent::Json(v),
        is_error,
        artifact_handles: vec![],
    }
}

fn update(id: &str, status: TaskStatus, outcome: Option<TaskOutcome>) -> TaskUpdate {
    TaskUpdate {
        id: TaskId(id.into()),
        status,
        outcome,
        eta: None,
        check_hint: None,
        source: WakerSource {
            kind: "test".into(),
            trust_tier: None,
            detail: json!(null),
        },
    }
}

#[tokio::test]
async fn task_started_then_text_turn_suspends_with_in_process_waker() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("I'll wait."),
    ]);
    let job = TaskTool::new("job");
    let setup = Setup::new(provider.clone()).tool(job.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    let stop = k.run().await.unwrap();
    let RunStop::Suspended(s) = stop else {
        panic!("expected Suspended, got {stop:?}")
    };
    assert_eq!(s.pending_task_ids, vec![TaskId("t1-c1".into())]);
    assert_eq!(s.in_process_wakers, 1);
    assert_eq!(s.checkpoint_hash, *k.checkpoint_hash());
    assert_eq!(k.status(), SessionStatus::Suspended);
    assert_eq!(
        k.state().turn,
        2,
        "turn 1 had a tool call → Continue; turn 2 suspended"
    );

    let events = setup.events();
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &[
            "tool_call",
            "tool_result",
            "task_started",
            "checkpoint",
            "model_request",
            "model_response",
            "checkpoint",
            "suspended",
        ],
    );
    assert!(kernel::log::reader::ends_cleanly(&events));
    // task_started payload.
    let started: Vec<&TaskStartedPayload> = find_all(&events, |b| match b {
        EventBody::TaskStarted(p) => Some(p),
        _ => None,
    });
    assert_eq!(started[0].task_id, TaskId("t1-c1".into()));
    assert_eq!(started[0].status, TaskStatus::Running);
    assert_eq!(started[0].eta_secs, Some(60));
    assert!(started[0].in_process_waker);
    assert_eq!(
        started[0].check_hint,
        Some(json!({"pid": 4242, "secret_hint": true}))
    );
    // The model-visible result has no check_hint (invariant 3).
    let res = tool_results(&events);
    assert_eq!(
        res[0].content,
        ToolResultContent::Json(json!({
            "task_id": "t1-c1", "status": "running", "eta_secs": 60, "description": "fake job"
        }))
    );
    assert_eq!(
        res[0].task.as_ref().unwrap().task_id,
        TaskId("t1-c1".into())
    );
    assert_invariant_3(k.state(), &provider.requests());
    let task = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert!(task.in_process_waker);
    assert_eq!(
        task.check_hint,
        Some(json!({"pid": 4242, "secret_hint": true}))
    );
    let sus: Vec<&SuspendedPayload> = find_all(&events, |b| match b {
        EventBody::Suspended(p) => Some(p),
        _ => None,
    });
    assert_eq!(sus[0].reason, SuspendReason::PendingTasks);
    assert_eq!(sus[0].in_process_wakers, 1);
    assert_eq!(sus[0].checkpoint_hash, s.checkpoint_hash);
    let cks = checkpoints(&events);
    assert_eq!(cks.last().unwrap().session_status, SessionStatus::Suspended);
}

#[tokio::test]
async fn in_process_waker_resumes_a_suspended_session() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
        text_response("job done, thanks"),
    ]);
    let job = TaskTool::new("job");
    let setup = Setup::new(provider.clone()).tool(job.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    // The waker fires while suspended.
    job.complete(outcome(json!({"exit_code": 0}), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 3);
    let events = setup.events();
    let ups = task_updates(&events);
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].from_status, Some(TaskStatus::Running));
    assert_eq!(ups[0].status, TaskStatus::Succeeded);
    assert_eq!(ups[0].applied.at, AppliedPoint::Suspended);
    assert_eq!(ups[0].waker.kind, "in_process_exit");
    assert!(ups[0].ignored_reason.is_none());
    let ks = kinds(&events);
    let p = ks.iter().position(|k| *k == "suspended").unwrap();
    assert_eq!(
        &ks[p..p + 4],
        &["suspended", "task_update", "checkpoint", "model_request"]
    );
    let cks = checkpoints(&events);
    let ck = cks[cks.len() - 2];
    assert_eq!(ck.reason, CheckpointReason::TaskUpdate);
    assert_eq!(ck.session_status, SessionStatus::Running);
    // The synthetic TaskResult is a User message and the third request carries it.
    let reqs = provider.requests();
    let last = reqs[2].messages.last().unwrap();
    assert_eq!(last.role, Role::User);
    assert!(matches!(
        &last.content[0],
        ContentBlock::TaskResult { task_id, status: TaskStatus::Succeeded, is_error: false, .. }
            if task_id.0 == "t1-c1"
    ));
    let task = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert_eq!(task.status, TaskStatus::Succeeded);
    assert_eq!(task.completed_turn, Some(2));
    assert_invariant_1(&events);
    assert_invariant_3(k.state(), &reqs);
}

#[tokio::test]
async fn completion_while_running_is_injected_at_end_of_turn_and_never_suspends() {
    // Turn 1 starts the job; turn 2 calls `finish` which completes it mid-turn; the update is
    // queued and applied at the end of turn 2 → the turn ends `Continue`, not `Suspended`.
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        tool_use_response(vec![("c2", "finish", json!({}))]),
        text_response("all done"),
    ]);
    let job = TaskTool::new("job");
    let job2 = job.clone();
    let finish = HandleTool::new("finish", true, move |_| {
        job2.complete(outcome(json!({"ok": 1}), false));
    });
    let setup = Setup::new(provider.clone())
        .tool(job.clone())
        .tool(finish.clone());
    let mut k = setup.create().await;
    finish.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 3);
    let events = setup.events();
    let ups = task_updates(&events);
    assert_eq!(
        ups[0].applied,
        AppliedAt {
            turn: 2,
            at: AppliedPoint::EndOfTurn
        }
    );
    let ks = kinds(&events);
    assert!(!ks.contains(&"suspended"));
    // Order at the end of turn 2: tool_result(finish), task_update, checkpoint.
    let p = ks.iter().rposition(|k| *k == "task_update").unwrap();
    assert_eq!(ks[p - 1], "tool_result");
    assert_eq!(ks[p + 1], "checkpoint");
    // Turn 3's request sees the TaskResult right after the finish tool result.
    let msgs = &provider.requests()[2].messages;
    assert!(matches!(
        msgs.last().unwrap().content[0],
        ContentBlock::TaskResult { .. }
    ));
    assert_invariant_2(k.state());
}

#[tokio::test]
async fn task_update_in_idle_is_applied_and_stays_idle() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
    ]);
    let job = TaskTool::external("job");
    let setup = Setup::new(provider.clone()).tool(job.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    // External waker → suspension has zero in-process wakers.
    let RunStop::Suspended(s) = k.run().await.unwrap() else {
        panic!()
    };
    assert_eq!(s.in_process_wakers, 0);
    // Cancel the turn? No: reach Idle via a user message + text turn, task still open.
    // (A user message in Suspended → Running; the text turn then suspends again because the
    // task is open; so instead use the spec's Idle route: cancel(Turn) leaves tasks open.)
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("status?"))
        .unwrap();
    // Apply the message (Suspended → Running) and cancel before the model call.
    assert!(k.apply_queued_input().await.unwrap());
    h.cancel(CancelScope::Turn);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Cancelled);
    assert_eq!(k.status(), SessionStatus::Idle);
    // Now a progress update, then a terminal update, both while Idle.
    h.deliver_task_update(update("t1-c1", TaskStatus::Running, None))
        .unwrap();
    h.deliver_task_update(update(
        "t1-c1",
        TaskStatus::Succeeded,
        Some(outcome(json!(1), false)),
    ))
    .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.status(), SessionStatus::Idle);
    let events = setup.events();
    let ups = task_updates(&events);
    assert_eq!(ups.len(), 2);
    assert_eq!(ups[0].status, TaskStatus::Running);
    assert_eq!(ups[0].from_status, Some(TaskStatus::Pending));
    assert_eq!(ups[0].applied.at, AppliedPoint::Idle);
    assert_eq!(ups[1].status, TaskStatus::Succeeded);
    assert_eq!(ups[1].applied.at, AppliedPoint::Idle);
    let cks = checkpoints(&events);
    let last = cks.last().unwrap();
    assert_eq!(last.reason, CheckpointReason::TaskUpdate);
    assert_eq!(last.session_status, SessionStatus::Idle);
    assert_eq!(k.state().turn, 3);
    assert!(
        model_requests(&events).len() == 2,
        "no turn ran for the Idle update"
    );
    // A later user message starts a turn that sees the TaskResult.
    h.enqueue_user_message(Message::user_text("and?")).unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let reqs = provider.requests();
    let msgs = &reqs.last().unwrap().messages;
    assert!(
        msgs.iter()
            .any(|m| matches!(m.content[0], ContentBlock::TaskResult { .. }))
    );
}

#[tokio::test]
async fn task_update_in_failed_is_applied_and_stays_failed() {
    let provider = FakeProvider::new(vec![
        Ok(tool_use_response(vec![("c1", "job", json!({}))])),
        Err(ProviderError::Auth("bad key".into())),
    ]);
    let job = TaskTool::external("job");
    let setup = Setup::new(provider).tool(job);
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Failed { .. }));
    h.deliver_task_update(update(
        "t1-c1",
        TaskStatus::Failed,
        Some(outcome(json!("boom"), true)),
    ))
    .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "provider_auth".into()
        }
    );
    assert_eq!(k.status(), SessionStatus::Failed);
    let events = setup.events();
    let ups = task_updates(&events);
    assert_eq!(ups[0].applied.at, AppliedPoint::Failed);
    assert_eq!(
        checkpoints(&events).last().unwrap().session_status,
        SessionStatus::Failed
    );
    assert!(matches!(
        k.state().messages.last().unwrap().content[0],
        ContentBlock::TaskResult {
            status: TaskStatus::Failed,
            is_error: true,
            ..
        }
    ));
}

#[tokio::test]
async fn ignored_updates_terminal_unknown_and_missing_outcome() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
    ]);
    let job = TaskTool::external("job");
    let setup = Setup::new(provider).tool(job);
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    // Missing outcome: rejected, task stays open, session stays Suspended.
    h.deliver_task_update(update("t1-c1", TaskStatus::Succeeded, None))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    // Unknown id.
    h.deliver_task_update(update(
        "t9-zz",
        TaskStatus::Succeeded,
        Some(outcome(json!(1), false)),
    ))
    .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    // Real completion, then a second update on the terminal task.
    h.deliver_task_update(update(
        "t1-c1",
        TaskStatus::Succeeded,
        Some(outcome(json!(1), false)),
    ))
    .unwrap();
    h.deliver_task_update(update(
        "t1-c1",
        TaskStatus::Failed,
        Some(outcome(json!(2), true)),
    ))
    .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ups = task_updates(&events);
    assert_eq!(ups.len(), 4);
    assert_eq!(ups[0].applied.at, AppliedPoint::Ignored);
    assert_eq!(ups[0].ignored_reason.as_deref(), Some("missing_outcome"));
    assert_eq!(ups[0].from_status, Some(TaskStatus::Pending));
    assert_eq!(ups[1].applied.at, AppliedPoint::Ignored);
    assert_eq!(ups[1].ignored_reason.as_deref(), Some("unknown_task"));
    assert_eq!(ups[1].from_status, None);
    assert!(ups[2].ignored_reason.is_none());
    assert_eq!(ups[3].ignored_reason.as_deref(), Some("terminal"));
    let w = warning_classes(&events);
    assert!(w.contains(&"task_update_invalid".to_owned()));
    assert!(w.contains(&"task_update_unknown".to_owned()));
    assert!(w.contains(&"task_update_ignored".to_owned()));
    assert_eq!(
        k.state().pending_tasks[&TaskId("t1-c1".into())].status,
        TaskStatus::Succeeded
    );
}

#[tokio::test]
async fn cancel_task_drops_the_waker_and_applies_the_terminal_transition() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
        text_response("ok, cancelled"),
    ]);
    let job = TaskTool::new("job");
    let setup = Setup::new(provider.clone()).tool(job.clone());
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    h.cancel(CancelScope::Task {
        task_id: TaskId("t1-c1".into()),
    });
    // The completer's receiver was dropped with the future: completing is harmless.
    job.complete(outcome(json!("late"), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let task = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert_eq!(task.status, TaskStatus::Cancelled);
    let events = setup.events();
    let c = cancelled_events(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].scope, CancelScopeKind::Task);
    assert_eq!(c[0].task_id, Some(TaskId("t1-c1".into())));
    assert!(
        task_updates(&events).is_empty(),
        "the late completion never arrived"
    );
    assert!(matches!(
        provider.requests()[2].messages.last().unwrap().content[0],
        ContentBlock::TaskResult {
            status: TaskStatus::Cancelled,
            is_error: true,
            ..
        }
    ));
    // Cancelling again finds nothing: no second event.
    h.cancel(CancelScope::Task {
        task_id: TaskId("t1-c1".into()),
    });
    h.enqueue_user_message(Message::user_text("more")).unwrap();
    k.run().await.unwrap();
    assert_eq!(cancelled_events(&setup.events()).len(), 1);
}

#[tokio::test]
async fn explicit_suspend_and_in_process_resume() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("x"),
        text_response("y"),
    ]);
    let job = TaskTool::external("job");
    let setup = Setup::new(provider).tool(job);
    let mut k = setup.create().await;
    assert!(matches!(k.suspend().await, Err(KernelError::NotRunning(_))));
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    // Running between turns with an open task: explicit suspend.
    let s = k.suspend().await.unwrap();
    assert_eq!(s.pending_task_ids, vec![TaskId("t1-c1".into())]);
    assert_eq!(k.status(), SessionStatus::Suspended);
    let events = setup.events();
    let sus: Vec<&SuspendedPayload> = find_all(&events, |b| match b {
        EventBody::Suspended(p) => Some(p),
        _ => None,
    });
    assert_eq!(sus[0].reason, SuspendReason::Explicit);
    assert_eq!(
        checkpoints(&events).last().unwrap().reason,
        CheckpointReason::Suspend
    );
    // In-process resume with a task update.
    k.resume(ResumeCause::TaskUpdate(update(
        "t1-c1",
        TaskStatus::Succeeded,
        Some(outcome(json!(1), false)),
    )))
    .await
    .unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    let events = setup.events();
    let ks = kinds(&events);
    let p = ks.iter().position(|k| *k == "resumed").unwrap();
    assert_eq!(&ks[p..p + 3], &["resumed", "task_update", "checkpoint"]);
    let res: Vec<&ResumedPayload> = find_all(&events, |b| match b {
        EventBody::Resumed(p) => Some(p),
        _ => None,
    });
    assert!(!res[0].new_process);
    assert_eq!(res[0].from_status, SessionStatus::Suspended);
    assert_eq!(res[0].cause, ResumeCauseKind::TaskUpdate);
    assert_eq!(
        checkpoints(&events).last().unwrap().reason,
        CheckpointReason::Resume
    );
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    // Idle with no open task: suspend refuses.
    assert!(matches!(
        k.suspend().await,
        Err(KernelError::NothingToWaitFor)
    ));
}

#[tokio::test]
async fn task_update_in_created_is_rejected_and_task_handle_with_foreign_id_is_an_error() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("done"),
    ]);
    let mut job = TaskTool::new("job");
    std::sync::Arc::get_mut(&mut job).unwrap().bad_id = true;
    let setup = Setup::new(provider).tool(job.clone());
    let mut k = setup.create().await;
    assert!(matches!(
        k.handle()
            .deliver_task_update(update("t1-c1", TaskStatus::Running, None)),
        Err(KernelError::NotRunning(SessionStatus::Created))
    ));
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let res = tool_results(&events);
    assert!(res[0].is_error);
    assert!(res[0].task.is_none());
    assert!(k.state().pending_tasks.is_empty());
    // The waker future registered for the foreign id was dropped; the session went Idle.
    assert_eq!(k.status(), SessionStatus::Idle);
}

//! P1.2: plain turns, tool-call turns, ordering, registry enforcement, invariants 1–4, queued
//! input, `end` from every state, `ask_user`, session-kind processes, the event broadcast.

mod support;

use std::sync::Arc;

use kernel::loop_::KernelError;
use kernel::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn plain_turn_goes_idle_with_the_spec_event_order() {
    let provider = FakeProvider::responses(vec![text_response("hello")]);
    let setup = Setup::new(provider.clone());
    let mut k = setup.create().await;
    assert_eq!(k.status(), SessionStatus::Created);
    assert_eq!(
        setup.kinds(),
        [
            "log_opened",
            "session_created",
            "profile_load",
            "middleware_chain_resolved",
            "warning",
            "warning"
        ]
    );
    assert_eq!(
        warning_classes(&setup.events()),
        ["artifact_store_noop", "memory_noop"]
    );

    k.handle()
        .enqueue_user_message(Message::user_text("hi"))
        .unwrap();
    let stop = k.run().await.unwrap();
    assert_eq!(stop, RunStop::Idle);
    assert_eq!(k.status(), SessionStatus::Idle);
    assert_eq!(k.state().turn, 1);
    assert_eq!(k.state().messages.len(), 2);
    assert_eq!(k.state().messages[1].role, Role::Assistant);

    let events = setup.events();
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &[
            "user_message",
            "checkpoint",
            "model_request",
            "model_response",
            "checkpoint",
        ],
    );
    let cks = checkpoints(&events);
    assert_eq!(cks.len(), 2);
    assert_eq!(cks[0].reason, CheckpointReason::UserInput);
    assert_eq!(cks[0].session_status, SessionStatus::Running);
    assert_eq!(cks[1].reason, CheckpointReason::TurnEnd);
    assert_eq!(cks[1].session_status, SessionStatus::Idle);
    assert_eq!(cks[1].state_hash, k.state().state_hash().unwrap());
    assert_eq!(k.checkpoint_hash(), &cks[1].state_hash);
    assert_invariant_1(&events);

    // model_request payload fields.
    let req = model_requests(&events)[0];
    let sent = &provider.requests()[0];
    assert_eq!(req.request_hash, sent.request_hash().unwrap());
    assert_eq!(req.message_count, 1);
    assert_eq!(req.model_id, "fake-model");
    assert_eq!(req.tool_names, Vec::<String>::new());
    assert_eq!(
        req.system_prompt_hash,
        Hash::of_canonical_json(&sent.system).unwrap()
    );
    assert_eq!(
        req.params_hash,
        Hash::of_canonical_json(&sent.params).unwrap()
    );
    assert_eq!(req.prompt_blocks.len(), 1);
    assert_eq!(req.prompt_blocks[0].name, "test");
    assert_eq!(sent.trace.turn, 1);
    assert_eq!(sent.trace.attempt, 1);
    assert_eq!(sent.trace.checkpoint_hash, cks[0].state_hash);
    assert!(uuid::Uuid::parse_str(&sent.trace.request_id).is_ok());
    // model_response payload fields.
    let resp = model_responses(&events)[0];
    assert_eq!(resp.request_hash, req.request_hash);
    assert_eq!(resp.attempts, 1);
    assert_eq!(resp.stop_reason, StopReason::EndTurn);
    assert_eq!(resp.content, k.state().messages[1].content);
    assert_eq!(
        resp.response_hash,
        text_response("hello").response_hash().unwrap()
    );
    assert!(reader_ends_cleanly(&events));
}

fn reader_ends_cleanly(events: &[Event]) -> bool {
    kernel::log::reader::ends_cleanly(events)
}

#[tokio::test]
async fn tool_call_turn_then_idle() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"x": 1}))]),
        text_response("done"),
    ]);
    let tool = ValueTool::new("echo", json!({"echoed": true}));
    let setup = Setup::new(provider.clone()).tool(tool.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    // Turn 1: tool call → Continue.
    k.run_turn().await.unwrap_err(); // not running yet: message not applied until `run`
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 2);
    assert_eq!(tool.call_count(), 1);
    assert_eq!(lock(&tool.inputs)[0], json!({"x": 1}));

    let events = setup.events();
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &[
            "model_request",
            "model_response",
            "tool_call",
            "tool_result",
            "checkpoint",
            "model_request",
            "model_response",
            "checkpoint",
        ],
    );
    let cks = checkpoints(&events);
    assert_eq!(cks[1].session_status, SessionStatus::Running);
    assert_eq!(cks[2].session_status, SessionStatus::Idle);
    let call = tool_calls(&events)[0];
    assert!(call.registered);
    assert_eq!(call.kind, Some(ToolKind::Stateless));
    assert_eq!(
        call.args_hash,
        Hash::of_canonical_json(&json!({"x": 1})).unwrap()
    );
    assert_eq!(
        call.request_hash,
        ToolCall {
            tool_use_id: "c1".into(),
            name: "echo".into(),
            input: json!({"x": 1})
        }
        .request_hash()
        .unwrap()
    );
    assert_eq!(call.checkpoint_hash, cks[0].state_hash);
    assert!(call.policy_hash.is_some());
    let res = tool_results(&events)[0];
    assert!(!res.is_error);
    assert_eq!(res.origin, ToolOutputOrigin::Invoke);
    assert_eq!(
        res.content,
        ToolResultContent::Json(json!({"echoed": true}))
    );
    assert_eq!(
        res.result_hash,
        Hash::of_canonical_json(&json!({"content": {"json": {"echoed": true}}, "is_error": false}))
            .unwrap()
    );
    assert!(res.task.is_none() && !res.spilled && res.spill.is_none());
    // The second request sees the tool result.
    let reqs = provider.requests();
    assert_eq!(reqs[1].messages.len(), 3);
    assert_eq!(reqs[1].messages[2].role, Role::Tool);
    assert_invariant_1(&events);
    assert_invariant_2(k.state());
    // Exposed tool definitions.
    assert_eq!(reqs[0].tools.len(), 1);
    assert_eq!(reqs[0].tools[0].name, "echo");
    assert_eq!(model_requests(&events)[0].tool_names, ["echo"]);
}

#[tokio::test]
async fn multi_tool_turn_runs_in_order_and_answers_each_call() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "a", json!({})),
            ("c2", "b", json!({})),
            ("c3", "a", json!({"again": true})),
        ]),
        text_response("done"),
    ]);
    let order: HookLog = Default::default();
    let a = ValueTool::new("a", json!("A"));
    let b = ValueTool::new("b", json!("B"));
    let setup = Setup::new(provider)
        .tool(a.clone())
        .tool(b.clone())
        .middleware(RecordingMiddleware::entry("rec", 200, &order));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ids: Vec<&str> = tool_results(&events)
        .iter()
        .map(|r| r.tool_use_id.as_str())
        .collect();
    assert_eq!(ids, ["c1", "c2", "c3"]);
    let names: Vec<&str> = tool_calls(&events)
        .iter()
        .map(|r| r.name.as_str())
        .collect();
    assert_eq!(names, ["a", "b", "a"]);
    assert_eq!(a.call_count(), 2);
    assert_eq!(b.call_count(), 1);
    assert_invariant_2(k.state());
    // Every call: tool_call then tool_result, adjacent.
    let ks = kinds(&events);
    let first = ks.iter().position(|k| *k == "tool_call").unwrap();
    assert_eq!(
        &ks[first..first + 6],
        &[
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result",
            "tool_call",
            "tool_result"
        ]
    );
    let hooks = lock(&order).clone();
    assert_eq!(
        hooks,
        [
            "rec:before_model",
            "rec:after_model",
            "rec:before_tool",
            "rec:after_tool",
            "rec:before_tool",
            "rec:after_tool",
            "rec:before_tool",
            "rec:after_tool",
            "rec:before_model",
            "rec:after_model",
        ]
    );
}

#[tokio::test]
async fn unregistered_tool_yields_error_result_and_never_invokes() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "nope", json!({"a": 1})),
            ("c2", "echo", json!({})),
        ]),
        text_response("done"),
    ]);
    let tool = ValueTool::new("echo", json!(1));
    let setup = Setup::new(provider).tool(tool.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let calls = tool_calls(&events);
    assert!(!calls[0].registered);
    assert_eq!(calls[0].kind, None);
    assert!(calls[0].policy_hash.is_none());
    assert!(calls[0].capabilities.is_empty());
    let res = tool_results(&events);
    assert!(res[0].is_error);
    assert_eq!(res[0].origin, ToolOutputOrigin::Unregistered);
    assert_eq!(
        res[0].content,
        ToolResultContent::Json(json!({"error": "unknown tool"}))
    );
    assert!(warning_classes(&events).contains(&"unregistered_tool".to_owned()));
    // Invariant 4: the registered tool still ran; the session recovered.
    assert_eq!(tool.call_count(), 1);
    assert_invariant_2(k.state());
    assert_eq!(k.status(), SessionStatus::Idle);
}

#[tokio::test]
async fn user_message_queued_while_running_is_drained_at_end_of_turn() {
    // The tool enqueues a user message mid-turn; it must be applied at the end of the turn (not
    // at the start of the next), producing Continue and a `user_message{applied: end_of_turn}`.
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "poke", json!({}))]),
        text_response("second"),
        text_response("third"),
    ]);
    let poke = HandleTool::new("poke", true, |h| {
        h.enqueue_user_message(Message::user_text("queued"))
            .unwrap();
    });
    let setup = Setup::new(provider.clone()).tool(poke.clone());
    let mut k = setup.create().await;
    poke.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ums: Vec<&UserMessagePayload> = find_all(&events, |b| match b {
        EventBody::UserMessage(u) => Some(u),
        _ => None,
    });
    assert_eq!(ums.len(), 2);
    assert_eq!(ums[0].applied.at, AppliedPoint::Created);
    assert_eq!(ums[1].applied.at, AppliedPoint::EndOfTurn);
    assert_eq!(ums[1].applied.turn, 1);
    // Order within turn 1: tool_result, user_message, checkpoint.
    let ks = kinds(&events);
    let p = ks.iter().position(|k| *k == "tool_result").unwrap();
    assert_eq!(
        &ks[p..p + 3],
        &["tool_result", "user_message", "checkpoint"]
    );
    // The queued message is in the second request's messages (last one).
    let reqs = provider.requests();
    assert_eq!(
        reqs[1].messages.last().unwrap(),
        &Message::user_text("queued")
    );
    // Turn 2 has no tool calls but queued input was drained → not Idle until turn... turn 2
    // had no queued input, so it ends Idle after two model calls.
    assert_eq!(k.state().turn, 2);
}

#[tokio::test]
async fn queued_input_while_running_with_text_only_turn_continues() {
    // A text-only turn whose inbox holds a user message ends `Continue`, not `Idle`.
    let provider = FakeProvider::responses(vec![text_response("a"), text_response("b")]);
    let setup = Setup::new(provider.clone());
    let mut k = setup.create().await;
    let h = k.handle();
    h.enqueue_user_message(Message::user_text("one")).unwrap();
    // Apply "one" (Created → Running) without running a turn.
    assert!(k.apply_queued_input().await.unwrap());
    assert!(
        !k.apply_queued_input().await.unwrap(),
        "no-op while Running"
    );
    // Enqueue while Running, before the turn starts: drained at end of turn 1.
    h.enqueue_user_message(Message::user_text("two")).unwrap();
    assert_eq!(k.status(), SessionStatus::Running);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Idle);
    assert_eq!(provider.requests()[1].messages.len(), 3);
}

#[tokio::test]
async fn run_turn_requires_running() {
    let setup = Setup::new(FakeProvider::responses(vec![]));
    let mut k = setup.create().await;
    assert!(matches!(
        k.run_turn().await,
        Err(KernelError::NotRunning(SessionStatus::Created))
    ));
    assert!(matches!(
        k.suspend().await,
        Err(KernelError::NotRunning(SessionStatus::Created))
    ));
    assert!(matches!(
        k.resume(ResumeCause::Operator).await,
        Err(KernelError::NotRunning(SessionStatus::Created))
    ));
}

#[tokio::test]
async fn end_from_created_idle_and_failed_and_every_later_call_is_done() {
    // Created.
    let setup = Setup::new(FakeProvider::responses(vec![]));
    let mut k = setup.create().await;
    k.end().await.unwrap();
    assert_eq!(k.status(), SessionStatus::Done);
    let events = setup.events();
    let ended: Vec<&SessionEndedPayload> = find_all(&events, |b| match b {
        EventBody::SessionEnded(p) => Some(p),
        _ => None,
    });
    assert_eq!(ended[0].turn, 0);
    assert_eq!(ended[0].by, EndedBy::User);
    let cks = checkpoints(&events);
    assert_eq!(cks.last().unwrap().reason, CheckpointReason::End);
    assert_eq!(cks.last().unwrap().session_status, SessionStatus::Done);
    assert_eq!(ended[0].checkpoint_hash, cks.last().unwrap().state_hash);
    assert!(matches!(k.end().await, Err(KernelError::Done)));
    assert!(matches!(k.run().await, Err(KernelError::Done)));
    assert!(matches!(k.run_turn().await, Err(KernelError::Done)));
    assert!(matches!(k.compact("x").await, Err(KernelError::Done)));
    assert!(matches!(
        k.handle().enqueue_user_message(Message::user_text("x")),
        Err(KernelError::Done)
    ));
    assert!(matches!(
        k.handle().deliver_task_update(TaskUpdate {
            id: TaskId("t".into()),
            status: TaskStatus::Running,
            outcome: None,
            eta: None,
            check_hint: None,
            source: WakerSource::in_process_exit(json!(null)),
        }),
        Err(KernelError::Done)
    ));

    // Idle.
    let setup = Setup::new(FakeProvider::responses(vec![text_response("x")]));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    assert_eq!(k.status(), SessionStatus::Idle);
    k.end().await.unwrap();
    assert_eq!(k.status(), SessionStatus::Done);
    assert!(reader_ends_cleanly(&setup.events()));

    // Failed.
    let setup = Setup::new(FakeProvider::new(vec![Err(ProviderError::Auth(
        "no".into(),
    ))]));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Failed { .. }));
    k.end().await.unwrap();
    assert_eq!(k.status(), SessionStatus::Done);
}

#[tokio::test]
async fn end_from_suspended_cancels_open_tasks() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
    ]);
    let job = TaskTool::new("job");
    let setup = Setup::new(provider).tool(job.clone());
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    k.end().await.unwrap();
    let events = setup.events();
    let ended: Vec<&SessionEndedPayload> = find_all(&events, |b| match b {
        EventBody::SessionEnded(p) => Some(p),
        _ => None,
    });
    assert_eq!(ended[0].cancelled_task_ids, vec![TaskId("t1-c1".into())]);
    let task = &k.state().pending_tasks[&TaskId("t1-c1".into())];
    assert_eq!(task.status, TaskStatus::Cancelled);
    assert!(matches!(
        k.state().messages.last().unwrap().content[0],
        ContentBlock::TaskResult {
            status: TaskStatus::Cancelled,
            is_error: true,
            ..
        }
    ));
    let c = cancelled_events(&events);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].scope, CancelScopeKind::Task);
    assert_eq!(c[0].task_id, Some(TaskId("t1-c1".into())));
    // The in-process waker future was dropped: completing it now is a no-op.
    assert_eq!(job.pending_count(), 1);
}

#[tokio::test]
async fn ask_user_is_logged_before_and_after_invoke() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![(
            "q1",
            "ask_user",
            json!({"question": "Run both?", "options": ["both", "LC1"], "allow_free_text": true}),
        )]),
        text_response("ok"),
    ]);
    let host = FakeHost::new();
    lock(&host.answers).push_back(UserAnswer {
        question_id: "q1-q1".into(),
        answer: Some("both".into()),
    });
    let mut setup = Setup::new(provider).tool(Arc::new(AskUserTool));
    setup.host = host.clone();
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ks = kinds(&events);
    let p = ks.iter().position(|k| *k == "tool_call").unwrap();
    assert_eq!(
        &ks[p..p + 4],
        &["tool_call", "ask_user", "user_answer", "tool_result"]
    );
    let ask: Vec<&AskUserPayload> = find_all(&events, |b| match b {
        EventBody::AskUser(p) => Some(p),
        _ => None,
    });
    assert_eq!(ask[0].question_id, "q1-q1");
    assert_eq!(ask[0].question, "Run both?");
    assert_eq!(ask[0].options, ["both", "LC1"]);
    assert!(ask[0].allow_free_text);
    let ans: Vec<&UserAnswerPayload> = find_all(&events, |b| match b {
        EventBody::UserAnswer(p) => Some(p),
        _ => None,
    });
    assert_eq!(ans[0].question_id, "q1-q1");
    assert_eq!(ans[0].answer.as_deref(), Some("both"));
    assert!(!ans[0].declined);
    assert_eq!(lock(&host.asked)[0].question_id, "q1-q1");
    // The answer entered the context as the tool's result.
    assert!(matches!(
        &k.state().messages[2].content[0],
        ContentBlock::ToolResult { content: ToolResultContent::Json(v), .. } if v["answer"] == "both"
    ));
}

#[tokio::test]
async fn session_kind_tool_launches_lazily_once_and_is_terminated_on_suspend() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "repl", json!({"code": "1"})),
            ("c2", "repl", json!({"code": "2"})),
        ]),
        tool_use_response(vec![("c3", "job", json!({}))]),
        text_response("waiting"),
    ]);
    let sandbox = FakeSandbox::new();
    let mut setup = Setup::new(provider)
        .tool(Arc::new(SessionTool))
        .tool(TaskTool::new("job"));
    setup.sandbox = sandbox.clone();
    let mut k = setup.create().await;
    assert_eq!(sandbox.launch_count(), 0, "launched lazily");
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    assert_eq!(sandbox.launch_count(), 1, "one process per tool per kernel");
    assert_eq!(lock(&sandbox.launches)[0].1.program, "fake-repl");
    assert!(lock(&sandbox.terminated_flags)[0].load(std::sync::atomic::Ordering::SeqCst));
    let events = setup.events();
    let res = tool_results(&events);
    assert_eq!(
        res[0].content,
        ToolResultContent::Json(json!({"echo": {"code": "1"}}))
    );
    assert_eq!(
        res[1].content,
        ToolResultContent::Json(json!({"echo": {"code": "2"}}))
    );
}

#[tokio::test]
async fn dead_session_process_is_relaunched_once() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "repl", json!({}))]),
        tool_use_response(vec![("c2", "repl", json!({}))]),
        text_response("done"),
    ]);
    let sandbox = FakeSandbox::new();
    let mut setup = Setup::new(provider).tool(Arc::new(SessionTool));
    setup.sandbox = sandbox.clone();
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run_turn().await.unwrap_err().to_string(),
        "session is not running (status Created)"
    );
    // Apply the message and run turn 1.
    assert!(k.apply_queued_input().await.unwrap());
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    assert_eq!(sandbox.launch_count(), 1);
    // Kill the process; the next invoke relaunches once.
    lock(&sandbox.alive_flags)[0].store(false, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(k.run_turn().await.unwrap(), TurnOutcome::Continue);
    assert_eq!(sandbox.launch_count(), 2);
    assert!(!tool_results(&setup.events())[1].is_error);
}

#[tokio::test]
async fn events_are_broadcast_post_write_and_handle_reports_session_id() {
    let setup = Setup::new(FakeProvider::responses(vec![text_response("x")]));
    let mut k = setup.create().await;
    let h = k.handle();
    assert_eq!(h.session_id(), &SessionId("s_test".into()));
    assert!(h.subscribe_deltas().is_none());
    let mut rx = h.subscribe();
    h.enqueue_user_message(Message::user_text("go")).unwrap();
    k.run().await.unwrap();
    let mut got = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        got.push(ev.body.kind().to_owned());
    }
    assert_eq!(
        got,
        [
            "user_message",
            "checkpoint",
            "model_request",
            "model_response",
            "checkpoint"
        ]
    );
}

#[tokio::test]
async fn tool_errors_become_error_results_and_the_turn_continues() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "denied", json!({})),
            ("c2", "bad_blocks", json!({})),
            ("c3", "nan", json!({})),
            ("c4", "bad_task", json!({})),
        ]),
        text_response("done"),
    ]);
    let denied = ValueTool::with_result("denied", Err(ToolError::Denied("no".into())));
    let bad_blocks = ValueTool::with_result(
        "bad_blocks",
        Ok(ToolResult::Blocks(vec![
            ContentBlock::Text { text: "t".into() },
            ContentBlock::ToolUse {
                id: "x".into(),
                name: "y".into(),
                input: json!({}),
            },
        ])),
    );
    let nan = ValueTool::with_result("nan", Ok(ToolResult::Value(json!({"v": "ok"}))));
    // A terminal task handle is an InvalidTaskHandle.
    let bad_task = ValueTool::with_result(
        "bad_task",
        Ok(ToolResult::Task(TaskHandle {
            id: TaskId("t1-c4".into()),
            status: TaskStatus::Succeeded,
            eta: None,
            check_hint: None,
            description: None,
        })),
    );
    let setup = Setup::new(provider)
        .tool(denied)
        .tool(bad_blocks)
        .tool(nan)
        .tool(bad_task);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let res = tool_results(&events);
    assert!(res[0].is_error);
    assert_eq!(
        res[0].content,
        ToolResultContent::Json(json!({"error": "denied by policy: no"}))
    );
    assert!(res[1].is_error);
    assert!(!res[2].is_error);
    assert!(res[3].is_error);
    assert!(res[3].task.is_none());
    assert!(k.state().pending_tasks.is_empty());
    assert_invariant_2(k.state());
}

#[tokio::test]
async fn blocks_result_with_image_reports_its_handle() {
    let handle = ArtifactHandle(h("ab"));
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "img", json!({}))]),
        text_response("done"),
    ]);
    let img = ValueTool::with_result(
        "img",
        Ok(ToolResult::Blocks(vec![
            ContentBlock::Text { text: "see".into() },
            ContentBlock::Image {
                artifact_handle: handle.clone(),
                mime: "image/png".into(),
            },
        ])),
    );
    let setup = Setup::new(provider).tool(img);
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let events = setup.events();
    let res = tool_results(&events);
    assert_eq!(res[0].artifact_handles, vec![handle]);
    assert!(!res[0].spilled);
}

//! P1.4: for every loop scenario the P1.2 fakes support, (1) `Recorder::cassette() ==
//! Cassette::from_log(log)` after a live run and (2) a second kernel built from `ReplayProvider`
//! plus `ReplayTool`s and driven by `ReplayDriver` produces a log that `diff_logs` finds
//! byte-identical (D16); plus cache misses, the replay clock, and the waker kind.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use kernel::loop_::Kernel;
use kernel::replay::*;
use kernel::*;
use serde_json::json;
use support::*;

// ---- helpers ---------------------------------------------------------------------------------

fn live(provider: Arc<dyn Provider>) -> (Setup, Arc<Recorder>) {
    let rec = Arc::new(Recorder::new());
    let setup = Setup::new(provider).middleware(Recorder::entry(rec.clone()));
    (setup, rec)
}

async fn create_attached(setup: &Setup, rec: &Recorder) -> Kernel {
    let k = setup.create().await;
    rec.attach(&k.handle());
    k
}

/// (1): the in-memory cassette equals the one rebuilt from the log.
fn assert_recorder_matches_log(setup: &Setup, rec: &Recorder) -> Cassette {
    let recorded = rec.cassette();
    let from_log = Cassette::from_log(&*setup.log.reader()).unwrap();
    assert_eq!(
        recorded, from_log,
        "Recorder::cassette() != Cassette::from_log()"
    );
    assert_eq!(rec.lagged(), 0);
    recorded
}

struct Replayed {
    events: Vec<Event>,
    stop: Result<RunStop, ReplayError>,
    kernel: Kernel,
    elapsed: Duration,
    recorder: Arc<Recorder>,
}

/// Build a replay kernel from the live setup's tools/chain/config, a `ReplayProvider`, a fresh
/// replay-mode memory log under `session_id`, and drive it through `cassette`.
async fn replay_with(
    live: &Setup,
    cassette: Cassette,
    session_id: &str,
    customize: impl FnOnce(&mut Setup),
) -> Replayed {
    let cassette = Arc::new(cassette);
    let driver = ReplayDriver::new(cassette.clone());
    let mut setup = Setup::new(Arc::new(driver.provider()));
    for t in &live.tools {
        setup = setup.tool(Arc::new(driver.tool(
            t.definition(),
            t.kind(),
            t.capabilities(),
        )));
    }
    let rec = Arc::new(Recorder::new());
    for e in &live.middleware {
        setup = if e.name == RECORDER_NAME {
            setup.middleware(Recorder::entry(rec.clone()))
        } else {
            setup.middleware(MiddlewareEntry {
                name: e.name.clone(),
                priority: e.priority,
                source: e.source,
                config_hash: e.config_hash.clone(),
                middleware: e.middleware.clone(),
            })
        };
    }
    setup.spill = live.spill.clone();
    setup.retry = live.retry.clone();
    setup.limits = live.limits.clone();
    setup.grants = live.grants.clone();
    setup.system_prompt = live.system_prompt.clone();
    setup.session_id = SessionId(session_id.to_owned());
    setup.log = Arc::new(MemoryEventLog::new(
        setup.session_id.clone(),
        setup.redactor.clone(),
    ));
    // `event-schema.md` §5.4: a replay log opens with `mode: replay`.
    setup
        .log
        .append(EventBody::LogOpened(LogOpenedPayload {
            event_schema_version: EVENT_SCHEMA_VERSION,
            state_schema_version: STATE_SCHEMA_VERSION,
            kernel_version: KERNEL_VERSION.to_owned(),
            mode: LogMode::Replay,
        }))
        .await
        .unwrap();
    customize(&mut setup);
    let start = Instant::now();
    let mut kernel = setup.create().await;
    rec.attach(&kernel.handle());
    let stop = driver.drive(&mut kernel).await;
    let elapsed = start.elapsed();
    Replayed {
        events: setup.events(),
        stop,
        kernel,
        elapsed,
        recorder: rec,
    }
}

async fn replay(live: &Setup, cassette: Cassette) -> Replayed {
    replay_with(live, cassette, "s_replay", |_| {}).await
}

fn log_of(events: &[Event]) -> MemoryEventLog {
    MemoryEventLog::from_events(
        SessionId("x".into()),
        Arc::new(Redactor::new()),
        events.to_vec(),
    )
}

fn diff(recorded: &[Event], replayed: &[Event]) -> DiffReport {
    diff_logs(&*log_of(recorded).reader(), &*log_of(replayed).reader()).unwrap()
}

fn assert_identical(recorded: &[Event], replayed: &[Event]) {
    let report = diff(recorded, replayed);
    assert!(
        report.identical,
        "logs diverge: {:?}\nrecorded kinds: {:?}\nreplayed kinds: {:?}",
        report.first_diff,
        kinds(recorded),
        kinds(replayed)
    );
    assert!(report.compared > 0);
}

/// (2): replay `cassette` against `live`'s configuration and require a byte-identical log.
async fn assert_replays_identically(live: &Setup, cassette: Cassette) -> Replayed {
    let r = replay(live, cassette.clone()).await;
    assert_identical(&live.events(), &r.events);
    // The replay run's own Recorder sees the same cassette (a replay is itself a recording).
    assert_eq!(r.recorder.cassette().model, cassette.model);
    assert_eq!(r.recorder.cassette().tools, cassette.tools);
    assert_eq!(r.recorder.cassette().inputs, cassette.inputs);
    // The replay log carries `mode: replay` and the replay provider, and is a log in its own right.
    let opened: Vec<&LogOpenedPayload> = find_all(&r.events, |b| match b {
        EventBody::LogOpened(p) => Some(p),
        _ => None,
    });
    assert_eq!(opened[0].mode, LogMode::Replay);
    let created: Vec<&SessionCreatedPayload> = find_all(&r.events, |b| match b {
        EventBody::SessionCreated(p) => Some(p),
        _ => None,
    });
    assert_eq!(created[0].provider, "replay");
    assert!(
        log_of(&r.events)
            .reader()
            .restore_latest(&MigrationRegistry::new())
            .unwrap()
            .is_some()
    );
    r
}

fn outcome(v: serde_json::Value, is_error: bool) -> TaskOutcome {
    TaskOutcome {
        content: ToolResultContent::Json(v),
        is_error,
        artifact_handles: vec![],
    }
}

fn user_message_at(c: &Cassette, i: usize) -> AppliedAt {
    match &c.inputs[i] {
        ReplayInput::UserMessage { applied, .. } => applied.clone(),
        other => panic!("input {i} is not a user message: {other:?}"),
    }
}

// ---- scenarios: record (1) and replay (2) ---------------------------------------------------

#[tokio::test]
async fn plain_turn_records_and_replays() {
    let (setup, rec) = live(FakeProvider::responses(vec![text_response("hello")]));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("hi"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.session_id, SessionId("s_test".into()));
    assert_eq!(c.model.len(), 1);
    assert!(c.tools.is_empty());
    assert_eq!(c.inputs.len(), 1);
    assert_eq!(
        user_message_at(&c, 0),
        AppliedAt {
            turn: 0,
            at: AppliedPoint::Created
        }
    );
    let key = c.model.keys().next().unwrap();
    let events = setup.events();
    let req = model_requests(&events)[0];
    assert_eq!(key.checkpoint_hash, req.checkpoint_hash);
    assert_eq!(key.request_hash, req.request_hash);
    assert_eq!(c.model[key].content, text_response("hello").content);
    assert!(c.model[key].response_id.is_none());

    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(r.stop.unwrap(), RunStop::Idle);
    assert_eq!(r.kernel.status(), SessionStatus::Idle);
    // A different session id hits the same keys (`event-schema.md` §5.2).
    assert_eq!(r.events[0].session_id, SessionId("s_replay".into()));
    assert_eq!(
        r.kernel.state().state_hash().unwrap(),
        checkpoints(&setup.events()).last().unwrap().state_hash
    );
}

#[tokio::test]
async fn tool_turn_records_and_replays_without_invoking_a_live_tool() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"x": 1}))]),
        text_response("done"),
    ]));
    let echo = ValueTool::new("echo", json!({"echoed": true}));
    let setup = setup.tool(echo.clone());
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.model.len(), 2);
    assert_eq!(c.tools.len(), 1);
    let events = setup.events();
    let call = tool_calls(&events)[0];
    let key = CassetteKey {
        checkpoint_hash: call.checkpoint_hash.clone(),
        request_hash: call.request_hash.clone(),
    };
    let out = &c.tools[&key];
    assert_eq!(
        out.content,
        ToolResultContent::Json(json!({"echoed": true}))
    );
    assert_eq!(out.origin, ToolOutputOrigin::Invoke);
    assert!(!out.in_process_waker);

    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(r.stop.unwrap(), RunStop::Idle);
    assert_eq!(
        echo.call_count(),
        1,
        "the live tool was never invoked again"
    );
    assert_invariant_2(r.kernel.state());
}

#[tokio::test]
async fn multi_tool_turn_with_errors_and_blocks_records_and_replays() {
    let handle = ArtifactHandle(h("ab"));
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "a", json!({})),
            ("c2", "denied", json!({})),
            ("c3", "a", json!({"again": true})),
            ("c4", "img", json!({})),
        ]),
        text_response("done"),
    ]));
    let setup = setup
        .tool(ValueTool::new("a", json!("A")))
        .tool(ValueTool::with_result(
            "denied",
            Err(ToolError::Denied("no".into())),
        ))
        .tool(ValueTool::with_result(
            "img",
            Ok(ToolResult::Blocks(vec![
                ContentBlock::Text { text: "see".into() },
                ContentBlock::Image {
                    artifact_handle: handle.clone(),
                    mime: "image/png".into(),
                },
            ])),
        ));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.tools.len(), 4);
    assert!(c.tools.values().any(|o| o.is_error));
    assert!(
        c.tools
            .values()
            .any(|o| o.artifact_handles == vec![handle.clone()])
    );
    assert_replays_identically(&setup, c).await;
}

#[tokio::test]
async fn task_suspend_and_waker_resume_records_and_replays() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("I'll wait."),
        text_response("job done, thanks"),
    ]));
    let job = TaskTool::new("job");
    let setup = setup.tool(job.clone());
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    let RunStop::Suspended(s) = k.run().await.unwrap() else {
        panic!()
    };
    assert_eq!(s.in_process_wakers, 1);
    // Halfway: the cassette so far already matches the log.
    assert_recorder_matches_log(&setup, &rec);
    job.complete(outcome(json!({"exit_code": 0}), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    // The recorded task output carries the full handle (check_hint from `task_started`) and the waker flag.
    let out = c.tools.values().next().unwrap();
    let handle = out.task.as_ref().unwrap();
    assert_eq!(handle.id, TaskId("t1-c1".into()));
    assert_eq!(handle.eta, Some(Duration::from_secs(60)));
    assert_eq!(handle.description.as_deref(), Some("fake job"));
    assert_eq!(
        handle.check_hint,
        Some(json!({"pid": 4242, "secret_hint": true}))
    );
    assert!(out.in_process_waker);
    // (7) The recorded task update has waker kind "replay".
    assert_eq!(c.inputs.len(), 2);
    let ReplayInput::TaskUpdate { applied, update } = &c.inputs[1] else {
        panic!("{:?}", c.inputs[1])
    };
    assert_eq!(
        *applied,
        AppliedAt {
            turn: 2,
            at: AppliedPoint::Suspended
        }
    );
    assert_eq!(update.source.kind, REPLAY_WAKER_KIND);
    assert_eq!(update.source, replay_waker());
    assert_eq!(update.status, TaskStatus::Succeeded);

    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(r.stop.unwrap(), RunStop::Idle);
    assert_eq!(r.kernel.state().turn, 3);
    let ups = task_updates(&r.events);
    assert_eq!(ups.len(), 1);
    assert_eq!(ups[0].waker.kind, "replay");
    assert_eq!(ups[0].applied.at, AppliedPoint::Suspended);
    assert_eq!(
        job.pending_count(),
        0,
        "the live waker was not re-registered"
    );
    // The replayed session suspended with a stand-in in-process waker, like the recording.
    let sus: Vec<&SuspendedPayload> = find_all(&r.events, |b| match b {
        EventBody::Suspended(p) => Some(p),
        _ => None,
    });
    assert_eq!(sus[0].in_process_wakers, 1);
}

#[tokio::test]
async fn task_started_then_suspend_only_replays_to_a_suspended_stop() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
    ]));
    let setup = setup.tool(TaskTool::new("job"));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    let RunStop::Suspended(live_s) = k.run().await.unwrap() else {
        panic!()
    };
    let c = assert_recorder_matches_log(&setup, &rec);
    let r = assert_replays_identically(&setup, c).await;
    let RunStop::Suspended(s) = r.stop.unwrap() else {
        panic!()
    };
    assert_eq!(s.pending_task_ids, live_s.pending_task_ids);
    assert_eq!(s.in_process_wakers, 1);
    assert_eq!(s.checkpoint_hash, live_s.checkpoint_hash);
}

#[tokio::test]
async fn user_message_queued_during_a_turn_records_and_replays() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "poke", json!({}))]),
        text_response("second"),
        text_response("third"),
    ]));
    let poke = HandleTool::new("poke", true, |h| {
        h.enqueue_user_message(Message::user_text("queued"))
            .unwrap();
    });
    let setup = setup.tool(poke.clone());
    let mut k = create_attached(&setup, &rec).await;
    poke.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    // Then an Idle message for a third turn.
    k.handle()
        .enqueue_user_message(Message::user_text("more"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.inputs.len(), 3);
    assert_eq!(
        user_message_at(&c, 1),
        AppliedAt {
            turn: 1,
            at: AppliedPoint::EndOfTurn
        }
    );
    assert_eq!(
        user_message_at(&c, 2),
        AppliedAt {
            turn: 2,
            at: AppliedPoint::Idle
        }
    );
    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(r.stop.unwrap(), RunStop::Idle);
    assert_eq!(r.kernel.state().turn, 3);
}

#[tokio::test]
async fn spilled_results_record_and_replay_including_a_failed_store() {
    let big = "€".repeat(1500);
    let small_spill = SpillConfig {
        cap_bytes: 1024,
        head_bytes: 100,
        tail_bytes: 50,
    };
    // Stored spill (noop store returns the content hash).
    let (mut setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "big", json!({})), ("c2", "blocks", json!({}))]),
        text_response("done"),
    ]));
    setup.spill = small_spill.clone();
    let setup = setup
        .tool(ValueTool::new("big", json!({"stdout": big})))
        .tool(ValueTool::with_result(
            "blocks",
            Ok(ToolResult::Blocks(vec![
                ContentBlock::Text {
                    text: "small".into(),
                },
                ContentBlock::Text { text: big.clone() },
            ])),
        ));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(c.tools.values().all(|o| o.spilled));
    let r = assert_replays_identically(&setup, c).await;
    let res = tool_results(&r.events);
    assert!(res[0].spilled && res[0].spill.is_some());
    assert_eq!(res[0].spill, tool_results(&setup.events())[0].spill);
    assert_eq!(
        res[1].spill.as_ref().unwrap().mime,
        "text/plain; charset=utf-8"
    );

    // Store failure: `is_error`, no handle, `spill: null`.
    let (mut setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "big", json!({}))]),
        text_response("done"),
    ]));
    setup.spill = small_spill;
    setup.artifact_store = Arc::new(FailingArtifactStore);
    let setup = setup.tool(ValueTool::new("big", json!(big)));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    let out = c.tools.values().next().unwrap();
    assert!(out.spilled && out.is_error && out.artifact_handles.is_empty());
    let r = assert_replays_identically(&setup, c).await;
    assert!(tool_results(&r.events)[0].spill.is_none());
}

#[tokio::test]
async fn unregistered_tool_call_is_not_in_the_cassette_and_replays() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "nope", json!({"a": 1})),
            ("c2", "echo", json!({})),
        ]),
        text_response("done"),
    ]));
    let setup = setup.tool(ValueTool::new("echo", json!(1)));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(
        c.tools.len(),
        1,
        "the unregistered call never reached after_tool"
    );
    let r = assert_replays_identically(&setup, c).await;
    let res = tool_results(&r.events);
    assert_eq!(res[0].origin, ToolOutputOrigin::Unregistered);
    assert_eq!(res[1].origin, ToolOutputOrigin::Invoke);
}

#[tokio::test]
async fn tool_scope_cancellation_records_and_replays() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "canceller", json!({})),
            ("c2", "echo", json!({})),
        ]),
        text_response("after"),
    ]));
    let canceller = HandleTool::new("canceller", false, |h| {
        h.cancel(CancelScope::Tool {
            tool_use_id: "c1".into(),
        })
    });
    let setup = setup
        .tool(ValueTool::new("echo", json!("ok")))
        .tool(canceller.clone());
    let mut k = create_attached(&setup, &rec).await;
    canceller.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(
        c.tools
            .values()
            .any(|o| o.origin == ToolOutputOrigin::Cancelled)
    );
    let r = assert_replays_identically(&setup, c).await;
    let cancelled = cancelled_events(&r.events);
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].scope, CancelScopeKind::Tool);
}

#[tokio::test]
async fn turn_scope_cancellation_synthetic_results_are_not_in_the_cassette() {
    // Not replayable (timing-dependent, see the module doc), but the Recorder and `from_log` agree.
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "echo", json!({})),
            ("c2", "canceller", json!({})),
            ("c3", "echo", json!({})),
        ]),
        text_response("after"),
    ]));
    let canceller = HandleTool::new("canceller", false, |h| h.cancel(CancelScope::Turn));
    let setup = setup
        .tool(ValueTool::new("echo", json!("ok")))
        .tool(canceller.clone());
    let mut k = create_attached(&setup, &rec).await;
    canceller.attach(k.handle());
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.tools.len(), 1, "only c1 went through after_tool");
    assert_eq!(tool_results(&setup.events()).len(), 3);
}

#[tokio::test]
async fn middleware_replace_and_input_edit_record_and_replay() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"a": 1}))]),
        text_response("done"),
    ]));
    let echo = ValueTool::new("echo", json!("real"));
    let mut gate = FnMiddleware::empty();
    gate.before_tool = Some(Box::new(|_, call, _| {
        call.input = json!({"a": 1, "edited": true});
        Ok(ToolFlow::Replace(Ok(ToolResult::Value(json!("replaced")))))
    }));
    let setup = setup
        .tool(echo.clone())
        .middleware(FnMiddleware::entry("gate", 200, gate));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    let (key, out) = c.tools.iter().next().unwrap();
    assert_eq!(out.origin, ToolOutputOrigin::Middleware);
    // Keyed on the edited call (what `after_tool` and the log see).
    assert_eq!(
        key.request_hash,
        ToolCall {
            tool_use_id: "c1".into(),
            name: "echo".into(),
            input: json!({"a": 1, "edited": true})
        }
        .request_hash()
        .unwrap()
    );
    assert_replays_identically(&setup, c).await;
    assert_eq!(echo.call_count(), 0);
}

#[tokio::test]
async fn ask_user_answer_is_served_as_the_tool_result() {
    let (mut setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![(
            "q1",
            "ask_user",
            json!({"question": "Run both?", "options": ["both", "LC1"], "allow_free_text": true}),
        )]),
        text_response("ok"),
    ]));
    let host = FakeHost::new();
    lock(&host.answers).push_back(UserAnswer {
        question_id: "q1-q1".into(),
        answer: Some("both".into()),
    });
    setup.host = host.clone();
    let setup = setup.tool(Arc::new(AskUserTool));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(matches!(
        &c.inputs[1],
        ReplayInput::UserAnswer { question_id, answer }
            if question_id == "q1-q1" && answer.answer.as_deref() == Some("both")
    ));
    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(lock(&host.asked).len(), 1, "the host was never asked again");
    let ans: Vec<&UserAnswerPayload> = find_all(&r.events, |b| match b {
        EventBody::UserAnswer(p) => Some(p),
        _ => None,
    });
    assert_eq!(ans[0].answer.as_deref(), Some("both"));
}

#[tokio::test]
async fn session_tool_and_compaction_record_and_replay() {
    // A `Session`-kind tool (the replay tool reports the same kind and a stub command) and a
    // compaction that moves the checkpoint between `before_model` and the model call.
    let (mut setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "repl", json!({"code": "1"}))]),
        text_response("done"),
    ]));
    let sandbox = FakeSandbox::new();
    setup.sandbox = sandbox.clone();
    let mut compactor = FnMiddleware::empty();
    compactor.before_model = Some(Box::new(|_, _, cx| {
        if cx.turn == 2 {
            cx.request_compaction("squash");
        }
        Ok(())
    }));
    compactor.on_compact = Some(Box::new(|state, _| {
        state.messages = vec![Message::user_text("summary")];
        Ok(())
    }));
    let setup = setup
        .tool(Arc::new(SessionTool))
        .middleware(FnMiddleware::entry("compactor", 200, compactor));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert!(kinds(&setup.events()).contains(&"compaction"));
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.model.len(), 2);
    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(tool_calls(&r.events)[0].kind, Some(ToolKind::Session));
    assert_eq!(sandbox.launch_count(), 1);
}

#[tokio::test]
async fn a_replay_independent_failure_is_reproduced() {
    let mut bad = FnMiddleware::empty();
    bad.before_model = Some(Box::new(|_, req, _| {
        req.tools.push(ToolDefinition {
            name: "ghost".into(),
            description: "not registered".into(),
            input_schema: json!({}),
        });
        Ok(())
    }));
    let (setup, rec) = live(FakeProvider::responses(vec![text_response("a")]));
    let setup = setup.middleware(FnMiddleware::entry("bad", 200, bad));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(
        k.run().await.unwrap(),
        RunStop::Failed {
            error_class: "middleware".into()
        }
    );
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(c.model.is_empty());
    let r = assert_replays_identically(&setup, c).await;
    assert_eq!(
        r.stop.unwrap(),
        RunStop::Failed {
            error_class: "middleware".into()
        }
    );
}

#[tokio::test]
async fn a_failed_provider_turn_contributes_nothing_and_retry_leaves_the_cassette_alone() {
    // Two retryable failures then success: the cassette has the one response (attempts is volatile).
    let (setup, rec) = live(FakeProvider::new(vec![
        Err(ProviderError::Transport("x".into())),
        Err(ProviderError::RateLimited { retry_after: None }),
        Ok(text_response("finally")),
    ]));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.model.len(), 1);
    assert_replays_identically(&setup, c).await;

    // A non-retryable failure: `model_request` without a `model_response` contributes nothing.
    let (setup, rec) = live(FakeProvider::new(vec![Err(ProviderError::Auth(
        "no".into(),
    ))]));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Failed { .. }));
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(c.model.is_empty());
    assert_eq!(c.inputs.len(), 1);
}

// ---- (3) cache misses ------------------------------------------------------------------------

async fn recorded_tool_session() -> (Setup, Cassette) {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"x": 1}))]),
        text_response("done"),
    ]));
    let setup = setup.tool(ValueTool::new("echo", json!({"echoed": true})));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    (setup, c)
}

fn assert_replay_miss(events: &[Event], kernel: &Kernel) {
    let tf: Vec<&TurnFailedPayload> = find_all(events, |b| match b {
        EventBody::TurnFailed(p) => Some(p),
        _ => None,
    });
    assert_eq!(tf.len(), 1);
    assert_eq!(tf[0].error_class, "replay_miss");
    assert!(tf[0].message.contains("replay miss"));
    let sf: Vec<&SessionFailedPayload> = find_all(events, |b| match b {
        EventBody::SessionFailed(p) => Some(p),
        _ => None,
    });
    assert_eq!(sf[0].error_class, "replay_miss");
    assert!(sf[0].resumable);
    assert_eq!(kernel.status(), SessionStatus::Failed);
    assert_eq!(
        checkpoints(events).last().unwrap().session_status,
        SessionStatus::Failed
    );
    assert!(kernel::log::reader::ends_cleanly(events));
}

#[tokio::test]
async fn a_changed_system_prompt_is_a_provider_replay_miss_that_fails_the_turn() {
    let (setup, c) = recorded_tool_session().await;
    let r = replay_with(&setup, c, "s_replay", |s| {
        s.system_prompt = vec![PromptBlock::new(
            PromptBlockKind::Role,
            "test",
            "You are a DIFFERENT test.",
        )];
    })
    .await;
    // The only input was consumed, so the driver reports the stop rather than EarlyStop.
    assert_eq!(
        r.stop.unwrap(),
        RunStop::Failed {
            error_class: "replay_miss".into()
        }
    );
    assert_replay_miss(&r.events, &r.kernel);
    assert!(
        model_responses(&r.events).is_empty(),
        "no live call happened"
    );
    assert_eq!(model_requests(&r.events).len(), 1);
    // Not retried: one `turn_failed` with attempts 1 and no `provider_retry`.
    assert!(!kinds(&r.events).contains(&"provider_retry"));
    assert!(
        !ProviderError::ReplayMiss {
            checkpoint_hash: h("00"),
            request_hash: h("00")
        }
        .retryable()
    );
}

#[tokio::test]
async fn a_missing_tool_entry_is_a_tool_replay_miss_that_fails_the_turn() {
    let (setup, mut c) = recorded_tool_session().await;
    c.tools.clear();
    let r = replay(&setup, c).await;
    assert_eq!(
        r.stop.unwrap(),
        RunStop::Failed {
            error_class: "replay_miss".into()
        }
    );
    assert_replay_miss(&r.events, &r.kernel);
    assert_eq!(tool_calls(&r.events).len(), 1, "the call was logged");
    assert!(
        tool_results(&r.events).is_empty(),
        "a ReplayMiss is a turn failure, not an is_error result"
    );
    assert_eq!(r.kernel.state().messages.len(), 1, "partial turn discarded");
}

#[tokio::test]
async fn a_miss_with_inputs_left_is_early_stop() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
        text_response("done"),
    ]));
    let job = TaskTool::new("job");
    let setup = setup.tool(job.clone());
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    job.complete(outcome(json!(1), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let mut c = assert_recorder_matches_log(&setup, &rec);
    // Drop the second turn's response: turn 2 misses while the task update is still queued.
    let events = setup.events();
    let second = model_requests(&events)[1];
    let key = CassetteKey {
        checkpoint_hash: second.checkpoint_hash.clone(),
        request_hash: second.request_hash.clone(),
    };
    assert!(c.model.remove(&key).is_some());
    let r = replay(&setup, c).await;
    match r.stop {
        Err(ReplayError::EarlyStop(RunStop::Failed { error_class })) => {
            assert_eq!(error_class, "replay_miss");
        }
        other => panic!("expected EarlyStop, got {other:?}"),
    }
    assert_replay_miss(&r.events, &r.kernel);
}

// ---- tool key: exact through the tracker, fallback without --------------------------------------

async fn recorded_same_call_twice() -> (Setup, Cassette) {
    // The same `tool_use_id`/input in two turns with different outputs: the request hash alone is
    // ambiguous; the checkpoint half disambiguates.
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "flip", json!({}))]),
        tool_use_response(vec![("c1", "flip", json!({}))]),
        text_response("done"),
    ]));
    let setup = setup.tool(ValueTool::with_result(
        "flip",
        Ok(ToolResult::Value(json!("first"))),
    ));
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    assert_eq!(c.tools.len(), 2);
    let hashes: Vec<&Hash> = c.tools.keys().map(|k| &k.request_hash).collect();
    assert_eq!(hashes[0], hashes[1]);
    (setup, c)
}

#[tokio::test]
async fn the_driver_tracker_keys_tool_lookups_exactly() {
    let (setup, c) = recorded_same_call_twice().await;
    let r = assert_replays_identically(&setup, c).await;
    let res = tool_results(&r.events);
    assert_eq!(res[0].content, ToolResultContent::Json(json!("first")));
    assert_eq!(res[1].content, ToolResultContent::Json(json!(null)));
}

#[tokio::test]
async fn a_replay_tool_without_a_tracker_falls_back_to_the_request_hash_or_misses() {
    // Unique request hashes: the standalone `ReplayTool::new` serves them.
    let (setup, c) = recorded_tool_session().await;
    let cassette = Arc::new(c);
    let r = replay_with(&setup, (*cassette).clone(), "s_replay", |s| {
        s.tools = vec![Arc::new(ReplayTool::new(
            ToolDefinition {
                name: "echo".into(),
                description: "fake value tool".into(),
                input_schema: json!({"type": "object"}),
            },
            ToolKind::Stateless,
            vec![],
            cassette.clone(),
        ))];
    })
    .await;
    assert_eq!(r.stop.unwrap(), RunStop::Idle);
    assert_identical(&setup.events(), &r.events);

    // Ambiguous request hashes: a miss, never a guess.
    let (setup, c) = recorded_same_call_twice().await;
    let cassette = Arc::new(c);
    let r = replay_with(&setup, (*cassette).clone(), "s_replay", |s| {
        s.tools = vec![Arc::new(ReplayTool::new(
            ToolDefinition {
                name: "flip".into(),
                description: "fake value tool".into(),
                input_schema: json!({"type": "object"}),
            },
            ToolKind::Stateless,
            vec![],
            cassette.clone(),
        ))];
    })
    .await;
    assert_eq!(
        r.stop.unwrap(),
        RunStop::Failed {
            error_class: "replay_miss".into()
        }
    );
}

// ---- (4) the replay clock ----------------------------------------------------------------------

#[tokio::test]
async fn a_recorded_session_replays_in_under_a_second() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![
            ("c1", "echo", json!({"x": 1})),
            ("c2", "job", json!({})),
        ]),
        text_response("waiting"),
        tool_use_response(vec![("c3", "echo", json!({"x": 2}))]),
        text_response("done"),
    ]));
    let job = TaskTool::new("job");
    let setup = setup
        .tool(ValueTool::new("echo", json!({"echoed": true})))
        .tool(job.clone());
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    job.complete(outcome(json!({"exit_code": 0}), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(k.state().turn, 4);
    let c = assert_recorder_matches_log(&setup, &rec);
    let r = assert_replays_identically(&setup, c).await;
    assert!(
        r.elapsed < Duration::from_secs(1),
        "replay took {:?}",
        r.elapsed
    );
    eprintln!("replay of a 4-turn session took {:?}", r.elapsed);
}

// ---- cassette files ---------------------------------------------------------------------------

#[tokio::test]
async fn cassette_round_trips_through_json_and_files() {
    let (setup, rec) = live(FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "job", json!({}))]),
        text_response("waiting"),
        text_response("done"),
    ]));
    let job = TaskTool::new("job");
    let setup = setup.tool(job.clone());
    let mut k = create_attached(&setup, &rec).await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert!(matches!(k.run().await.unwrap(), RunStop::Suspended(_)));
    job.complete(outcome(json!({"exit_code": 0}), false));
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let c = assert_recorder_matches_log(&setup, &rec);
    let text = serde_json::to_string(&c).unwrap();
    let back: Cassette = serde_json::from_str(&text).unwrap();
    assert_eq!(back, c);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.cassette.json");
    c.write_to(&path).unwrap();
    let read = Cassette::read_from(&path).unwrap();
    assert_eq!(read, c);
    assert!(matches!(
        Cassette::read_from(&dir.path().join("missing.json")),
        Err(ReplayError::Io(_))
    ));
    // A file cassette replays like the in-memory one.
    assert_replays_identically(&setup, read).await;
}

#[tokio::test]
async fn from_log_on_an_empty_log_is_incomplete_and_the_driver_needs_a_first_input() {
    let empty = MemoryEventLog::new(SessionId("e".into()), Arc::new(Redactor::new()));
    assert!(matches!(
        Cassette::from_log(&*empty.reader()),
        Err(ReplayError::Incomplete(_))
    ));
    let (setup, rec) = live(FakeProvider::responses(vec![]));
    let _k = create_attached(&setup, &rec).await;
    let c = assert_recorder_matches_log(&setup, &rec);
    assert!(c.inputs.is_empty());
    let r = replay(&setup, c).await;
    assert!(matches!(r.stop, Err(ReplayError::Incomplete(_))));
}

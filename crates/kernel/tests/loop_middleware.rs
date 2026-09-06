//! P1.2: middleware chain (§7.6) — ordering, reserved priority, duplicate names, `Replace`,
//! the chain event, compaction requests, `emit` ordering, request tool validation.

mod support;

use kernel::loop_::{Kernel, KernelError};
use kernel::*;
use serde_json::json;
use support::*;

#[tokio::test]
async fn hooks_run_in_priority_order_for_every_hook_and_chain_is_logged() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({}))]),
        text_response("done"),
    ]);
    let log: HookLog = Default::default();
    let setup = Setup::new(provider)
        .tool(ValueTool::new("echo", json!(1)))
        // Inserted out of order; equal priorities keep insertion order (b before c).
        .middleware(RecordingMiddleware::entry("z_last", 900, &log))
        .middleware(RecordingMiddleware::entry("b", 300, &log))
        .middleware(RecordingMiddleware::entry("c", 300, &log))
        .middleware(RecordingMiddleware::entry(
            TOOL_CALL_PARSER_NAME,
            TOOL_CALL_PARSER_PRIORITY,
            &log,
        ))
        .middleware(RecordingMiddleware::entry("a", 200, &log));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    k.compact("test").await.unwrap();
    let hooks = lock(&log).clone();
    let order = ["tool_call_parser", "a", "b", "c", "z_last"];
    for hook in [
        "before_model",
        "after_model",
        "before_tool",
        "after_tool",
        "on_compact",
    ] {
        let seen: Vec<&str> = hooks
            .iter()
            .filter(|h| h.ends_with(&format!(":{hook}")))
            .map(|h| h.split(':').next().unwrap())
            .take(5)
            .collect();
        assert_eq!(seen, order, "order of {hook}");
    }
    let events = setup.events();
    let chain: Vec<&MiddlewareChainResolvedPayload> = find_all(&events, |b| match b {
        EventBody::MiddlewareChainResolved(p) => Some(p),
        _ => None,
    });
    assert_eq!(chain.len(), 1);
    let names: Vec<&str> = chain[0].chain.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, order);
    assert_eq!(
        chain[0].chain.iter().map(|e| e.index).collect::<Vec<_>>(),
        [0, 1, 2, 3, 4]
    );
    assert_eq!(
        chain[0].chain_hash,
        Hash::of_canonical_json(&chain[0].chain).unwrap()
    );
    // Compaction event and checkpoint.
    let ks = kinds(&events);
    let p = ks.iter().position(|k| *k == "compaction").unwrap();
    assert_eq!(ks[p + 1], "checkpoint");
    assert_eq!(
        checkpoints(&events).last().unwrap().reason,
        CheckpointReason::Compaction
    );
}

#[tokio::test]
async fn reserved_priority_and_duplicate_names_are_rejected() {
    let log: HookLog = Default::default();
    // A non-parser entry at or below the parser priority.
    let setup = Setup::new(FakeProvider::responses(vec![]))
        .middleware(RecordingMiddleware::entry("early", 100, &log));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::ReservedPriority(n)) if n == "early"
    ));
    let setup = Setup::new(FakeProvider::responses(vec![]))
        .middleware(RecordingMiddleware::entry("early", 50, &log));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::ReservedPriority(_))
    ));
    // The parser at the wrong priority.
    let setup = Setup::new(FakeProvider::responses(vec![])).middleware(RecordingMiddleware::entry(
        TOOL_CALL_PARSER_NAME,
        200,
        &log,
    ));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::ReservedPriority(n)) if n == TOOL_CALL_PARSER_NAME
    ));
    // Duplicate names.
    let setup = Setup::new(FakeProvider::responses(vec![]))
        .middleware(RecordingMiddleware::entry("dup", 200, &log))
        .middleware(RecordingMiddleware::entry("dup", 300, &log));
    assert!(matches!(
        Kernel::create(setup.config(), setup.init()).await,
        Err(KernelError::DuplicateMiddleware(n)) if n == "dup"
    ));
}

#[tokio::test]
async fn before_tool_replace_short_circuits_later_hooks_and_the_tool() {
    let provider = FakeProvider::responses(vec![
        tool_use_response(vec![("c1", "echo", json!({"a": 1}))]),
        text_response("done"),
    ]);
    let log: HookLog = Default::default();
    let echo = ValueTool::new("echo", json!("real"));
    let mut gate = FnMiddleware::empty();
    gate.before_tool = Some(Box::new(|_, call, _| {
        call.input = json!({"a": 1, "edited": true});
        Ok(ToolFlow::Replace(Ok(ToolResult::Value(json!("replaced")))))
    }));
    let setup = Setup::new(provider)
        .tool(echo.clone())
        .middleware(FnMiddleware::entry("gate", 200, gate))
        .middleware(RecordingMiddleware::entry("later", 300, &log));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    assert_eq!(echo.call_count(), 0, "invoke never ran");
    let hooks = lock(&log).clone();
    assert!(
        !hooks.contains(&"later:before_tool".to_owned()),
        "later hooks skipped"
    );
    assert!(
        hooks.contains(&"later:after_tool".to_owned()),
        "after_tool still runs"
    );
    let events = setup.events();
    let res = tool_results(&events)[0];
    assert_eq!(res.origin, ToolOutputOrigin::Middleware);
    assert_eq!(res.content, ToolResultContent::Json(json!("replaced")));
    assert_eq!(res.duration_ms, 0);
    // The edited call is what gets logged and hashed.
    let call = tool_calls(&events)[0];
    assert_eq!(call.input, json!({"a": 1, "edited": true}));
    assert_eq!(
        call.args_hash,
        Hash::of_canonical_json(&json!({"a": 1, "edited": true})).unwrap()
    );
}

#[tokio::test]
async fn before_model_may_drop_tools_but_not_add_unregistered_ones() {
    let provider = FakeProvider::responses(vec![text_response("a"), text_response("b")]);
    let mut drop_tools = FnMiddleware::empty();
    drop_tools.before_model = Some(Box::new(|_, req, cx| {
        assert_eq!(cx.registry.len(), 2);
        req.tools.retain(|t| t.name == "keep");
        Ok(())
    }));
    let setup = Setup::new(provider.clone())
        .tool(ValueTool::new("keep", json!(1)))
        .tool(ValueTool::new("drop", json!(2)))
        .middleware(FnMiddleware::entry("lazy", 200, drop_tools));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    assert_eq!(provider.requests()[0].tools.len(), 1);
    assert_eq!(model_requests(&setup.events())[0].tool_names, ["keep"]);

    let mut add_tool = FnMiddleware::empty();
    add_tool.before_model = Some(Box::new(|_, req, _| {
        req.tools.push(ToolDefinition {
            name: "ghost".into(),
            description: "not registered".into(),
            input_schema: json!({}),
        });
        Ok(())
    }));
    let setup = Setup::new(FakeProvider::responses(vec![text_response("a")]))
        .middleware(FnMiddleware::entry("bad", 200, add_tool));
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
    assert_eq!(tf[0].hook.as_deref(), Some("before_model"));
}

#[tokio::test]
async fn compaction_request_from_before_model_runs_on_compact_and_rebuilds_messages() {
    let provider = FakeProvider::responses(vec![text_response("a")]);
    let log: HookLog = Default::default();
    let mut req_mw = FnMiddleware::empty();
    req_mw.before_model = Some(Box::new(|_, _, cx| {
        cx.request_compaction("squash");
        Ok(())
    }));
    req_mw.on_compact = Some(Box::new(|state, _| {
        state.messages = vec![Message::user_text("summary")];
        Ok(())
    }));
    // A hook that asks for compaction outside before_model gets a warning.
    req_mw.after_model = Some(Box::new(|_, _, cx| {
        cx.request_compaction("late");
        Ok(())
    }));
    let setup = Setup::new(provider.clone())
        .middleware(FnMiddleware::entry("compactor", 200, req_mw))
        .middleware(RecordingMiddleware::entry("rec", 300, &log));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("a long message"))
        .unwrap();
    assert_eq!(k.run().await.unwrap(), RunStop::Idle);
    let events = setup.events();
    let ks = kinds(&events);
    assert_ordered(
        &ks,
        &["checkpoint", "compaction", "checkpoint", "model_request"],
    );
    let comp: Vec<&CompactionPayload> = find_all(&events, |b| match b {
        EventBody::Compaction(p) => Some(p),
        _ => None,
    });
    assert_eq!(comp[0].strategy, "squash");
    assert_eq!(comp[0].messages_before, 1);
    assert_eq!(comp[0].messages_after, 1);
    assert_ne!(comp[0].before_state_hash, comp[0].after_state_hash);
    assert_eq!(
        provider.requests()[0].messages,
        vec![Message::user_text("summary")]
    );
    assert!(warning_classes(&events).contains(&"compaction_request_ignored".to_owned()));
    assert!(lock(&log).contains(&"rec:on_compact".to_owned()));
    assert_invariant_1(&events);
}

#[tokio::test]
async fn emitted_events_land_where_the_hook_ran() {
    let provider = FakeProvider::responses(vec![text_response("a")]);
    let mut mw = FnMiddleware::empty();
    mw.after_model = Some(Box::new(|_, _, cx| {
        cx.emit(ExtensionEvent::ContextUsage(ContextUsagePayload {
            turn: cx.turn,
            input_tokens: 10,
            budget_tokens: 40000,
            over_budget: false,
            source: UsageSource::ProviderUsage,
        }))
        .unwrap();
        Ok(())
    }));
    let setup = Setup::new(provider).middleware(FnMiddleware::entry("budget", 200, mw));
    let mut k = setup.create().await;
    k.handle()
        .enqueue_user_message(Message::user_text("go"))
        .unwrap();
    k.run().await.unwrap();
    let ks = setup.kinds();
    let p = ks.iter().position(|k| k == "model_request").unwrap();
    assert_eq!(
        &ks[p..p + 3],
        &["model_request", "context_usage", "model_response"]
    );
}

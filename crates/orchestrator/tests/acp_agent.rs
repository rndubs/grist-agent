//! The ACP agent end to end, in process (ADR-0004): an SDK `Client` talks to `agent_component`
//! over a duplex byte stream, exactly the framing `grist-kernel` speaks on stdio and
//! `grist-daemon` on the socket. The kernel is real (shipped `profiles/` tree, `NativeHost`,
//! the `None` backend, the base tools, a file log); only the provider is scripted.
//!
//! Needs `--features dev-sandbox-none` (D14): the CI host has no `bwrap`.
#![cfg(feature = "dev-sandbox-none")]

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CancelNotification, CloseSessionRequest, ContentBlock as AcpBlock, CreateElicitationRequest,
    CreateElicitationResponse, ElicitationAcceptAction, ElicitationAction, ElicitationContentValue,
    InitializeRequest, ListSessionsRequest, LoadSessionRequest, NewSessionRequest, PromptRequest,
    SessionNotification, SessionUpdate, StopReason, TextContent, ToolCallStatus, ToolKind,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Responder};
use async_trait::async_trait;
use kernel::log::FileEventLog;
use kernel::{
    ContentBlock, EventLogReader, Hash, ModelRequest, ModelResponse, Provider, ProviderError,
    SessionStatus, StopReason as KStop, Usage,
};
use orchestrator::acp::grist::{EventNotification, StatusRequest, SubscribeRequest};
use orchestrator::acp::{Server, ServerOptions, agent_component};
use orchestrator::launcher::LaunchOptions;
use serde_json::{Value, json};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

// ---- scripted provider ----------------------------------------------------------------------

enum Step {
    Reply(ModelResponse),
    Slow(Duration, ModelResponse),
}

struct Scripted {
    steps: Mutex<VecDeque<Step>>,
    requests: Mutex<Vec<ModelRequest>>,
}

impl Scripted {
    fn new(steps: Vec<Step>) -> Arc<Scripted> {
        Arc::new(Scripted {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(Vec::new()),
        })
    }
    fn requests(&self) -> Vec<ModelRequest> {
        self.requests.lock().unwrap().clone()
    }
}

#[async_trait]
impl Provider for Scripted {
    fn name(&self) -> &str {
        "scripted"
    }
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests.lock().unwrap().push(req);
        let step = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("script exhausted");
        match step {
            Step::Reply(r) => Ok(r),
            Step::Slow(d, r) => {
                tokio::time::sleep(d).await;
                Ok(r)
            }
        }
    }
}

fn response(content: Vec<ContentBlock>, stop: KStop) -> ModelResponse {
    let raw = serde_json::to_vec(&content).unwrap();
    ModelResponse {
        content,
        stop_reason: stop,
        usage: Usage::default(),
        model_id: "stand-in/default".into(),
        raw_response_hash: Hash::of_bytes(&raw),
        response_id: None,
    }
}

fn text(t: &str) -> Step {
    Step::Reply(response(
        vec![ContentBlock::Text { text: t.into() }],
        KStop::EndTurn,
    ))
}

fn call(id: &str, name: &str, input: Value) -> Step {
    Step::Reply(response(
        vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }],
        KStop::ToolUse,
    ))
}

// ---- fixtures ---------------------------------------------------------------------------------

fn profiles_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../profiles")
        .canonicalize()
        .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    repo: PathBuf,
    launch: LaunchOptions,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".grist/skills")).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "Keep functions short.\n").unwrap();
    std::fs::write(repo.join("hello.txt"), "hello from the checkout\n").unwrap();
    std::fs::write(
        repo.join("job.sh"),
        "#!/bin/sh\nsleep 1\necho \"job output: $1\"\n",
    )
    .unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let launch = LaunchOptions {
        profiles_dir: profiles_dir(),
        state_dir: dir.path().join("state"),
        home,
        sandbox_override: Some("none".into()),
        question_capacity: 4,
        delta_capacity: 64,
    };
    Fixture {
        repo: repo.canonicalize().unwrap(),
        _dir: dir,
        launch,
    }
}

fn server(f: &Fixture, provider: Arc<Scripted>, single: bool) -> Arc<Server> {
    Server::new(ServerOptions {
        launch: f.launch.clone(),
        single_session: single,
        default_agent: "default".into(),
        provider_override: Some(provider),
    })
}

/// What the test client saw.
#[derive(Default)]
struct Seen {
    updates: Mutex<Vec<SessionUpdate>>,
    events: Mutex<Vec<Value>>,
    answer: Mutex<Option<String>>,
}

impl Seen {
    fn updates(&self) -> Vec<SessionUpdate> {
        self.updates.lock().unwrap().clone()
    }
    fn event_kinds(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| e["kind"].as_str().map(str::to_owned))
            .collect()
    }
}

/// Connect a test client to `server` and run `body` with the connection.
async fn with_client<F>(server: Arc<Server>, seen: Arc<Seen>, body: F)
where
    F: AsyncFnOnce(ConnectionTo<Agent>) -> Result<(), agent_client_protocol::Error>,
{
    let (client_writer, agent_reader) = tokio::io::duplex(64 * 1024);
    let (agent_writer, client_reader) = tokio::io::duplex(64 * 1024);
    tokio::spawn(agent_component(server).connect_to(ByteStreams::new(
        agent_writer.compat_write(),
        agent_reader.compat(),
    )));
    let s1 = seen.clone();
    let s2 = seen.clone();
    let s3 = seen.clone();
    Client
        .builder()
        .name("test-client")
        .on_receive_notification(
            async move |n: SessionNotification, _cx: ConnectionTo<Agent>| {
                s1.updates.lock().unwrap().push(n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: EventNotification, _cx: ConnectionTo<Agent>| {
                s2.events.lock().unwrap().push(n.event);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |_r: CreateElicitationRequest,
                        responder: Responder<CreateElicitationResponse>,
                        _cx: ConnectionTo<Agent>| {
                let answer = s3.answer.lock().unwrap().clone();
                let action = match answer {
                    Some(a) => ElicitationAction::Accept(ElicitationAcceptAction::new().content(
                        BTreeMap::from([("answer".to_owned(), ElicitationContentValue::String(a))]),
                    )),
                    None => ElicitationAction::Decline,
                };
                responder.respond(CreateElicitationResponse::new(action))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(
            ByteStreams::new(client_writer.compat_write(), client_reader.compat()),
            body,
        )
        .await
        .unwrap();
}

async fn start(cx: &ConnectionTo<Agent>, cwd: &Path) -> String {
    let init = cx
        .send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await
        .unwrap();
    assert_eq!(init.protocol_version, ProtocolVersion::V1);
    assert!(init.agent_capabilities.load_session);
    let new = cx
        .send_request(NewSessionRequest::new(cwd))
        .block_task()
        .await
        .unwrap();
    new.session_id.0.to_string()
}

async fn prompt(cx: &ConnectionTo<Agent>, id: &str, text: &str) -> StopReason {
    cx.send_request(PromptRequest::new(
        id.to_owned(),
        vec![AcpBlock::Text(TextContent::new(text))],
    ))
    .block_task()
    .await
    .unwrap()
    .stop_reason
}

fn log_kinds(launch: &LaunchOptions, id: &str) -> Vec<String> {
    let path = launch.sessions_dir().join(format!("{id}.jsonl"));
    FileEventLog::snapshot(&path)
        .unwrap()
        .iter()
        .map(|e| e.unwrap().body.kind().to_owned())
        .collect()
}

fn agent_text(updates: &[SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::AgentMessageChunk(c) => match &c.content {
                AcpBlock::Text(t) => Some(t.text.clone()),
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

// ---- tests ------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn coding_turn_streams_tool_calls_and_text_and_answers_status_and_list() {
    let f = fixture();
    let provider = Scripted::new(vec![
        call("c1", "read", json!({"path": "hello.txt"})),
        text("It says hello."),
    ]);
    let srv = server(&f, provider.clone(), false);
    let seen = Arc::new(Seen::default());
    let repo = f.repo.clone();
    let launch = f.launch.clone();
    with_client(srv, seen.clone(), async move |cx| {
        let id = start(&cx, &repo).await;
        assert!(id.starts_with("s_"));
        assert_eq!(
            prompt(&cx, &id, "What does hello.txt say?").await,
            StopReason::EndTurn
        );

        let updates = seen.updates();
        let tool_call = updates
            .iter()
            .find_map(|u| match u {
                SessionUpdate::ToolCall(c) => Some(c.clone()),
                _ => None,
            })
            .expect("a tool_call update");
        assert_eq!(tool_call.tool_call_id.0.as_ref(), "c1");
        assert_eq!(tool_call.kind, ToolKind::Read);
        assert_eq!(tool_call.status, ToolCallStatus::InProgress);
        assert_eq!(tool_call.raw_input, Some(json!({"path": "hello.txt"})));
        let done = updates
            .iter()
            .find_map(|u| match u {
                SessionUpdate::ToolCallUpdate(c) if c.tool_call_id.0.as_ref() == "c1" => {
                    Some(c.clone())
                }
                _ => None,
            })
            .expect("a tool_call update");
        assert_eq!(done.fields.status, Some(ToolCallStatus::Completed));
        assert!(
            done.fields.raw_output.as_ref().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains("hello from the checkout")
        );
        assert_eq!(agent_text(&updates), "It says hello.");

        // The model profile's tool-description override reached the request (launcher `Described`).
        let req = &provider.requests()[0];
        let bash = req.tools.iter().find(|t| t.name == "bash").unwrap();
        assert!(
            bash.description
                .starts_with("Run one shell command in a fresh sandbox")
        );

        let st = cx
            .send_request(StatusRequest {
                session_id: id.clone(),
            })
            .block_task()
            .await
            .unwrap();
        assert_eq!(st.status, SessionStatus::Idle);
        assert_eq!(st.turn, 2);
        assert!(st.pending_task_ids.is_empty());
        assert!(st.log_path.ends_with(&format!("{id}.jsonl")));

        let list = cx
            .send_request(ListSessionsRequest::new())
            .block_task()
            .await
            .unwrap();
        assert_eq!(list.sessions.len(), 1);
        assert_eq!(list.sessions[0].session_id.0.as_ref(), id);
        assert_eq!(list.sessions[0].cwd, repo);

        let kinds = log_kinds(&launch, &id);
        assert_eq!(kinds[0], "log_opened");
        assert!(kinds.contains(&"session_created".to_owned()));
        assert!(kinds.contains(&"tool_result".to_owned()));

        cx.send_request(CloseSessionRequest::new(id.clone()))
            .block_task()
            .await
            .unwrap();
        assert!(log_kinds(&launch, &id).last().unwrap() == "session_ended");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ask_user_round_trips_through_elicitation_and_the_event_subscription_sees_it() {
    let f = fixture();
    let provider = Scripted::new(vec![
        call(
            "c1",
            "ask_user",
            json!({"question": "Colour?", "options": ["red", "blue"], "allow_free_text": false}),
        ),
        text("You chose blue."),
    ]);
    let srv = server(&f, provider, false);
    let seen = Arc::new(Seen::default());
    *seen.answer.lock().unwrap() = Some("blue".into());
    let repo = f.repo.clone();
    with_client(srv, seen.clone(), async move |cx| {
        let id = start(&cx, &repo).await;
        cx.send_request(SubscribeRequest {
            session_id: id.clone(),
            kinds: None,
        })
        .block_task()
        .await
        .unwrap();
        assert_eq!(
            prompt(&cx, &id, "Pick a colour for me.").await,
            StopReason::EndTurn
        );
        assert_eq!(agent_text(&seen.updates()), "You chose blue.");
        let kinds = seen.event_kinds();
        assert!(kinds.contains(&"ask_user".to_owned()), "{kinds:?}");
        assert!(kinds.contains(&"user_answer".to_owned()), "{kinds:?}");
        let answer = seen
            .events
            .lock()
            .unwrap()
            .iter()
            .find(|e| e["kind"] == "user_answer")
            .cloned()
            .unwrap();
        assert_eq!(answer["payload"]["answer"], "blue");
        assert_eq!(answer["payload"]["declined"], false);
        // Every event of the log reached the subscriber, in order, envelope included.
        let seqs: Vec<u64> = seen
            .events
            .lock()
            .unwrap()
            .iter()
            .map(|e| e["seq"].as_u64().unwrap())
            .collect();
        assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscription_kinds_filter_and_unsubscribed_sessions_get_nothing() {
    let f = fixture();
    let provider = Scripted::new(vec![text("ok"), text("ok again")]);
    let srv = server(&f, provider, false);
    let seen = Arc::new(Seen::default());
    let repo = f.repo.clone();
    with_client(srv, seen.clone(), async move |cx| {
        let id = start(&cx, &repo).await;
        assert_eq!(prompt(&cx, &id, "hi").await, StopReason::EndTurn);
        assert!(seen.event_kinds().is_empty(), "nothing before subscribing");
        cx.send_request(SubscribeRequest {
            session_id: id.clone(),
            kinds: Some(vec!["checkpoint".into(), "model_response".into()]),
        })
        .block_task()
        .await
        .unwrap();
        assert_eq!(prompt(&cx, &id, "again").await, StopReason::EndTurn);
        let kinds = seen.event_kinds();
        assert!(!kinds.is_empty());
        assert!(
            kinds
                .iter()
                .all(|k| k == "checkpoint" || k == "model_response"),
            "{kinds:?}"
        );
        assert_eq!(agent_text(&seen.updates()), "okok again");
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_cancel_stops_the_turn_with_stop_reason_cancelled() {
    let f = fixture();
    let provider = Scripted::new(vec![
        Step::Slow(
            Duration::from_secs(3),
            response(
                vec![ContentBlock::Text {
                    text: "late".into(),
                }],
                KStop::EndTurn,
            ),
        ),
        text("after"),
    ]);
    let srv = server(&f, provider, false);
    let seen = Arc::new(Seen::default());
    let repo = f.repo.clone();
    let launch = f.launch.clone();
    with_client(srv, seen.clone(), async move |cx| {
        let id = start(&cx, &repo).await;
        cx.send_request(SubscribeRequest {
            session_id: id.clone(),
            kinds: Some(vec!["cancelled".into()]),
        })
        .block_task()
        .await
        .unwrap();
        let cx2 = cx.clone();
        let id2 = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            cx2.send_notification(CancelNotification::new(id2)).unwrap();
        });
        assert_eq!(
            prompt(&cx, &id, "take your time").await,
            StopReason::Cancelled
        );
        assert_eq!(seen.event_kinds(), vec!["cancelled".to_owned()]);
        assert!(log_kinds(&launch, &id).contains(&"cancelled".to_owned()));
        // The session is usable afterwards.
        assert_eq!(prompt(&cx, &id, "and now?").await, StopReason::EndTurn);
        assert!(agent_text(&seen.updates()).ends_with("after"));
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn run_script_suspends_and_the_process_exit_waker_resumes_within_one_prompt() {
    let f = fixture();
    let provider = Scripted::new(vec![
        call(
            "c1",
            "run_script",
            json!({"path": "job.sh", "args": ["alpha"]}),
        ),
        text("Started the job; waiting."),
        text("The job printed `job output: alpha`."),
    ]);
    let srv = server(&f, provider, false);
    let seen = Arc::new(Seen::default());
    let repo = f.repo.clone();
    let launch = f.launch.clone();
    with_client(srv, seen.clone(), async move |cx| {
        let id = start(&cx, &repo).await;
        assert_eq!(
            prompt(&cx, &id, "Run job.sh with alpha and report.").await,
            StopReason::EndTurn
        );
        // In-process continuation: `suspended`, then the waker's `task_update` applied from the
        // inbox (no `resumed` event; that is `Kernel::resume`/`open`, the cross-process path).
        let kinds = log_kinds(&launch, &id);
        let at = |k: &str| {
            kinds
                .iter()
                .position(|x| x == k)
                .unwrap_or_else(|| panic!("{k} missing from {kinds:?}"))
        };
        assert!(at("task_started") < at("suspended"));
        assert!(at("suspended") < at("task_update"));
        let text = agent_text(&seen.updates());
        assert!(
            text.ends_with("The job printed `job output: alpha`."),
            "{text}"
        );
        let task_done = seen.updates().iter().any(|u| {
            matches!(u,
            SessionUpdate::ToolCallUpdate(c) if c.tool_call_id.0.as_ref() == "task:t1-c1"
                && c.fields.status == Some(ToolCallStatus::Completed))
        });
        assert!(
            task_done,
            "the task's completion is projected onto its tool call"
        );
        let st = cx
            .send_request(StatusRequest {
                session_id: id.clone(),
            })
            .block_task()
            .await
            .unwrap();
        assert_eq!(st.status, SessionStatus::Idle);
        assert_eq!(st.turn, 3);
        assert_eq!(st.in_process_wakers, 0);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn session_load_replays_the_history_and_the_session_continues() {
    let f = fixture();
    let provider = Scripted::new(vec![text("First answer."), text("Second answer.")]);
    let seen1 = Arc::new(Seen::default());
    let repo = f.repo.clone();
    let id_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    {
        let srv = server(&f, provider.clone(), false);
        let repo = repo.clone();
        let id_cell = id_cell.clone();
        with_client(srv, seen1.clone(), async move |cx| {
            let id = start(&cx, &repo).await;
            assert_eq!(
                prompt(&cx, &id, "First question?").await,
                StopReason::EndTurn
            );
            *id_cell.lock().unwrap() = Some(id);
            Ok(())
        })
        .await;
    }
    let id = id_cell.lock().unwrap().clone().unwrap();
    // The first server is gone (dropped with its connection); give its kernel task a moment to
    // release the log.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let seen2 = Arc::new(Seen::default());
    let srv = server(&f, provider, false);
    let launch = f.launch.clone();
    with_client(srv, seen2.clone(), async move |cx| {
        cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
            .block_task()
            .await
            .unwrap();
        cx.send_request(LoadSessionRequest::new(id.clone(), repo.clone()))
            .block_task()
            .await
            .unwrap();
        let replayed = seen2.updates();
        let user: Vec<String> = replayed
            .iter()
            .filter_map(|u| match u {
                SessionUpdate::UserMessageChunk(c) => match &c.content {
                    AcpBlock::Text(t) => Some(t.text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect();
        assert_eq!(user, vec!["First question?".to_owned()]);
        assert_eq!(agent_text(&replayed), "First answer.");
        assert_eq!(
            prompt(&cx, &id, "Second question?").await,
            StopReason::EndTurn
        );
        assert_eq!(agent_text(&seen2.updates()), "First answer.Second answer.");
        let kinds = log_kinds(&launch, &id);
        assert!(kinds.contains(&"resumed".to_owned()), "{kinds:?}");
        let st = cx
            .send_request(StatusRequest {
                session_id: id.clone(),
            })
            .block_task()
            .await
            .unwrap();
        // `turn` counts model calls: one in the first process, one here.
        assert_eq!(st.turn, 2);
        Ok(())
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn single_session_mode_refuses_a_second_session() {
    let f = fixture();
    let provider = Scripted::new(vec![]);
    let srv = server(&f, provider, true);
    let seen = Arc::new(Seen::default());
    let repo = f.repo.clone();
    with_client(srv, seen, async move |cx| {
        let _id = start(&cx, &repo).await;
        let err = cx
            .send_request(NewSessionRequest::new(repo.clone()))
            .block_task()
            .await
            .expect_err("second session refused");
        assert!(
            format!("{err:?}").contains("exactly one session"),
            "{err:?}"
        );
        Ok(())
    })
    .await;
}

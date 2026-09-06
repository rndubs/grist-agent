//! The ACP agent component (ADR-0004, protocol v1 as the SDK ships it; v2 is its
//! `unstable_protocol_v2` feature and a follow-on). One [`Server`] holds the sessions a
//! connection may address; [`agent_component`] builds the handler set for one connection.
//!
//! Handlers run inside the SDK's dispatch loop and must return quickly, so a prompt is answered
//! from a spawned task once the kernel stops; everything the client sees meanwhile is a
//! `session/update` derived from the log ([`super::project`]) or a `_grist/event`.

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    ContentBlock as AcpBlock, CreateElicitationRequest, ElicitationAction, ElicitationContentValue,
    ElicitationFormMode, ElicitationSchema, ElicitationSessionScope, Implementation,
    InitializeRequest, InitializeResponse, ListSessionsRequest, ListSessionsResponse,
    LoadSessionRequest, LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities,
    SessionCloseCapabilities, SessionInfo, SessionListCapabilities, SessionNotification,
    SessionResumeCapabilities, StopReason, StringPropertySchema,
};
use agent_client_protocol::{Agent, Client, ConnectTo, ConnectionTo, Error, Responder};
use host::PendingQuestion;
use kernel::{
    CancelScope, EventLogReader, FileEventLog, KernelError, Message, Provider, ResumeCause,
    RunStop, SessionId,
};
use tokio::sync::broadcast;

use super::grist::{
    CancelRequest, CancelResponse, EventNotification, StatusRequest, StatusResponse,
    SubscribeRequest, SubscribeResponse, UnsubscribeRequest,
};
use super::project::{Projector, updates_for};
use super::session::Driver;
use crate::launcher::{
    LaunchError, LaunchOptions, Launched, SessionRecord, create_session, resume_session,
};

/// How a [`Server`] behaves.
pub struct ServerOptions {
    /// Profiles, state directory, backend override.
    pub launch: LaunchOptions,
    /// `grist-kernel` (stdio, D4) serves exactly one session and refuses a second `session/new`.
    pub single_session: bool,
    /// Catalog entry when `session/new` names none (`_meta.grist.agent`).
    pub default_agent: String,
    /// Replace the OpenAI-compatible provider (tests, replay).
    pub provider_override: Option<Arc<dyn Provider>>,
}

/// What the projector task shares with the handlers. Deliberately without the [`Driver`]: the
/// task must not keep the kernel alive after the server drops the session.
struct Shared {
    id: SessionId,
    /// `None`: not subscribed; `Some(None)`: every kind; `Some(Some(kinds))`: those kinds.
    subscription: Mutex<Option<Option<BTreeSet<String>>>>,
    cancel_requested: AtomicBool,
}

/// A live session as the protocol server sees it.
struct Live {
    id: SessionId,
    driver: Driver,
    shared: Arc<Shared>,
}

/// Session registry shared by every connection of one process.
pub struct Server {
    opts: ServerOptions,
    sessions: Mutex<HashMap<String, Arc<Live>>>,
}

fn internal(msg: impl ToString) -> Error {
    Error::internal_error().data(serde_json::json!({ "grist": msg.to_string() }))
}

fn launch_err(e: LaunchError) -> Error {
    match e {
        LaunchError::UnknownSession(s) => Error::resource_not_found(Some(s))
            .data(serde_json::json!({ "grist": "unknown session" })),
        LaunchError::Profile(lines) => {
            Error::invalid_params().data(serde_json::json!({ "grist": { "profile": lines } }))
        }
        other => internal(other),
    }
}

impl Server {
    /// A server with no sessions.
    pub fn new(opts: ServerOptions) -> Arc<Server> {
        Arc::new(Server {
            opts,
            sessions: Mutex::new(HashMap::new()),
        })
    }

    /// The launch options in force.
    pub fn launch_options(&self) -> &LaunchOptions {
        &self.opts.launch
    }

    fn get(&self, id: &str) -> Result<Arc<Live>, Error> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| Error::resource_not_found(Some(id.to_owned())))
    }

    fn insert(&self, live: Arc<Live>) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(live.id.0.clone(), live);
    }

    fn remove(&self, id: &str) -> Option<Arc<Live>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }

    fn count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// Ids of the live sessions.
    pub fn live_sessions(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    }

    /// Wire a launched kernel into the registry and start its projector and question tasks.
    fn adopt(self: &Arc<Self>, launched: Launched, cx: &ConnectionTo<Client>) -> Arc<Live> {
        let Launched {
            kernel,
            questions,
            deltas,
            log_path,
            ..
        } = launched;
        let id = kernel.handle().session_id().clone();
        let driver = Driver::spawn(kernel, log_path.display().to_string());
        let events = driver.handle().subscribe();
        let _ = log_path;
        let shared = Arc::new(Shared {
            id: id.clone(),
            subscription: Mutex::new(None),
            cancel_requested: AtomicBool::new(false),
        });
        let live = Arc::new(Live {
            id: id.clone(),
            driver,
            shared: shared.clone(),
        });
        self.insert(live.clone());
        tokio::spawn(project_task(shared, events, deltas, cx.clone()));
        tokio::spawn(question_task(id, questions, cx.clone()));
        live
    }
}

/// Events and deltas → `session/update` and (when subscribed) `_grist/event`.
async fn project_task(
    live: Arc<Shared>,
    mut events: broadcast::Receiver<Arc<kernel::Event>>,
    mut deltas: broadcast::Receiver<kernel::ModelDelta>,
    cx: ConnectionTo<Client>,
) {
    let mut projector = Projector::default();
    let sid = live.id.0.clone();
    let mut deltas_open = true;
    loop {
        tokio::select! {
            biased;
            d = deltas.recv(), if deltas_open => match d {
                Ok(delta) => {
                    if let Some(u) = projector.delta(&delta) {
                        let _ = cx.send_notification(SessionNotification::new(sid.clone(), u));
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => deltas_open = false,
            },
            e = events.recv() => match e {
                Ok(event) => {
                    for u in projector.event(&event) {
                        let _ = cx.send_notification(SessionNotification::new(sid.clone(), u));
                    }
                    let wanted = {
                        let sub = live.subscription.lock().unwrap_or_else(|e| e.into_inner());
                        match &*sub {
                            None => false,
                            Some(None) => true,
                            Some(Some(kinds)) => kinds.contains(event.body.kind()),
                        }
                    };
                    if wanted && let Ok(v) = serde_json::to_value(&*event) {
                        let _ = cx.send_notification(EventNotification { session_id: sid.clone(), event: v });
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}

/// `ask_user` questions → `elicitation/create` → answers (D17).
async fn question_task(
    id: SessionId,
    mut questions: tokio::sync::mpsc::Receiver<PendingQuestion>,
    cx: ConnectionTo<Client>,
) {
    while let Some(q) = questions.recv().await {
        let req = q.request();
        let mut message = req.question.clone();
        let schema = if req.options.is_empty() || req.allow_free_text {
            if !req.options.is_empty() {
                message.push_str("\nOptions: ");
                message.push_str(&req.options.join(", "));
            }
            ElicitationSchema::new().string("answer", true)
        } else {
            ElicitationSchema::new().property(
                "answer",
                StringPropertySchema::new().enum_values(req.options.clone()),
                true,
            )
        };
        let elicit = CreateElicitationRequest::new(
            ElicitationFormMode::new(ElicitationSessionScope::new(id.0.clone()), schema),
            message,
        );
        let sent = cx.send_request(elicit);
        let answer = match sent.block_task().await {
            Ok(resp) => match resp.action {
                ElicitationAction::Accept(a) => a.content.and_then(|mut c| {
                    c.remove("answer").map(|v| match v {
                        ElicitationContentValue::String(s) => s,
                        ElicitationContentValue::Integer(i) => i.to_string(),
                        ElicitationContentValue::Number(n) => n.to_string(),
                        ElicitationContentValue::Boolean(b) => b.to_string(),
                        ElicitationContentValue::StringArray(v) => v.join(", "),
                        other => serde_json::to_string(&other).unwrap_or_default(),
                    })
                }),
                _ => None,
            },
            Err(_) => None,
        };
        match answer {
            Some(a) => {
                q.answer(a);
            }
            None => {
                q.decline();
            }
        }
    }
}

/// The text of a prompt: every text block joined by newlines; resource links become their URI.
fn prompt_text(blocks: &[AcpBlock]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for b in blocks {
        match b {
            AcpBlock::Text(t) => parts.push(t.text.clone()),
            AcpBlock::ResourceLink(r) => parts.push(r.uri.clone()),
            _ => {}
        }
    }
    parts.join("\n")
}

/// `_meta.grist.{agent, profile_overrides}` of `session/new`.
fn session_meta(
    meta: Option<&agent_client_protocol::schema::v1::Meta>,
    default_agent: &str,
) -> Result<(String, toml::Table), Error> {
    let grist = meta.and_then(|m| m.get("grist"));
    let agent = grist
        .and_then(|g| g.get("agent"))
        .and_then(|v| v.as_str())
        .unwrap_or(default_agent)
        .to_owned();
    let overrides = match grist.and_then(|g| g.get("profile_overrides")) {
        None | Some(serde_json::Value::Null) => toml::Table::new(),
        Some(v) => serde_json::from_value::<toml::Table>(v.clone()).map_err(|e| {
            Error::invalid_params()
                .data(serde_json::json!({ "grist": format!("profile_overrides: {e}") }))
        })?,
    };
    Ok((agent, overrides))
}

/// The client-facing outcome of a driven run.
fn stop_response(stop: RunStop, cancelled: bool) -> Result<PromptResponse, Error> {
    match stop {
        RunStop::Idle | RunStop::Done => Ok(PromptResponse::new(if cancelled {
            StopReason::Cancelled
        } else {
            StopReason::EndTurn
        })),
        // Suspended on an external waker (none exist before P3.4): the turn is over for the client.
        RunStop::Suspended(_) => Ok(PromptResponse::new(StopReason::EndTurn)),
        // ADR-0004: neither `stopReason` nor a plain error says "failed with the checkpoint
        // intact and resumable"; the error's data does.
        RunStop::Failed { error_class } => Err(Error::internal_error().data(serde_json::json!({
            "grist": { "status": "failed", "error_class": error_class, "resumable": true }
        }))),
    }
}

fn kernel_err(e: KernelError) -> Error {
    internal(format!("kernel: {e}"))
}

/// History of a reopened session as `session/update`s, from its log (`session/load`).
fn history(
    log_path: &std::path::Path,
) -> Result<Vec<agent_client_protocol::schema::v1::SessionUpdate>, Error> {
    let reader = FileEventLog::snapshot(log_path).map_err(|e| internal(format!("log: {e}")))?;
    let mut out = Vec::new();
    for line in reader.iter() {
        let event = line.map_err(|e| internal(format!("log: {e}")))?;
        out.extend(updates_for(&event, true));
    }
    Ok(out)
}

/// The handler set for one connection to `server`.
pub fn agent_component(server: Arc<Server>) -> impl ConnectTo<Client> {
    let s = server;
    Agent
        .builder()
        .name("grist")
        .on_receive_request(
            async move |_req: InitializeRequest, responder: Responder<InitializeResponse>, _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(
                            AgentCapabilities::new()
                                .load_session(true)
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .resume(SessionResumeCapabilities::new())
                                        .close(SessionCloseCapabilities::new())
                                        .list(SessionListCapabilities::new()),
                                ),
                        )
                        .agent_info(Implementation::new("grist", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: NewSessionRequest, responder: Responder<NewSessionResponse>, cx: ConnectionTo<Client>| {
                    if s.opts.single_session && s.count() > 0 {
                        return responder.respond_with_error(
                            Error::invalid_request().data(serde_json::json!({ "grist": "this process serves exactly one session (D4)" })),
                        );
                    }
                    let (agent, overrides) = match session_meta(req.meta.as_ref(), &s.opts.default_agent) {
                        Ok(v) => v,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let id = SessionId(format!("s_{}", uuid::Uuid::now_v7().simple()));
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        let launched = create_session(
                            &s2.opts.launch,
                            id.clone(),
                            &req.cwd,
                            &agent,
                            overrides,
                            s2.opts.provider_override.clone(),
                        )
                        .await;
                        match launched {
                            Ok(l) => {
                                let live = s2.adopt(l, &cx);
                                let _ = responder.respond(NewSessionResponse::new(live.id.0.clone()));
                            }
                            Err(e) => {
                                let _ = responder.respond_with_error(launch_err(e));
                            }
                        }
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: PromptRequest, responder: Responder<PromptResponse>, _cx: ConnectionTo<Client>| {
                    let live = match s.get(&req.session_id.0) {
                        Ok(l) => l,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    let text = prompt_text(&req.prompt);
                    if text.trim().is_empty() {
                        return responder.respond_with_error(
                            Error::invalid_params().data(serde_json::json!({ "grist": "prompt has no text" })),
                        );
                    }
                    live.shared.cancel_requested.store(false, Ordering::SeqCst);
                    tokio::spawn(async move {
                        let r = match live.driver.prompt(Message::user_text(text)).await {
                            None => Err(internal("session has ended")),
                            Some(Err(e)) => Err(kernel_err(e)),
                            Some(Ok(stop)) => stop_response(stop, live.shared.cancel_requested.load(Ordering::SeqCst)),
                        };
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let s = s.clone();
                async move |n: CancelNotification, _cx: ConnectionTo<Client>| {
                    if let Ok(live) = s.get(&n.session_id.0) {
                        live.shared.cancel_requested.store(true, Ordering::SeqCst);
                        live.driver.handle().cancel(CancelScope::Turn);
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: CloseSessionRequest, responder: Responder<CloseSessionResponse>, _cx: ConnectionTo<Client>| {
                    let Some(live) = s.remove(&req.session_id.0) else {
                        return responder.respond_with_error(Error::resource_not_found(Some(req.session_id.0.to_string())));
                    };
                    tokio::spawn(async move {
                        let r = match live.driver.end().await {
                            None | Some(Ok(())) => Ok(CloseSessionResponse::new()),
                            Some(Err(e)) => Err(kernel_err(e)),
                        };
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |_req: ListSessionsRequest, responder: Responder<ListSessionsResponse>, _cx: ConnectionTo<Client>| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        let r = match SessionRecord::list(&s2.opts.launch).await {
                            Ok(records) => Ok(ListSessionsResponse::new(
                                records
                                    .into_iter()
                                    .map(|r| {
                                        SessionInfo::new(r.session_id.clone(), r.workdir.clone())
                                            .title(format!("{} · {}", r.agent, r.workdir.display()))
                                            .updated_at(r.created_at.clone())
                                    })
                                    .collect(),
                            )),
                            Err(e) => Err(launch_err(e)),
                        };
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: LoadSessionRequest, responder: Responder<LoadSessionResponse>, cx: ConnectionTo<Client>| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        let r = reopen(&s2, &req.session_id.0, &cx, true).await.map(|()| LoadSessionResponse::new());
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: ResumeSessionRequest, responder: Responder<ResumeSessionResponse>, cx: ConnectionTo<Client>| {
                    let s2 = s.clone();
                    tokio::spawn(async move {
                        let r = reopen(&s2, &req.session_id.0, &cx, false).await.map(|()| ResumeSessionResponse::new());
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ---- _grist/* -------------------------------------------------------------------------
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: SubscribeRequest, responder: Responder<SubscribeResponse>, _cx: ConnectionTo<Client>| {
                    match s.get(&req.session_id) {
                        Ok(live) => {
                            *live.shared.subscription.lock().unwrap_or_else(|e| e.into_inner()) =
                                Some(req.kinds.filter(|k| !k.is_empty()).map(|k| k.into_iter().collect()));
                            responder.respond(SubscribeResponse {})
                        }
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: UnsubscribeRequest, responder: Responder<SubscribeResponse>, _cx: ConnectionTo<Client>| {
                    match s.get(&req.session_id) {
                        Ok(live) => {
                            *live.shared.subscription.lock().unwrap_or_else(|e| e.into_inner()) = None;
                            responder.respond(SubscribeResponse {})
                        }
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: CancelRequest, responder: Responder<CancelResponse>, _cx: ConnectionTo<Client>| {
                    match s.get(&req.session_id) {
                        Ok(live) => {
                            if matches!(req.scope, CancelScope::Turn) {
                                live.shared.cancel_requested.store(true, Ordering::SeqCst);
                            }
                            live.driver.handle().cancel(req.scope);
                            responder.respond(CancelResponse {})
                        }
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let s = s.clone();
                async move |req: StatusRequest, responder: Responder<StatusResponse>, _cx: ConnectionTo<Client>| {
                    let live = match s.get(&req.session_id) {
                        Ok(l) => l,
                        Err(e) => return responder.respond_with_error(e),
                    };
                    tokio::spawn(async move {
                        let r = live.driver.status().await.ok_or_else(|| internal("session has ended"));
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
}

/// `session/load` (replay history) and `session/resume` (no replay): reopen from the record and
/// the log, adopt, and if `replay` send the history first.
async fn reopen(
    s: &Arc<Server>,
    id: &str,
    cx: &ConnectionTo<Client>,
    replay: bool,
) -> Result<(), Error> {
    if s.get(id).is_ok() {
        return Err(Error::invalid_request()
            .data(serde_json::json!({ "grist": "session is already open in this process" })));
    }
    if s.opts.single_session && s.count() > 0 {
        return Err(Error::invalid_request()
            .data(serde_json::json!({ "grist": "this process serves exactly one session (D4)" })));
    }
    let record = SessionRecord::load(&s.opts.launch, &SessionId(id.to_owned()))
        .await
        .map_err(launch_err)?;
    let launched = resume_session(
        &s.opts.launch,
        SessionId(id.to_owned()),
        ResumeCause::Operator,
        s.opts.provider_override.clone(),
    )
    .await
    .map_err(launch_err)?;
    if replay {
        for u in history(&launched.log_path)? {
            cx.send_notification(SessionNotification::new(id.to_owned(), u))?;
        }
    }
    let _ = record;
    s.adopt(launched, cx);
    Ok(())
}

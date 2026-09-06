//! `grist-daemon`: the supervisor (D4, D11; ADR-0004 "What P1.9 implements"). It owns the unix
//! socket and its permissions, checks the peer's credentials, and for every session spawns one
//! `grist-kernel` process that it talks to as an ACP *client* over stdio, while being an ACP
//! *agent* to the connection that asked. Every message is forwarded typed and unchanged; the
//! daemon is a router, not a translator, and holds no protocol-only state beyond the
//! session-id → process map.
//!
//! Sessions belong to the connection that opened them: when it closes, its kernel processes are
//! killed. Nothing is lost by that (the log holds every checkpoint); a later `session/load`
//! reopens the session in a fresh process.

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse,
    CreateElicitationRequest, CreateElicitationResponse, Implementation, InitializeRequest,
    InitializeResponse, ListSessionsRequest, ListSessionsResponse, LoadSessionRequest,
    LoadSessionResponse, NewSessionRequest, NewSessionResponse, PromptRequest, PromptResponse,
    ResumeSessionRequest, ResumeSessionResponse, SessionCapabilities, SessionCloseCapabilities,
    SessionInfo, SessionListCapabilities, SessionNotification, SessionResumeCapabilities,
};
use agent_client_protocol::{
    Agent, ByteStreams, Client, ConnectTo, ConnectionTo, Error, Responder,
};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::Child;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use super::grist::{
    CancelRequest, CancelResponse, EventNotification, StatusRequest, StatusResponse,
    SubscribeRequest, SubscribeResponse, UnsubscribeRequest,
};
use crate::launcher::{LaunchOptions, SessionRecord};

/// How the daemon runs.
#[derive(Clone, Debug)]
pub struct DaemonOptions {
    /// The `grist-kernel` executable.
    pub kernel_bin: PathBuf,
    /// Passed to every kernel process as `GRIST_STATE_DIR` / `GRIST_PROFILES_DIR` (and used for
    /// `session/list` here).
    pub launch: LaunchOptions,
    /// Extra environment for every kernel process (on top of the daemon's own, which is
    /// inherited): endpoint URLs and secrets in tests, nothing in the shipped binary.
    pub env: Vec<(String, String)>,
}

/// One kernel process and the connection to it.
struct KernelProc {
    cx: ConnectionTo<Agent>,
    child: Mutex<Option<Child>>,
}

impl KernelProc {
    fn kill(&self) {
        if let Some(mut c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = c.start_kill();
        }
    }
}

impl Drop for KernelProc {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Per-connection state.
struct Conn {
    opts: Arc<DaemonOptions>,
    sessions: Mutex<HashMap<String, Arc<KernelProc>>>,
}

impl Conn {
    fn get(&self, id: &str) -> Result<Arc<KernelProc>, Error> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
            .ok_or_else(|| Error::resource_not_found(Some(id.to_owned())))
    }
    fn insert(&self, id: String, p: Arc<KernelProc>) {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, p);
    }
    fn remove(&self, id: &str) -> Option<Arc<KernelProc>> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id)
    }
    fn kill_all(&self) {
        for (_, p) in self
            .sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .drain()
        {
            p.kill();
        }
    }
}

fn internal(msg: impl ToString) -> Error {
    Error::internal_error().data(serde_json::json!({ "grist": msg.to_string() }))
}

/// Create the socket's directory (mode 0700), remove a stale socket, bind, and set the socket to
/// mode 0600 (D11: file permissions are the first half of the authentication).
pub async fn bind(socket: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = socket.parent().filter(|d| !d.as_os_str().is_empty()) {
        // Only a directory this daemon creates gets its mode set; an existing one (`/tmp`, a
        // user's own dir) is left as it is.
        if tokio::fs::metadata(dir).await.is_err() {
            tokio::fs::create_dir_all(dir).await?;
            tokio::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).await?;
        }
    }
    match tokio::fs::remove_file(socket).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let listener = UnixListener::bind(socket)?;
    tokio::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).await?;
    Ok(listener)
}

/// The second half of D11: the peer must be this uid.
pub fn peer_is_us(stream: &UnixStream) -> std::io::Result<bool> {
    let cred = stream.peer_cred()?;
    Ok(cred.uid() == nix::unistd::getuid().as_raw())
}

/// Accept loop: one [`connection_component`] per accepted, credential-checked connection.
pub async fn serve(listener: UnixListener, opts: Arc<DaemonOptions>) -> std::io::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        match peer_is_us(&stream) {
            Ok(true) => {}
            Ok(false) => {
                eprintln!("grist-daemon: refused a connection from another uid");
                continue;
            }
            Err(e) => {
                eprintln!("grist-daemon: peer credentials unavailable: {e}");
                continue;
            }
        }
        let opts = opts.clone();
        tokio::spawn(async move {
            let (rd, wr) = stream.into_split();
            let bytes = ByteStreams::new(wr.compat_write(), rd.compat());
            if let Err(e) = connection_component(opts).connect_to(bytes).await {
                eprintln!("grist-daemon: connection ended with error: {e}");
            }
        });
    }
}

/// Spawn a kernel process and connect to it as its client; every notification and request it
/// sends is forwarded to `client`.
fn spawn_kernel(conn: &Arc<Conn>, client: &ConnectionTo<Client>) -> Result<Arc<KernelProc>, Error> {
    let opts = &conn.opts;
    let mut cmd = tokio::process::Command::new(&opts.kernel_bin);
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .env("GRIST_STATE_DIR", &opts.launch.state_dir)
        .env("GRIST_PROFILES_DIR", &opts.launch.profiles_dir)
        .kill_on_drop(true);
    if let Some(b) = &opts.launch.sandbox_override {
        cmd.env("GRIST_SANDBOX_BACKEND", b);
    }
    for (k, v) in &opts.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| internal(format!("spawn {}: {e}", opts.kernel_bin.display())))?;
    let stdin = child.stdin.take().ok_or_else(|| internal("no stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| internal("no stdout"))?;
    let transport = ByteStreams::new(stdin.compat_write(), stdout.compat());
    let c1 = client.clone();
    let c2 = client.clone();
    let c3 = client.clone();
    let builder = Client
        .builder()
        .name("grist-daemon→kernel")
        .on_receive_notification(
            async move |n: SessionNotification, _cx: ConnectionTo<Agent>| c1.send_notification(n),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_notification(
            async move |n: EventNotification, _cx: ConnectionTo<Agent>| c2.send_notification(n),
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            async move |r: CreateElicitationRequest,
                        responder: Responder<CreateElicitationResponse>,
                        _cx: ConnectionTo<Agent>| {
                c3.send_request(r).forward_response_to(responder)
            },
            agent_client_protocol::on_receive_request!(),
        );
    let cx = client.spawn_connection(builder, transport)?;
    Ok(Arc::new(KernelProc {
        cx,
        child: Mutex::new(Some(child)),
    }))
}

/// Spawn and initialize a kernel process.
async fn kernel_ready(
    conn: &Arc<Conn>,
    client: &ConnectionTo<Client>,
) -> Result<Arc<KernelProc>, Error> {
    let p = spawn_kernel(conn, client)?;
    p.cx.send_request(InitializeRequest::new(ProtocolVersion::V1))
        .block_task()
        .await?;
    Ok(p)
}

/// The agent-side handler set for one client connection.
pub fn connection_component(opts: Arc<DaemonOptions>) -> impl ConnectTo<Client> {
    let conn = Arc::new(Conn {
        opts,
        sessions: Mutex::new(HashMap::new()),
    });
    Agent
        .builder()
        .name("grist-daemon")
        .on_receive_request(
            async move |_req: InitializeRequest, responder: Responder<InitializeResponse>, _cx: ConnectionTo<Client>| {
                responder.respond(
                    InitializeResponse::new(ProtocolVersion::V1)
                        .agent_capabilities(
                            AgentCapabilities::new().load_session(true).session_capabilities(
                                SessionCapabilities::new()
                                    .resume(SessionResumeCapabilities::new())
                                    .close(SessionCloseCapabilities::new())
                                    .list(SessionListCapabilities::new()),
                            ),
                        )
                        .agent_info(Implementation::new("grist-daemon", env!("CARGO_PKG_VERSION"))),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: NewSessionRequest, responder: Responder<NewSessionResponse>, cx: ConnectionTo<Client>| {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let r = async {
                            let p = kernel_ready(&conn, &cx).await?;
                            let resp = p.cx.send_request(req).block_task().await?;
                            conn.insert(resp.session_id.0.to_string(), p);
                            Ok::<_, Error>(resp)
                        }
                        .await;
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: LoadSessionRequest, responder: Responder<LoadSessionResponse>, cx: ConnectionTo<Client>| {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let id = req.session_id.0.to_string();
                        let r = async {
                            if conn.get(&id).is_ok() {
                                return Err(Error::invalid_request().data(serde_json::json!({ "grist": "session is already open on this connection" })));
                            }
                            let p = kernel_ready(&conn, &cx).await?;
                            let resp = p.cx.send_request(req).block_task().await?;
                            conn.insert(id, p);
                            Ok::<_, Error>(resp)
                        }
                        .await;
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: ResumeSessionRequest, responder: Responder<ResumeSessionResponse>, cx: ConnectionTo<Client>| {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let id = req.session_id.0.to_string();
                        let r = async {
                            if conn.get(&id).is_ok() {
                                return Err(Error::invalid_request().data(serde_json::json!({ "grist": "session is already open on this connection" })));
                            }
                            let p = kernel_ready(&conn, &cx).await?;
                            let resp = p.cx.send_request(req).block_task().await?;
                            conn.insert(id, p);
                            Ok::<_, Error>(resp)
                        }
                        .await;
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: PromptRequest, responder: Responder<PromptResponse>, _cx: ConnectionTo<Client>| {
                    match conn.get(&req.session_id.0) {
                        Ok(p) => p.cx.send_request(req).forward_response_to(responder),
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_notification(
            {
                let conn = conn.clone();
                async move |n: CancelNotification, _cx: ConnectionTo<Client>| {
                    if let Ok(p) = conn.get(&n.session_id.0) {
                        p.cx.send_notification(n)?;
                    }
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_notification!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: CloseSessionRequest, responder: Responder<CloseSessionResponse>, _cx: ConnectionTo<Client>| {
                    let id = req.session_id.0.to_string();
                    let Some(p) = conn.remove(&id) else {
                        return responder.respond_with_error(Error::resource_not_found(Some(id)));
                    };
                    p.cx.send_request(req).on_receiving_result(async move |r| {
                        let out = responder.respond_with_result(r);
                        p.kill();
                        out
                    })
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |_req: ListSessionsRequest, responder: Responder<ListSessionsResponse>, _cx: ConnectionTo<Client>| {
                    let conn = conn.clone();
                    tokio::spawn(async move {
                        let r = match SessionRecord::list(&conn.opts.launch).await {
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
                            Err(e) => Err(internal(e)),
                        };
                        let _ = responder.respond_with_result(r);
                    });
                    Ok(())
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        // ---- _grist/* forwarded to the owning kernel ------------------------------------------
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: SubscribeRequest, responder: Responder<SubscribeResponse>, _cx: ConnectionTo<Client>| {
                    match conn.get(&req.session_id) {
                        Ok(p) => p.cx.send_request(req).forward_response_to(responder),
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: UnsubscribeRequest, responder: Responder<SubscribeResponse>, _cx: ConnectionTo<Client>| {
                    match conn.get(&req.session_id) {
                        Ok(p) => p.cx.send_request(req).forward_response_to(responder),
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: CancelRequest, responder: Responder<CancelResponse>, _cx: ConnectionTo<Client>| {
                    match conn.get(&req.session_id) {
                        Ok(p) => p.cx.send_request(req).forward_response_to(responder),
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            {
                let conn = conn.clone();
                async move |req: StatusRequest, responder: Responder<StatusResponse>, _cx: ConnectionTo<Client>| {
                    match conn.get(&req.session_id) {
                        Ok(p) => p.cx.send_request(req).forward_response_to(responder),
                        Err(e) => responder.respond_with_error(e),
                    }
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_close({
            let conn = conn.clone();
            async move |_cx: ConnectionTo<Client>| {
                conn.kill_all();
                Ok(())
            }
        })
}

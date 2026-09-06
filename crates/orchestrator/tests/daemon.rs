//! `grist-daemon` end to end: the daemon (in-process `serve`) spawns the real `grist-kernel`
//! binary per session and proxies a unix-socket client to it; `grist-connect` (the real binary)
//! is then driven over its stdio the way an editor would. The kernel process talks to a fake
//! OpenAI-compatible endpoint served here, through the real `providers` client.
//!
//! Needs `--features dev-sandbox-none` (D14) so the spawned kernel can select the `None` backend.
#![cfg(feature = "dev-sandbox-none")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::schema::v1::{
    CloseSessionRequest, ContentBlock as AcpBlock, InitializeRequest, ListSessionsRequest,
    NewSessionRequest, PromptRequest, SessionNotification, SessionUpdate, StopReason, TextContent,
};
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use orchestrator::acp::daemon::{DaemonOptions, bind, serve};
use orchestrator::acp::grist::StatusRequest;
use orchestrator::launcher::LaunchOptions;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixStream};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// A one-route fake of `POST /v1/chat/completions` (streaming): every request gets `reply` as
/// one SSE chunk, then a `stop` chunk, then `[DONE]`.
async fn fake_llm(reply: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            tokio::spawn(async move {
                let (rd, mut wr) = sock.split();
                let mut rd = BufReader::new(rd);
                let mut line = String::new();
                let mut len = 0usize;
                loop {
                    line.clear();
                    if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
                        return;
                    }
                    if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap_or(0);
                    }
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0u8; len];
                rd.read_exact(&mut body).await.ok();
                let chunk = |delta: serde_json::Value, finish: Option<&str>| {
                    serde_json::json!({
                        "id": "chatcmpl-1", "object": "chat.completion.chunk", "created": 1,
                        "model": "fake", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]
                    })
                };
                let sse = format!(
                    "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                    chunk(
                        serde_json::json!({"role": "assistant", "content": reply}),
                        None
                    ),
                    chunk(serde_json::json!({}), Some("stop"))
                );
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    sse.len(),
                    sse
                );
                let _ = wr.write_all(resp.as_bytes()).await;
                let _ = wr.shutdown().await;
            });
        }
    });
    format!("http://{addr}/v1")
}

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
    socket: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(repo.join(".grist/skills")).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "Be brief.\n").unwrap();
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
    // Short path: unix socket paths are limited to ~104 bytes on macOS.
    let socket = std::env::temp_dir().join(format!("grist-t-{}.sock", std::process::id()));
    Fixture {
        repo: repo.canonicalize().unwrap(),
        _dir: dir,
        launch,
        socket,
    }
}

/// Environment the spawned kernel needs: the endpoint URL and the bearer secret the stand-in
/// model profile names (`auth = "bearer:LITELLM_CI_API_KEY"`).
fn provider_env(url: &str) -> Vec<(String, String)> {
    vec![
        ("GRIST_ENDPOINT_LITELLM_CI_URL".to_owned(), url.to_owned()),
        ("LITELLM_CI_API_KEY".to_owned(), "sk-test".to_owned()),
    ]
}

#[derive(Default)]
struct Seen {
    updates: Mutex<Vec<SessionUpdate>>,
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

/// Drive one client conversation over `transport`: initialize, new session, prompt, status,
/// list, close. Returns the session id.
async fn conversation<T>(
    transport: T,
    seen: Arc<Seen>,
    repo: PathBuf,
    expect_text: &'static str,
) -> String
where
    T: agent_client_protocol::ConnectTo<Client>,
{
    let s1 = seen.clone();
    let id_cell: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let id_out = id_cell.clone();
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
        .connect_with(transport, async move |cx: ConnectionTo<Agent>| {
            let init = cx
                .send_request(InitializeRequest::new(ProtocolVersion::V1))
                .block_task()
                .await?;
            assert_eq!(init.protocol_version, ProtocolVersion::V1);
            let new = cx
                .send_request(NewSessionRequest::new(repo.clone()))
                .block_task()
                .await?;
            let id = new.session_id.0.to_string();
            let p = cx
                .send_request(PromptRequest::new(
                    id.clone(),
                    vec![AcpBlock::Text(TextContent::new("Say hello."))],
                ))
                .block_task()
                .await?;
            assert_eq!(p.stop_reason, StopReason::EndTurn);
            assert_eq!(agent_text(&seen.updates.lock().unwrap()), expect_text);
            let st = cx
                .send_request(StatusRequest {
                    session_id: id.clone(),
                })
                .block_task()
                .await?;
            assert_eq!(st.turn, 1);
            let list = cx
                .send_request(ListSessionsRequest::new())
                .block_task()
                .await?;
            assert!(list.sessions.iter().any(|s| s.session_id.0.as_ref() == id));
            cx.send_request(CloseSessionRequest::new(id.clone()))
                .block_task()
                .await?;
            *id_out.lock().unwrap() = Some(id);
            Ok(())
        })
        .await
        .unwrap();
    id_cell.lock().unwrap().clone().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_spawns_a_kernel_per_session_and_proxies_the_socket() {
    let f = fixture();
    let url = fake_llm("Hello from the fake model.").await;
    let listener = bind(&f.socket).await.unwrap();
    let mode = std::fs::metadata(&f.socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "socket is owner-only (D11)");
    let dir_mode = std::fs::metadata(f.socket.parent().unwrap())
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert!(
        dir_mode & 0o077 == 0 || f.socket.parent() == Some(&std::env::temp_dir()),
        "{dir_mode:o}"
    );
    let opts = Arc::new(DaemonOptions {
        kernel_bin: PathBuf::from(env!("CARGO_BIN_EXE_grist-kernel")),
        launch: f.launch.clone(),
        env: provider_env(&url),
    });
    let daemon = tokio::spawn(serve(listener, opts));

    // 1. A raw socket client.
    let stream = UnixStream::connect(&f.socket).await.unwrap();
    let (rd, wr) = stream.into_split();
    let seen = Arc::new(Seen::default());
    let id = conversation(
        ByteStreams::new(wr.compat_write(), rd.compat()),
        seen,
        f.repo.clone(),
        "Hello from the fake model.",
    )
    .await;
    let log = f.launch.sessions_dir().join(format!("{id}.jsonl"));
    assert!(log.is_file(), "the kernel process wrote {}", log.display());
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("\"kind\":\"session_ended\""));

    // 2. The same conversation through the real `grist-connect` binary over its stdio.
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_grist-connect"))
        .arg("--socket")
        .arg(&f.socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let seen2 = Arc::new(Seen::default());
    let id2 = conversation(
        ByteStreams::new(stdin.compat_write(), stdout.compat()),
        seen2,
        f.repo.clone(),
        "Hello from the fake model.",
    )
    .await;
    assert_ne!(id, id2);
    // The forwarder exits once its stdin closes (the connection above ended).
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("grist-connect exits")
        .unwrap();
    assert!(status.success(), "{status:?}");

    daemon.abort();
    let _ = std::fs::remove_file(&f.socket);
}

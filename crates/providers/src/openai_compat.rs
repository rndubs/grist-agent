//! `OpenAiCompatProvider`: one client for every `/v1/chat/completions` endpoint, parameterised
//! by [`Quirks`] (ADR-0003).

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::StreamExt;
use kernel::{
    ArtifactStore, DeltaStream, HostError, HttpMethod, HttpRequest, HttpResponse, ModelDelta,
    ModelRequest, ModelResponse, NetHandle, Provider, ProviderError, SecretResolver,
};
use serde_json::Value;

use crate::accumulate::Accumulator;
use crate::error::{classify_host_error, classify_status};
use crate::quirks::{Auth, Quirks};
use crate::request::render_body;
use crate::sse::{SseEvent, SseParser};

/// Where and how to reach one endpoint. The base URL comes from the launcher (it resolves the
/// profile's `endpoint` name to `GRIST_ENDPOINT_<NAME>_URL`); it is never in a profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EndpointConfig {
    /// Base URL ending in `/v1` (e.g. `http://127.0.0.1:8080/v1`); `/chat/completions` is appended.
    pub base_url: String,
    /// Per-request timeout, applied to the connect/headers phase and as an idle timeout between
    /// body chunks. Also passed to the host as `HttpRequest.timeout`. `None` disables it here
    /// (the kernel still applies its own).
    pub timeout: Option<Duration>,
    /// Extra headers sent verbatim on every request (never credentials; use `Quirks::auth`).
    pub extra_headers: Vec<(String, String)>,
}

impl EndpointConfig {
    /// A config with the given base URL, a 120 s timeout, and no extra headers.
    pub fn new(base_url: impl Into<String>) -> Self {
        EndpointConfig {
            base_url: base_url.into(),
            timeout: Some(Duration::from_secs(120)),
            extra_headers: Vec::new(),
        }
    }

    /// The chat-completions URL.
    pub fn chat_completions_url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

/// Upper bound on how much of an error body is read.
const MAX_ERROR_BODY: usize = 64 * 1024;

/// The OpenAI-compatible client (vLLM, LiteLLM, llama.cpp, …).
pub struct OpenAiCompatProvider {
    name: String,
    endpoint: EndpointConfig,
    quirks: Quirks,
    net: Arc<dyn NetHandle>,
    secrets: Arc<dyn SecretResolver>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("name", &self.name)
            .field("endpoint", &self.endpoint)
            .field("quirks", &self.quirks)
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatProvider {
    /// Build a client. `name` is what `Provider::name` reports in logs (`"openai_compat"` is the
    /// conventional value; a launcher may use the endpoint name).
    pub fn new(
        name: impl Into<String>,
        endpoint: EndpointConfig,
        quirks: Quirks,
        net: Arc<dyn NetHandle>,
        secrets: Arc<dyn SecretResolver>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        OpenAiCompatProvider {
            name: name.into(),
            endpoint,
            quirks,
            net,
            secrets,
            artifacts,
        }
    }

    /// The quirk flags in force.
    pub fn quirks(&self) -> &Quirks {
        &self.quirks
    }

    /// The endpoint config in force.
    pub fn endpoint(&self) -> &EndpointConfig {
        &self.endpoint
    }

    /// Build the wire request: body from `render_body`, headers (content type, `X-Request-Id`,
    /// extras, and `Authorization` resolved through the `SecretResolver` right now — D10).
    async fn build_request(&self, req: &ModelRequest) -> Result<HttpRequest, ProviderError> {
        let body = render_body(req, &self.quirks, &self.artifacts).await?;
        let body = serde_json::to_vec(&body)
            .map_err(|e| ProviderError::InvalidResponse(format!("serialize body: {e}")))?;
        let mut headers: Vec<(String, String)> = vec![
            ("Content-Type".into(), "application/json".into()),
            ("Accept".into(), "text/event-stream".into()),
            ("X-Request-Id".into(), req.trace.request_id.clone()),
        ];
        headers.extend(self.endpoint.extra_headers.iter().cloned());
        if let Auth::Bearer(handle) = &self.quirks.auth {
            let secret = self.secrets.resolve_secret(handle).map_err(|e| {
                ProviderError::Auth(format!(
                    "secret `{}` could not be resolved: {e}",
                    handle.name()
                ))
            })?;
            headers.push((
                "Authorization".into(),
                format!("Bearer {}", secret.expose()),
            ));
        }
        Ok(HttpRequest {
            method: HttpMethod::Post,
            url: self.endpoint.chat_completions_url(),
            headers,
            body,
            timeout: self.endpoint.timeout,
        })
    }

    async fn send(&self, http: HttpRequest) -> Result<HttpResponse, ProviderError> {
        let timeout = self.endpoint.timeout;
        let fut = self.net.send(http);
        let res = match timeout {
            Some(t) => match tokio::time::timeout(t, fut).await {
                Ok(r) => r,
                Err(_) => return Err(ProviderError::Timeout(t)),
            },
            None => fut.await,
        };
        res.map_err(|e| classify_host_error(e, timeout))
    }
}

#[async_trait]
impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    /// Drives `complete_stream` and returns the final response.
    async fn complete(&self, req: ModelRequest) -> Result<ModelResponse, ProviderError> {
        let mut stream = self.complete_stream(req).await?;
        while let Some(item) = stream.next().await {
            if let ModelDelta::Complete(resp) = item? {
                return Ok(resp);
            }
        }
        Err(ProviderError::InvalidResponse(
            "stream ended without a Complete delta".into(),
        ))
    }

    /// Always `stream: true` with `stream_options.include_usage`; parses SSE incrementally and
    /// ends with exactly one `Complete` (or an `Err`).
    async fn complete_stream(&self, req: ModelRequest) -> Result<DeltaStream, ProviderError> {
        let http = self.build_request(&req).await?;
        let resp = self.send(http).await?;
        if !(200..300).contains(&resp.status) {
            let body = read_bounded(resp.body, MAX_ERROR_BODY, self.endpoint.timeout).await?;
            return Err(classify_status(resp.status, &resp.headers, &body));
        }
        let state = StreamState {
            body: resp.body,
            parser: SseParser::new(),
            acc: Some(Accumulator::new(self.quirks.reasoning_field.clone())),
            pending: VecDeque::new(),
            model_id: req.model_id,
            timeout: self.endpoint.timeout,
            done: false,
        };
        Ok(Box::pin(futures_util::stream::unfold(state, step)))
    }
}

struct StreamState {
    body: std::pin::Pin<Box<dyn futures_core::Stream<Item = Result<Vec<u8>, HostError>> + Send>>,
    parser: SseParser,
    /// `None` once finished.
    acc: Option<Accumulator>,
    pending: VecDeque<ModelDelta>,
    model_id: String,
    timeout: Option<Duration>,
    done: bool,
}

type StepItem = Result<ModelDelta, ProviderError>;

async fn step(mut st: StreamState) -> Option<(StepItem, StreamState)> {
    loop {
        if let Some(d) = st.pending.pop_front() {
            return Some((Ok(d), st));
        }
        if st.done {
            return None;
        }
        let next = match st.timeout {
            Some(t) => match tokio::time::timeout(t, st.body.next()).await {
                Ok(n) => n,
                Err(_) => {
                    st.done = true;
                    return Some((Err(ProviderError::Timeout(t)), st));
                }
            },
            None => st.body.next().await,
        };
        match next {
            Some(Ok(bytes)) => {
                let events = st.parser.push(&bytes);
                if let Err(e) = handle_events(&mut st, events) {
                    st.done = true;
                    return Some((Err(e), st));
                }
            }
            Some(Err(e)) => {
                st.done = true;
                return Some((Err(classify_host_error(e, st.timeout)), st));
            }
            None => {
                // EOF without `[DONE]`: tolerated when the choice was finished (some servers omit
                // the sentinel); otherwise the stream was truncated.
                let events = st.parser.finish();
                if let Err(e) = handle_events(&mut st, events) {
                    st.done = true;
                    return Some((Err(e), st));
                }
                if st.acc.as_ref().is_some_and(Accumulator::saw_finish_reason) {
                    finalize(&mut st);
                } else if let Some(acc) = st.acc.take() {
                    st.done = true;
                    return Some((
                        Err(ProviderError::InvalidResponse(format!(
                            "stream ended without [DONE] after {} chunk(s) and no finish_reason",
                            acc.chunks()
                        ))),
                        st,
                    ));
                }
            }
        }
    }
}

fn handle_events(st: &mut StreamState, events: Vec<SseEvent>) -> Result<(), ProviderError> {
    for ev in events {
        let Some(acc) = st.acc.as_mut() else {
            // Anything after `[DONE]` is ignored.
            return Ok(());
        };
        match ev {
            SseEvent::Done => {
                finalize(st);
                return Ok(());
            }
            SseEvent::Data(payload) => {
                acc.push_raw(&payload);
                let v: Value = serde_json::from_str(&payload).map_err(|e| {
                    ProviderError::InvalidResponse(format!(
                        "malformed SSE JSON chunk: {e}: {}",
                        crate::error::error_message(payload.as_bytes())
                    ))
                })?;
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    // Mid-stream error object (LiteLLM/vLLM emit these instead of a status).
                    return Err(ProviderError::Server {
                        status: 200,
                        message: crate::error::error_message(err.to_string().as_bytes()),
                    });
                }
                st.pending.extend(acc.push_chunk(&v));
            }
        }
    }
    Ok(())
}

fn finalize(st: &mut StreamState) {
    if let Some(acc) = st.acc.take() {
        let (deltas, _) = acc.finish(&st.model_id);
        st.pending.extend(deltas);
    }
    st.done = true;
}

async fn read_bounded(
    mut body: std::pin::Pin<
        Box<dyn futures_core::Stream<Item = Result<Vec<u8>, HostError>> + Send>,
    >,
    max: usize,
    timeout: Option<Duration>,
) -> Result<Vec<u8>, ProviderError> {
    let mut out = Vec::new();
    loop {
        let next = match timeout {
            Some(t) => match tokio::time::timeout(t, body.next()).await {
                Ok(n) => n,
                Err(_) => return Err(ProviderError::Timeout(t)),
            },
            None => body.next().await,
        };
        match next {
            Some(Ok(chunk)) => {
                out.extend_from_slice(&chunk);
                if out.len() >= max {
                    out.truncate(max);
                    return Ok(out);
                }
            }
            Some(Err(e)) => return Err(classify_host_error(e, timeout)),
            None => return Ok(out),
        }
    }
}

//! Scenario runners and the structured-output probe. `run_all` produces the
//! observed quirk row for one endpoint.

use serde::Serialize;
use serde_json::{Value, json};

use crate::client::{Client, Error, Request};
use crate::quirks::{ParsedSyntax, Quirks, ReasoningField, ToolFormat, Tri};
use crate::types::{self, Completion, ToolCall};

pub fn plain_request(stream: bool) -> Request {
    Request {
        messages: vec![
            types::system("You are a terse assistant."),
            types::user("Say hello in one short sentence."),
        ],
        stream,
        max_tokens: Some(256),
        temperature: Some(0.0),
        ..Default::default()
    }
}

pub fn reasoning_request(stream: bool) -> Request {
    Request {
        messages: vec![
            types::system("You are a terse assistant. Think step by step before answering."),
            types::user("What is 17 * 23? Answer with just the number."),
        ],
        stream,
        max_tokens: Some(1024),
        temperature: Some(0.0),
        ..Default::default()
    }
}

pub fn tools_request(stream: bool) -> Request {
    Request {
        messages: vec![
            types::system("You are a terse assistant. Use the provided tools when they apply."),
            types::user("What is the weather in Oslo right now, in celsius?"),
        ],
        tools: vec![types::weather_tool()],
        stream,
        max_tokens: Some(512),
        temperature: Some(0.0),
        ..Default::default()
    }
}

/// Trivial schema used by the structured-output probe.
pub fn probe_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "answer": {"type": "string"},
            "confidence": {"type": "number"}
        },
        "required": ["answer", "confidence"],
        "additionalProperties": false
    })
}

pub fn structured_request() -> Request {
    Request {
        messages: vec![
            types::system("Answer in JSON only."),
            types::user(
                "What is the capital of Norway? Give the answer and a confidence in [0,1].",
            ),
        ],
        response_format: Some(json!({
            "type": "json_schema",
            "json_schema": {"name": "probe", "strict": true, "schema": probe_schema()}
        })),
        stream: false,
        max_tokens: Some(256),
        temperature: Some(0.0),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum StructuredOutcome {
    /// Valid JSON that matches the probe schema.
    Honored { value: Value },
    /// Endpoint rejected the request (typically 400: unsupported response_format).
    Error { status: u16, body: String },
    /// Endpoint accepted the request but the output is not schema-conformant JSON.
    Ignored { text: String },
}

/// Hand-rolled check for the trivial probe schema (no jsonschema crate: spike).
pub fn classify_structured(text: &str) -> StructuredOutcome {
    let candidate = text.trim();
    // Some models wrap JSON in ```json fences even under json_object mode.
    let candidate = candidate
        .strip_prefix("```json")
        .or_else(|| candidate.strip_prefix("```"))
        .map(|s| s.trim_end_matches("```").trim())
        .unwrap_or(candidate);
    match serde_json::from_str::<Value>(candidate) {
        Ok(v) => {
            let ok = v.is_object()
                && v["answer"].is_string()
                && v["confidence"].is_number()
                && v.as_object()
                    .unwrap()
                    .keys()
                    .all(|k| k == "answer" || k == "confidence");
            if ok {
                StructuredOutcome::Honored { value: v }
            } else {
                StructuredOutcome::Ignored {
                    text: text.to_string(),
                }
            }
        }
        Err(_) => StructuredOutcome::Ignored {
            text: text.to_string(),
        },
    }
}

pub async fn structured_probe(client: &Client) -> Result<StructuredOutcome, Error> {
    match client.complete(&structured_request()).await {
        Ok(c) => Ok(classify_structured(&c.response.text)),
        Err(Error::Http { status, body }) => Ok(StructuredOutcome::Error { status, body }),
        Err(e) => Err(e),
    }
}

/// Two-turn tool loop: ask -> tool call -> synthetic tool result -> final answer.
pub async fn tool_loop(
    client: &Client,
    stream: bool,
) -> Result<(Completion, Option<Completion>), Error> {
    let req = tools_request(stream);
    let first = client.complete(&req).await?;
    if first.response.tool_calls.is_empty() {
        return Ok((first, None));
    }
    let mut messages = req.messages.clone();
    messages.push(types::assistant_with_tool_calls(
        &first.response.text,
        &first.response.tool_calls,
    ));
    for c in &first.response.tool_calls {
        messages.push(types::tool_result(
            &c.id,
            "{\"temp_c\": 12, \"sky\": \"cloudy\"}",
        ));
    }
    let second = client
        .complete(&Request {
            messages,
            tools: req.tools.clone(),
            stream,
            max_tokens: Some(256),
            temperature: Some(0.0),
            ..Default::default()
        })
        .await?;
    Ok((first, Some(second)))
}

#[derive(Debug, Serialize)]
pub struct ScenarioResult {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub completion: Option<Completion>,
}

#[derive(Debug, Serialize)]
pub struct ProbeReport {
    pub base_url: String,
    pub model: String,
    pub configured: Quirks,
    pub observed: Quirks,
    pub structured: Option<StructuredOutcome>,
    pub strict_tool_schema_accepted: Option<bool>,
    pub scenarios: Vec<ScenarioResult>,
    pub notes: Vec<String>,
}

fn args_ok(tc: &ToolCall) -> bool {
    tc.name == "get_weather" && tc.arguments.get("city").and_then(Value::as_str).is_some()
}

/// Run `f`; on a transport error (connection refused/reset before any response bytes,
/// as seen when a server closes a keep-alive connection) retry exactly once and record
/// that it happened in `notes`. HTTP and protocol errors are not retried: they are
/// observations about the endpoint, not about the connection.
async fn with_transport_retry<T, F, Fut>(
    notes: &mut Vec<String>,
    what: &str,
    mut f: F,
) -> Result<T, Error>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    match f().await {
        Err(Error::Transport(e)) => {
            notes.push(format!(
                "{what}: transport error `{e}`; retried once on a fresh connection"
            ));
            f().await
        }
        r => r,
    }
}

/// Run every scenario and derive the observed quirk row.
pub async fn run_all(client: &Client) -> ProbeReport {
    let mut scenarios = Vec::new();
    let mut notes = Vec::new();
    let mut observed = client.endpoint.quirks.clone();
    let mut reasoning_seen: Vec<String> = Vec::new();

    // 1. plain, non-streaming
    match with_transport_retry(&mut notes, "plain", || async {
        client.complete(&plain_request(false)).await
    })
    .await
    {
        Ok(c) => {
            let ok = !c.response.text.is_empty();
            let detail = format!(
                "text={:?} usage={:?}",
                c.response.text.chars().take(60).collect::<String>(),
                c.response.usage
            );
            reasoning_seen.extend(c.observed.reasoning_fields_seen.clone());
            scenarios.push(ScenarioResult {
                name: "plain".into(),
                ok,
                detail,
                completion: Some(c),
            });
        }
        Err(e) => scenarios.push(ScenarioResult {
            name: "plain".into(),
            ok: false,
            detail: e.to_string(),
            completion: None,
        }),
    }

    // 2. plain, streaming (usage chunk)
    match with_transport_retry(&mut notes, "stream", || async {
        client.complete(&plain_request(true)).await
    })
    .await
    {
        Ok(c) => {
            let ok = !c.response.text.is_empty();
            observed.supports_stream_usage = c.observed.usage_in_final_chunk;
            if client.endpoint.quirks.supports_stream_usage && !c.observed.usage_in_final_chunk {
                notes.push(
                    "stream_options.include_usage was sent but no trailing usage chunk arrived"
                        .into(),
                );
            }
            let detail = format!(
                "chunks={} usage_in_final_chunk={} usage={:?}",
                c.observed.chunks, c.observed.usage_in_final_chunk, c.response.usage
            );
            reasoning_seen.extend(c.observed.reasoning_fields_seen.clone());
            scenarios.push(ScenarioResult {
                name: "stream".into(),
                ok,
                detail,
                completion: Some(c),
            });
        }
        Err(e) => scenarios.push(ScenarioResult {
            name: "stream".into(),
            ok: false,
            detail: e.to_string(),
            completion: None,
        }),
    }

    // 3. reasoning capture, streaming
    match with_transport_retry(&mut notes, "reasoning", || async {
        client.complete(&reasoning_request(true)).await
    })
    .await
    {
        Ok(c) => {
            let ok = c.response.thinking.is_some();
            reasoning_seen.extend(c.observed.reasoning_fields_seen.clone());
            let detail = format!(
                "fields_seen={:?} thinking_len={}",
                c.observed.reasoning_fields_seen,
                c.response.thinking.as_deref().map_or(0, str::len)
            );
            scenarios.push(ScenarioResult {
                name: "reasoning".into(),
                ok,
                detail,
                completion: Some(c),
            });
        }
        Err(e) => scenarios.push(ScenarioResult {
            name: "reasoning".into(),
            ok: false,
            detail: e.to_string(),
            completion: None,
        }),
    }

    // 4. tool loop, streaming
    match with_transport_retry(&mut notes, "tools", || tool_loop(client, true)).await {
        Ok((first, second)) => {
            let called = first.response.tool_calls.iter().any(args_ok);
            let finished = second.as_ref().is_some_and(|s| !s.response.text.is_empty());
            observed.sends_finish_reason_tool_calls =
                first.observed.finish_reason.as_deref() == Some("tool_calls");
            observed.streams_tool_call_fragments = first.observed.tool_call_delta_chunks > 1;
            if first.observed.tool_calls_parsed_from_text {
                observed.tool_format = ToolFormat::Parsed(ParsedSyntax::Hermes);
                if client.endpoint.quirks.tool_format == ToolFormat::Native {
                    notes.push("endpoint configured as native but tool calls arrived as <tool_call> text: needs parsed(hermes) or a server-side parser".into());
                }
            } else if called {
                observed.tool_format = ToolFormat::Native;
            } else {
                notes.push("no tool call produced: tool_format could not be observed".into());
            }
            reasoning_seen.extend(first.observed.reasoning_fields_seen.clone());
            let detail = format!(
                "calls={:?} finish_reason={:?} delta_chunks={} parsed_from_text={} final={:?}",
                first.response.tool_calls,
                first.observed.finish_reason,
                first.observed.tool_call_delta_chunks,
                first.observed.tool_calls_parsed_from_text,
                second
                    .as_ref()
                    .map(|s| s.response.text.chars().take(60).collect::<String>())
            );
            scenarios.push(ScenarioResult {
                name: "tools".into(),
                ok: called && finished,
                detail,
                completion: Some(first),
            });
        }
        Err(e) => scenarios.push(ScenarioResult {
            name: "tools".into(),
            ok: false,
            detail: e.to_string(),
            completion: None,
        }),
    }

    // 5. strict tool schema accepted?
    let strict_tool_schema_accepted = if client.endpoint.quirks.tool_format == ToolFormat::Native {
        let mut strict_ep = client.endpoint.clone();
        strict_ep.quirks.strict_tool_schema = true;
        match Client::new(strict_ep) {
            Ok(sc) => match sc.complete(&tools_request(false)).await {
                Ok(_) => Some(true),
                Err(Error::Http { status, body }) => {
                    notes.push(format!(
                        "strict:true rejected with HTTP {status}: {}",
                        body.chars().take(120).collect::<String>()
                    ));
                    Some(false)
                }
                Err(_) => None,
            },
            Err(_) => None,
        }
    } else {
        None
    };
    observed.strict_tool_schema = strict_tool_schema_accepted.unwrap_or(false);

    // 6. structured output. Any determinate outcome is a successful probe:
    // "not supported" is a flag value, not a failure.
    let structured =
        match with_transport_retry(&mut notes, "structured", || structured_probe(client)).await {
            Ok(o) => {
                let (flag, word) = match &o {
                    StructuredOutcome::Honored { .. } => (Tri::Yes, "honored"),
                    StructuredOutcome::Error { .. } => (Tri::No, "rejected"),
                    StructuredOutcome::Ignored { .. } => (Tri::No, "ignored"),
                };
                observed.supports_structured_output = flag;
                let detail = format!(
                    "{word}: {}",
                    format!("{o:?}").chars().take(200).collect::<String>()
                );
                scenarios.push(ScenarioResult {
                    name: "structured".into(),
                    ok: true,
                    detail,
                    completion: None,
                });
                Some(o)
            }
            Err(e) => {
                scenarios.push(ScenarioResult {
                    name: "structured".into(),
                    ok: false,
                    detail: e.to_string(),
                    completion: None,
                });
                None
            }
        };

    // Derive reasoning_field from what was actually seen.
    observed.reasoning_field = match reasoning_seen.first().map(String::as_str) {
        Some("reasoning_content") => ReasoningField::ReasoningContent,
        Some("reasoning") => ReasoningField::Reasoning,
        Some("provider_specific_fields.reasoning_content") => {
            ReasoningField::ProviderSpecificFields
        }
        Some("inline_think") => ReasoningField::InlineThink,
        _ => ReasoningField::None,
    };
    let mut uniq = reasoning_seen.clone();
    uniq.sort();
    uniq.dedup();
    if uniq.len() > 1 {
        notes.push(format!(
            "reasoning arrived in several places: {uniq:?} (take the first, ignore duplicates)"
        ));
    }

    ProbeReport {
        base_url: client.endpoint.base_url.clone(),
        model: client.endpoint.model.clone(),
        configured: client.endpoint.quirks.clone(),
        observed,
        structured,
        strict_tool_schema_accepted,
        scenarios,
        notes,
    }
}

impl ProbeReport {
    pub fn markdown(&self, label: &str) -> String {
        let mut s = String::new();
        s.push_str(&format!(
            "### {label}\n\n`{}` model=`{}`\n\n",
            self.base_url, self.model
        ));
        s.push_str("| scenario | ok | detail |\n|---|---|---|\n");
        for sc in &self.scenarios {
            s.push_str(&format!(
                "| {} | {} | {} |\n",
                sc.name,
                if sc.ok { "pass" } else { "FAIL" },
                sc.detail.replace('|', "\\|").replace('\n', " ")
            ));
        }
        s.push_str("\n| endpoint | ");
        s.push_str(&Quirks::MARKDOWN_HEADER.join(" | "));
        s.push_str(" |\n|---|");
        s.push_str(&"---|".repeat(Quirks::MARKDOWN_HEADER.len()));
        s.push('\n');
        s.push_str(&format!(
            "| {label} (configured) | {} |\n",
            self.configured.markdown_cells().join(" | ")
        ));
        s.push_str(&format!(
            "| {label} (observed) | {} |\n",
            self.observed.markdown_cells().join(" | ")
        ));
        if !self.notes.is_empty() {
            s.push_str("\nNotes:\n");
            for n in &self.notes {
                s.push_str(&format!("- {n}\n"));
            }
        }
        s
    }

    pub fn all_ok(&self) -> bool {
        self.scenarios.iter().all(|s| s.ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_classifier() {
        assert!(matches!(
            classify_structured("{\"answer\":\"Oslo\",\"confidence\":1}"),
            StructuredOutcome::Honored { .. }
        ));
        assert!(matches!(
            classify_structured("```json\n{\"answer\":\"Oslo\",\"confidence\":0.5}\n```"),
            StructuredOutcome::Honored { .. }
        ));
        assert!(matches!(
            classify_structured("{\"answer\":\"Oslo\"}"),
            StructuredOutcome::Ignored { .. }
        ));
        assert!(matches!(
            classify_structured("Oslo, I think."),
            StructuredOutcome::Ignored { .. }
        ));
        assert!(matches!(
            classify_structured("{\"answer\":\"Oslo\",\"confidence\":1,\"extra\":true}"),
            StructuredOutcome::Ignored { .. }
        ));
    }
}

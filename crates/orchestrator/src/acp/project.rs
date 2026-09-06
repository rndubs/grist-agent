//! The projection of the event log onto ACP `session/update` (ADR-0004, "Event mapping"). Pure:
//! `updates_for` maps one logged `Event` to zero or more updates; `delta_update` maps one
//! streaming delta. The log stays the source of truth (every update here is derived from an
//! event or a delta the kernel also logs), and the twelve log-facing kinds with no ACP
//! counterpart reach a client only through `_grist/event`.

use agent_client_protocol::schema::v1::{
    Content, ContentBlock as AcpBlock, ContentChunk, SessionUpdate, TextContent, ToolCall,
    ToolCallContent, ToolCallStatus, ToolCallUpdate, ToolCallUpdateFields, ToolKind,
};
use kernel::{ContentBlock, Event, EventBody, ModelDelta, ToolResultContent};

fn text(s: &str) -> AcpBlock {
    AcpBlock::Text(TextContent::new(s))
}

/// The ACP tool kind for one of our tool names.
pub fn tool_kind(name: &str) -> ToolKind {
    match name {
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "bash" | "run_script" | "python" => ToolKind::Execute,
        "ask_user" => ToolKind::Other,
        _ => ToolKind::Other,
    }
}

/// Human title for a tool call.
fn title(name: &str, input: &serde_json::Value) -> String {
    let arg = ["path", "command", "code", "question"]
        .iter()
        .find_map(|k| input.get(k).and_then(|v| v.as_str()))
        .map(|s| {
            s.lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(80)
                .collect::<String>()
        });
    match arg {
        Some(a) if !a.is_empty() => format!("{name}: {a}"),
        _ => name.to_owned(),
    }
}

/// Updates for one logged event. `include_text` is false when the response text was already
/// streamed as deltas this turn (see [`Projector`]).
pub fn updates_for(event: &Event, include_text: bool) -> Vec<SessionUpdate> {
    let mut out = Vec::new();
    match &event.body {
        EventBody::UserMessage(p) => {
            for b in &p.content {
                if let ContentBlock::Text { text: t } = b {
                    out.push(SessionUpdate::UserMessageChunk(ContentChunk::new(text(t))));
                }
            }
        }
        EventBody::ModelResponse(p) => {
            for b in &p.content {
                match b {
                    ContentBlock::Text { text: t } if include_text => {
                        out.push(SessionUpdate::AgentMessageChunk(ContentChunk::new(text(t))));
                    }
                    ContentBlock::Thinking { text: t, .. } if include_text => {
                        out.push(SessionUpdate::AgentThoughtChunk(ContentChunk::new(text(t))));
                    }
                    _ => {}
                }
            }
        }
        EventBody::ToolCall(p) => {
            let mut call = ToolCall::new(p.tool_use_id.clone(), title(&p.name, &p.input));
            call.kind = tool_kind(&p.name);
            call.status = ToolCallStatus::InProgress;
            call.raw_input = Some(p.input.clone());
            out.push(SessionUpdate::ToolCall(call));
        }
        EventBody::ToolResult(p) => {
            let (raw, shown) = match &p.content {
                ToolResultContent::Json(v) => (v.clone(), None),
                ToolResultContent::Blocks(blocks) => {
                    let joined: Vec<String> = blocks
                        .iter()
                        .filter_map(|b| match b {
                            ContentBlock::Text { text: t } => Some(t.clone()),
                            _ => None,
                        })
                        .collect();
                    (
                        serde_json::to_value(blocks).unwrap_or(serde_json::Value::Null),
                        (!joined.is_empty()).then(|| joined.join("\n")),
                    )
                }
            };
            let mut fields = ToolCallUpdateFields::new()
                .status(if p.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                })
                .raw_output(raw);
            if let Some(s) = shown {
                fields = fields.content(vec![ToolCallContent::Content(Content::new(text(&s)))]);
            }
            out.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                p.tool_use_id.clone(),
                fields,
            )));
        }
        EventBody::TaskUpdate(p) => {
            // A task's completion is the moment its tool call is really done.
            if p.status.is_terminal() {
                let fields = ToolCallUpdateFields::new()
                    .status(if p.outcome.as_ref().is_some_and(|o| o.is_error) {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .raw_output(serde_json::to_value(p).unwrap_or(serde_json::Value::Null));
                out.push(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                    format!("task:{}", p.task_id.0),
                    fields,
                )));
            }
        }
        _ => {}
    }
    out
}

/// The update for one streaming delta, if it has a chat-facing form.
pub fn delta_update(delta: &ModelDelta) -> Option<SessionUpdate> {
    match delta {
        ModelDelta::TextDelta { text: t, .. } => {
            Some(SessionUpdate::AgentMessageChunk(ContentChunk::new(text(t))))
        }
        ModelDelta::ThinkingDelta { text: t, .. } => {
            Some(SessionUpdate::AgentThoughtChunk(ContentChunk::new(text(t))))
        }
        _ => None,
    }
}

/// Per-session projection state: whether any text/thinking delta arrived since the last
/// `model_request`, so the `model_response` event does not repeat what was streamed.
#[derive(Debug, Default)]
pub struct Projector {
    streamed: bool,
}

impl Projector {
    /// Updates for `event`, tracking the streamed flag.
    pub fn event(&mut self, event: &Event) -> Vec<SessionUpdate> {
        if matches!(event.body, EventBody::ModelRequest(_)) {
            self.streamed = false;
        }
        let out = updates_for(event, !self.streamed);
        if matches!(event.body, EventBody::ModelResponse(_)) {
            self.streamed = false;
        }
        out
    }

    /// The update for `delta`, tracking the streamed flag.
    pub fn delta(&mut self, delta: &ModelDelta) -> Option<SessionUpdate> {
        let u = delta_update(delta);
        if u.is_some() {
            self.streamed = true;
        }
        u
    }
}

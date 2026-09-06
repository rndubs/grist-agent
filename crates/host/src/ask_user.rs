//! The `ask_user` tool (D17, §7.10): capabilities `[]`, calls `Host::ask_user` through the
//! `ToolContext`. The kernel recognizes the reserved name `ask_user` and logs
//! `ask_user{question_id, question, options}` before and `user_answer{question_id, answer}` after
//! the call, reading exactly the input and output fields defined here.

use async_trait::async_trait;
use kernel::{
    AskUserRequest, Capability, HostError, Tool, ToolContext, ToolError, ToolKind, ToolResult,
};
use serde_json::{Value, json};

/// The reserved tool name.
pub const ASK_USER_TOOL_NAME: &str = "ask_user";

/// Asks the user a question and returns the answer as the tool result.
///
/// Input: `{question: string, options?: [string], allow_free_text?: bool (default true)}`.
/// Output: `{question_id: string, answer: string | null, declined: bool}`.
#[derive(Clone, Copy, Debug, Default)]
pub struct AskUserTool;

impl AskUserTool {
    /// The tool.
    pub fn new() -> AskUserTool {
        AskUserTool
    }
}

fn map_host_err(e: HostError) -> ToolError {
    match e {
        HostError::Cancelled => ToolError::Cancelled,
        HostError::Denied(p) => ToolError::Denied(p.to_string()),
        other => ToolError::Failed(other.to_string()),
    }
}

fn parse_input(input: &Value) -> Result<(String, Vec<String>, bool), ToolError> {
    let obj = input
        .as_object()
        .ok_or_else(|| ToolError::InvalidInput("input must be an object".to_owned()))?;
    let question = obj
        .get("question")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::InvalidInput("`question` must be a string".to_owned()))?
        .to_owned();
    if question.trim().is_empty() {
        return Err(ToolError::InvalidInput("`question` is empty".to_owned()));
    }
    let options = match obj.get("options") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| {
                v.as_str().map(str::to_owned).ok_or_else(|| {
                    ToolError::InvalidInput("`options` must be an array of strings".to_owned())
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => {
            return Err(ToolError::InvalidInput(
                "`options` must be an array of strings".to_owned(),
            ));
        }
    };
    let allow_free_text = match obj.get("allow_free_text") {
        None | Some(Value::Null) => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => {
            return Err(ToolError::InvalidInput(
                "`allow_free_text` must be a boolean".to_owned(),
            ));
        }
    };
    if !allow_free_text && options.is_empty() {
        return Err(ToolError::InvalidInput(
            "`allow_free_text: false` requires at least one option".to_owned(),
        ));
    }
    Ok((question, options, allow_free_text))
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        ASK_USER_TOOL_NAME
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for the answer. Use it when the task is ambiguous or \
         needs a decision only the user can make; never to ask for permission. Provide `options` \
         for a fixed choice; the answer is `null` when the user declines."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to ask."
                },
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional fixed choices."
                },
                "allow_free_text": {
                    "type": "boolean",
                    "default": true,
                    "description": "Whether a free-text answer is accepted alongside `options`."
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Stateless
    }

    fn capabilities(&self) -> Vec<Capability> {
        Vec::new()
    }

    async fn invoke(&self, ctx: &ToolContext<'_>, input: Value) -> Result<ToolResult, ToolError> {
        let (question, options, allow_free_text) = parse_input(&input)?;
        let req = AskUserRequest {
            question_id: format!("q{}-{}", ctx.turn, ctx.tool_use_id),
            question,
            options,
            allow_free_text,
        };
        let answer = ctx.host.ask_user(req).await.map_err(map_host_err)?;
        Ok(ToolResult::Value(json!({
            "question_id": answer.question_id,
            "declined": answer.answer.is_none(),
            "answer": answer.answer,
        })))
    }
}

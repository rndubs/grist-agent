//! The `parsed(hermes)` normalizer and inline `<think>` extraction: what the
//! D7 "tool-call parser middleware" would do for servers without a parser.

use serde_json::Value;

use crate::quirks::ParsedSyntax;
use crate::types::{ToolCall, ToolDef};

/// System prompt block advertising the tools in the model's expected syntax.
/// Mirrors the Hermes-2-Pro / Qwen2.5 chat-template wording.
pub fn system_block(syntax: ParsedSyntax, tools: &[ToolDef]) -> String {
    match syntax {
        ParsedSyntax::Hermes => {
            let mut s = String::from(
                "# Tools\n\nYou may call one or more functions to assist with the user query.\n\n\
                 You are provided with function signatures within <tools></tools> XML tags:\n<tools>\n",
            );
            for t in tools {
                let f = serde_json::json!({
                    "type": "function",
                    "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
                });
                s.push_str(&f.to_string());
                s.push('\n');
            }
            s.push_str(
                "</tools>\n\nFor each function call, return a json object with function name and arguments \
                 within <tool_call></tool_call> XML tags:\n<tool_call>\n{\"name\": <function-name>, \
                 \"arguments\": <args-json-object>}\n</tool_call>",
            );
            s
        }
    }
}

/// Extract `<think>...</think>` from the front of the text. Also tolerates a
/// missing opening tag (Qwen3 templates sometimes pre-fill `<think>` so the
/// model only emits the close tag).
pub fn extract_think(text: &str) -> (Option<String>, String) {
    let Some(close) = text.find("</think>") else {
        return (None, text.to_string());
    };
    let head = &text[..close];
    let think = match head.find("<think>") {
        Some(open) => &head[open + "<think>".len()..],
        None => head,
    };
    let rest = &text[close + "</think>".len()..];
    (Some(think.trim().to_string()), rest.to_string())
}

/// Extract every `<tool_call>{json}</tool_call>` block. Returns the calls and
/// the text with the blocks removed. `first_index` seeds synthesized ids.
pub fn extract_tool_calls(text: &str, first_index: usize) -> (Vec<ToolCall>, String) {
    let mut calls = Vec::new();
    let mut rest = String::new();
    let mut cursor = text;
    loop {
        let Some(open) = cursor.find("<tool_call>") else {
            rest.push_str(cursor);
            break;
        };
        rest.push_str(&cursor[..open]);
        let after = &cursor[open + "<tool_call>".len()..];
        // Unterminated block (truncated output): keep the text as-is.
        let Some(close) = after.find("</tool_call>") else {
            rest.push_str(&cursor[open..]);
            break;
        };
        let inner = after[..close].trim();
        if let Ok(v) = serde_json::from_str::<Value>(inner) {
            let name = v["name"].as_str().unwrap_or("").to_string();
            let arguments = match &v["arguments"] {
                // Some models double-encode the arguments as a string.
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
                Value::Null => v.get("parameters").cloned().unwrap_or(Value::Null),
                other => other.clone(),
            };
            if !name.is_empty() {
                calls.push(ToolCall {
                    id: format!("call_{}", first_index + calls.len()),
                    name,
                    arguments,
                });
            } else {
                rest.push_str(
                    &cursor[open..open + "<tool_call>".len() + close + "</tool_call>".len()],
                );
            }
        } else {
            rest.push_str(&cursor[open..open + "<tool_call>".len() + close + "</tool_call>".len()]);
        }
        cursor = &after[close + "</tool_call>".len()..];
    }
    (calls, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn think_with_and_without_open_tag() {
        assert_eq!(
            extract_think("<think>\nhmm\n</think>\nhi"),
            (Some("hmm".into()), "\nhi".into())
        );
        assert_eq!(
            extract_think("hmm</think>hi"),
            (Some("hmm".into()), "hi".into())
        );
        assert_eq!(extract_think("plain"), (None, "plain".into()));
    }

    #[test]
    fn hermes_blocks_are_extracted_and_ids_synthesized() {
        let t = "Let me check.\n<tool_call>\n{\"name\": \"a\", \"arguments\": {\"x\": 1}}\n</tool_call>\n\
                 <tool_call>{\"name\":\"b\",\"arguments\":\"{\\\"y\\\":2}\"}</tool_call>";
        let (calls, rest) = extract_tool_calls(t, 0);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_0");
        assert_eq!(calls[0].arguments, serde_json::json!({"x": 1}));
        assert_eq!(calls[1].id, "call_1");
        assert_eq!(calls[1].arguments, serde_json::json!({"y": 2}));
        assert_eq!(rest.trim(), "Let me check.");
    }

    #[test]
    fn unterminated_block_is_left_alone() {
        let (calls, rest) = extract_tool_calls("x <tool_call>{\"name\":", 0);
        assert!(calls.is_empty());
        assert_eq!(rest, "x <tool_call>{\"name\":");
    }
}

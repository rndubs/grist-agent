//! Result spill (§7.4, D12): runs in the kernel after invoke and ingress redaction, before `after_tool`.

use serde_json::{Value, json};

use crate::artifact::{ArtifactStore, Spilled};
use crate::config::SpillConfig;
use crate::content::{ContentBlock, ToolResultContent};
use crate::event::{SpillRef, WarningPayload};
use crate::hash::canonical_json_value;
use crate::tool::ToolOutput;

/// MIME of a spilled `Json` unit.
pub const JSON_MIME: &str = "application/json";
/// MIME of a spilled `Text` block.
pub const TEXT_MIME: &str = "text/plain; charset=utf-8";

/// The first `n` bytes of `s`, cut back to a UTF-8 character boundary.
pub fn head_utf8(s: &str, n: u64) -> &str {
    let mut end = usize::try_from(n).unwrap_or(usize::MAX).min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// The last `n` bytes of `s`, cut forward to a UTF-8 character boundary.
pub fn tail_utf8(s: &str, n: u64) -> &str {
    let n = usize::try_from(n).unwrap_or(usize::MAX).min(s.len());
    let mut start = s.len() - n;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// What spilling one unit produced.
enum Unit {
    Stored(Spilled),
    StoreFailed {
        head: String,
        tail: String,
        size: u64,
        mime: String,
        error: String,
    },
}

async fn spill_unit(text: &str, mime: &str, cfg: &SpillConfig, store: &dyn ArtifactStore) -> Unit {
    let head = head_utf8(text, cfg.head_bytes).to_owned();
    let tail = tail_utf8(text, cfg.tail_bytes).to_owned();
    let size = text.len() as u64;
    match store.put(text.as_bytes(), mime).await {
        Ok(handle) => Unit::Stored(Spilled {
            handle,
            head,
            tail,
            size,
            mime: mime.to_owned(),
        }),
        Err(e) => Unit::StoreFailed {
            head,
            tail,
            size,
            mime: mime.to_owned(),
            error: e.to_string(),
        },
    }
}

/// Spill every over-cap unit of `out` (§7.4). Returns the `tool_result.spill` reference (the first
/// stored unit) and any warnings to log. `Task` results never spill.
pub(super) async fn apply(
    out: &mut ToolOutput,
    cfg: &SpillConfig,
    store: &dyn ArtifactStore,
    turn: u64,
) -> (Option<SpillRef>, Vec<WarningPayload>) {
    let mut warnings = Vec::new();
    let mut spill_ref = None;
    if out.task.is_some() {
        return (None, warnings);
    }
    let mut units: Vec<(usize, String, &'static str)> = Vec::new();
    match &out.content {
        ToolResultContent::Json(v) => {
            if let Ok(bytes) = canonical_json_value(v)
                && bytes.len() as u64 > cfg.cap_bytes
            {
                // Canonical bytes of a JSON value are valid UTF-8.
                let text = String::from_utf8(bytes).unwrap_or_default();
                units.push((0, text, JSON_MIME));
            }
        }
        ToolResultContent::Blocks(blocks) => {
            for (i, b) in blocks.iter().enumerate() {
                if let ContentBlock::Text { text } = b
                    && text.len() as u64 > cfg.cap_bytes
                {
                    units.push((i, text.clone(), TEXT_MIME));
                }
            }
        }
    }
    for (index, text, mime) in units {
        let unit = spill_unit(&text, mime, cfg, store).await;
        let replacement: Value = match unit {
            Unit::Stored(spilled) => {
                if spill_ref.is_none() {
                    spill_ref = Some(SpillRef {
                        handle: spilled.handle.clone(),
                        size: spilled.size,
                        mime: spilled.mime.clone(),
                    });
                }
                out.artifact_handles.push(spilled.handle.clone());
                serde_json::to_value(&spilled).unwrap_or(Value::Null)
            }
            Unit::StoreFailed {
                head,
                tail,
                size,
                mime,
                error,
            } => {
                out.is_error = true;
                warnings.push(WarningPayload::kernel(
                    turn,
                    "spill_store_failed",
                    format!("artifact store refused a spilled result: {error}"),
                    Some(json!({ "size": size, "mime": mime })),
                ));
                json!({
                    "error": "spill store failed; oversized result was not stored",
                    "head": head, "tail": tail, "size": size, "mime": mime
                })
            }
        };
        out.spilled = true;
        match &mut out.content {
            ToolResultContent::Json(v) => *v = replacement,
            ToolResultContent::Blocks(blocks) => {
                blocks[index] = ContentBlock::Text {
                    text: replacement.to_string(),
                };
            }
        }
    }
    (spill_ref, warnings)
}

//! HTTP status and transport failures → `kernel::ProviderError` (`kernel-interface.md` §3.8, §7.2).
//!
//! The provider only classifies; the kernel decides what to retry. Nothing here ever sees the
//! resolved API key, so no message can leak it.

use std::time::Duration;

use kernel::{HostError, ProviderError};
use serde_json::Value;

/// Map a non-2xx response to a `ProviderError`. `body` is the (bounded) error body.
pub fn classify_status(status: u16, headers: &[(String, String)], body: &[u8]) -> ProviderError {
    let message = error_message(body);
    match status {
        429 => ProviderError::RateLimited {
            retry_after: retry_after(headers),
        },
        401 | 403 => ProviderError::Auth(format!("HTTP {status}: {message}")),
        400 if looks_like_context_overflow(&message) => ProviderError::ContextTooLong(message),
        500..=599 => ProviderError::Server { status, message },
        _ => ProviderError::Client { status, message },
    }
}

/// Map a `HostError` from `NetHandle::send` or the body stream. Timeouts reported by the host
/// become `Timeout`; everything else is transport-level.
pub fn classify_host_error(e: HostError, timeout: Option<Duration>) -> ProviderError {
    match e {
        HostError::Cancelled => ProviderError::Cancelled,
        HostError::Net(msg) | HostError::Io(msg) if mentions_timeout(&msg) => {
            ProviderError::Timeout(timeout.unwrap_or(Duration::ZERO))
        }
        other => ProviderError::Transport(other.to_string()),
    }
}

fn mentions_timeout(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("timed out") || m.contains("timeout")
}

/// `Retry-After` in seconds. HTTP-date forms are not parsed (`None`).
pub fn retry_after(headers: &[(String, String)]) -> Option<Duration> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Best-effort extraction of the human-readable message from an OpenAI-style error body
/// (`{"error": {"message": ...}}`), falling back to the truncated raw text.
pub fn error_message(body: &[u8]) -> String {
    const MAX: usize = 2048;
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if let Ok(v) = serde_json::from_str::<Value>(text) {
        let candidates = [
            v.get("error").and_then(|e| e.get("message")),
            v.get("error").filter(|e| e.is_string()),
            v.get("message"),
            v.get("detail"),
        ];
        for c in candidates.into_iter().flatten() {
            let s = match c {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            if !s.is_empty() {
                return truncate(&s, MAX);
            }
        }
    }
    truncate(text, MAX)
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Heuristics for a 400 that really means "the prompt does not fit".
pub fn looks_like_context_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "context length",
        "context_length",
        "context window",
        "maximum context",
        "max_tokens",
        "max tokens",
        "too many tokens",
        "prompt is too long",
        "reduce the length",
        "exceeds the limit",
        "input is too long",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    #[test]
    fn status_mapping() {
        let e = classify_status(429, &[h("Retry-After", "7")], b"{}");
        assert!(matches!(
            e,
            ProviderError::RateLimited {
                retry_after: Some(d)
            } if d == Duration::from_secs(7)
        ));
        let e = classify_status(
            429,
            &[h("retry-after", "Wed, 21 Oct 2015 07:28:00 GMT")],
            b"",
        );
        assert!(matches!(
            e,
            ProviderError::RateLimited { retry_after: None }
        ));
        assert!(matches!(
            classify_status(503, &[], b"upstream down"),
            ProviderError::Server { status: 503, .. }
        ));
        assert!(matches!(
            classify_status(401, &[], b"{\"error\":{\"message\":\"bad key\"}}"),
            ProviderError::Auth(m) if m.contains("bad key")
        ));
        assert!(matches!(
            classify_status(403, &[], b""),
            ProviderError::Auth(_)
        ));
        let body = br#"{"error":{"message":"This model's maximum context length is 4096 tokens.","type":"invalid_request_error"}}"#;
        assert!(matches!(
            classify_status(400, &[], body),
            ProviderError::ContextTooLong(m) if m.contains("4096")
        ));
        assert!(matches!(
            classify_status(400, &[], b"{\"error\":{\"message\":\"bad json\"}}"),
            ProviderError::Client { status: 400, .. }
        ));
        assert!(matches!(
            classify_status(404, &[], b"nope"),
            ProviderError::Client { status: 404, message } if message == "nope"
        ));
    }

    #[test]
    fn error_message_extraction_and_truncation() {
        assert_eq!(
            error_message(br#"{"error":"plain string"}"#),
            "plain string"
        );
        assert_eq!(error_message(br#"{"detail":"Not Found"}"#), "Not Found");
        assert_eq!(error_message(b"  raw text \n"), "raw text");
        let long = "x".repeat(5000);
        assert!(error_message(long.as_bytes()).len() < 2100);
    }

    #[test]
    fn host_error_mapping() {
        let t = Some(Duration::from_secs(3));
        assert!(matches!(
            classify_host_error(HostError::Net("connection timed out".into()), t),
            ProviderError::Timeout(d) if d == Duration::from_secs(3)
        ));
        assert!(matches!(
            classify_host_error(HostError::Net("connection reset".into()), t),
            ProviderError::Transport(_)
        ));
        assert!(matches!(
            classify_host_error(HostError::Cancelled, t),
            ProviderError::Cancelled
        ));
    }
}

//! Incremental Server-Sent-Events parser for the `/v1/chat/completions` stream.
//!
//! Bytes are buffered until a full line is available, so chunk boundaries may fall
//! mid-line or mid-UTF-8 sequence. Only `data:` lines matter; `event:`, `id:`, `retry:`
//! and comment lines are ignored. An event is dispatched on a blank line (or at EOF via
//! [`SseParser::finish`]); multi-line `data:` payloads are joined with `\n` per the spec.

/// One dispatched SSE event's data payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseEvent {
    /// The literal `[DONE]` sentinel.
    Done,
    /// Any other payload (expected to be a JSON chunk).
    Data(String),
}

/// Incremental parser; feed bytes with [`SseParser::push`].
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
    data_lines: Vec<String>,
}

impl SseParser {
    /// A fresh parser.
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes and return every event completed by them, in order.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.take_line(&line, &mut out);
        }
        out
    }

    /// Flush at end of stream: a trailing line without `\n` and a pending event.
    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut out = Vec::new();
        if !self.buf.is_empty() {
            let line = std::mem::take(&mut self.buf);
            self.take_line(&line, &mut out);
        }
        self.dispatch(&mut out);
        out
    }

    fn take_line(&mut self, raw: &[u8], out: &mut Vec<SseEvent>) {
        let mut line = String::from_utf8_lossy(raw).into_owned();
        while line.ends_with('\n') || line.ends_with('\r') {
            line.pop();
        }
        if line.is_empty() {
            self.dispatch(out);
            return;
        }
        if line.starts_with(':') {
            return;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            self.data_lines
                .push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }

    fn dispatch(&mut self, out: &mut Vec<SseEvent>) {
        if self.data_lines.is_empty() {
            return;
        }
        let payload = std::mem::take(&mut self.data_lines).join("\n");
        if payload.trim() == "[DONE]" {
            out.push(SseEvent::Done);
        } else {
            out.push(SseEvent::Data(payload));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_events_on_blank_lines_and_handles_crlf() {
        let mut p = SseParser::new();
        let ev = p.push(b"data: {\"a\":1}\r\n\r\ndata: {\"b\":2}\n\ndata: [DONE]\n\n");
        assert_eq!(
            ev,
            vec![
                SseEvent::Data("{\"a\":1}".into()),
                SseEvent::Data("{\"b\":2}".into()),
                SseEvent::Done
            ]
        );
    }

    #[test]
    fn tolerates_chunk_boundaries_mid_line_and_mid_utf8() {
        let full = "data: {\"t\":\"héllo ✓\"}\n\n".as_bytes();
        // Split inside "é" (2 bytes) and inside "✓" (3 bytes) and inside the prefix.
        let mut p = SseParser::new();
        let mut events = Vec::new();
        // Byte offsets: 'é' occupies 13..15 and '✓' 19..22.
        let cuts = [3usize, 14, 15, 20, 21, full.len()];
        let mut start = 0;
        for cut in cuts {
            events.extend(p.push(&full[start..cut]));
            start = cut;
        }
        assert_eq!(events, vec![SseEvent::Data("{\"t\":\"héllo ✓\"}".into())]);
    }

    #[test]
    fn ignores_comments_and_other_fields_and_joins_multiline_data() {
        let mut p = SseParser::new();
        let ev = p.push(b": keepalive\nevent: x\nid: 7\ndata: one\ndata: two\n\n");
        assert_eq!(ev, vec![SseEvent::Data("one\ntwo".into())]);
    }

    #[test]
    fn finish_flushes_a_trailing_event_without_newline() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"x\":1}").is_empty());
        assert_eq!(p.finish(), vec![SseEvent::Data("{\"x\":1}".into())]);
        assert!(p.finish().is_empty());
    }
}

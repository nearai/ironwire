//! Read-only SSE observation.
//!
//! The forward path streams bytes verbatim. This module reads a *copy* to learn
//! usage and served-model, and it is built around one rule: **the observer must
//! never be able to stall or fail the forward path** (`docs/PROTOCOL.md` §2).
//! Every failure mode here — an oversized frame, malformed JSON, an event shape
//! we do not know — degrades to "we learned less", never to a broken stream.

use crate::observe::{Observation, UsageReading, anthropic_usage, openai_usage};

/// Largest single SSE frame we will buffer while observing.
///
/// Frames above this are forwarded normally and simply not parsed. Anthropic's
/// largest frames (`message_start` with a big usage block) are a few KB; a
/// megabyte means something unusual is happening and holding it in the observer
/// would be the wrong response.
const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Which stream dialect to interpret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// Anthropic Messages SSE.
    Anthropic,
    /// OpenAI Responses SSE.
    OpenAiResponses,
    /// OpenAI Chat Completions SSE.
    OpenAiChat,
}

/// Incremental observer over an SSE byte stream.
#[derive(Debug)]
pub struct SseObserver {
    dialect: Dialect,
    buffer: Vec<u8>,
    observation: Observation,
    /// Set when a frame exceeded the buffer cap, so callers can distinguish
    /// "nothing to report" from "we stopped looking".
    truncated: bool,
}

impl SseObserver {
    /// New observer for a dialect.
    #[must_use]
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            buffer: Vec::new(),
            observation: Observation::default(),
            truncated: false,
        }
    }

    /// Feed a chunk. Infallible by construction.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.buffer.len().saturating_add(chunk.len()) > MAX_FRAME_BYTES {
            // Drop what we have and resynchronise at the next frame boundary
            // rather than growing without bound.
            self.truncated = true;
            self.buffer.clear();
            if let Some(pos) = find_boundary(chunk) {
                self.buffer.extend_from_slice(&chunk[pos..]);
            }
            return;
        }
        self.buffer.extend_from_slice(chunk);
        while let Some(pos) = find_boundary(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..pos).collect();
            self.consume_frame(&frame);
        }
    }

    /// Finish and return what was learned.
    #[must_use]
    pub fn finish(mut self) -> Observation {
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            self.consume_frame(&frame);
        }
        self.observation
    }

    /// Whether a frame was skipped for size.
    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    fn consume_frame(&mut self, frame: &[u8]) {
        let Ok(text) = std::str::from_utf8(frame) else {
            return;
        };
        // An SSE frame may carry multiple `data:` lines that concatenate.
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) else {
            return;
        };
        match self.dialect {
            Dialect::Anthropic => self.consume_anthropic(&value),
            Dialect::OpenAiResponses => self.consume_openai_responses(&value),
            Dialect::OpenAiChat => self.consume_openai_chat(&value),
        }
    }

    fn consume_anthropic(&mut self, value: &serde_json::Value) {
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                if let Some(model) = value
                    .pointer("/message/model")
                    .and_then(serde_json::Value::as_str)
                {
                    self.observation.served_model = Some(model.to_string());
                }
                if let Some(usage) = value.pointer("/message/usage").and_then(anthropic_usage) {
                    self.merge_usage(usage);
                }
            }
            Some("message_delta") => {
                if let Some(usage) = value.get("usage").and_then(anthropic_usage) {
                    self.merge_usage(usage);
                }
            }
            _ => {}
        }
    }

    fn consume_openai_responses(&mut self, value: &serde_json::Value) {
        if let Some(model) = value
            .pointer("/response/model")
            .and_then(serde_json::Value::as_str)
        {
            self.observation.served_model = Some(model.to_string());
        }
        if let Some(usage) = value.pointer("/response/usage").and_then(openai_usage) {
            self.merge_usage(usage);
        }
    }

    fn consume_openai_chat(&mut self, value: &serde_json::Value) {
        if let Some(model) = value.get("model").and_then(serde_json::Value::as_str) {
            self.observation.served_model = Some(model.to_string());
        }
        if let Some(usage) = value.get("usage").and_then(openai_usage) {
            self.merge_usage(usage);
        }
    }

    fn merge_usage(&mut self, usage: UsageReading) {
        self.observation
            .usage
            .get_or_insert_with(UsageReading::default)
            .merge(usage);
    }
}

/// Byte offset just past the next frame boundary (`\n\n` or `\r\n\r\n`).
fn find_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|p| p + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ANTHROPIC_STREAM: &str = concat!(
        "event: message_start\n",
        r#"data: {"type":"message_start","message":{"model":"claude-opus-4-6","usage":{"input_tokens":12,"cache_read_input_tokens":98000,"output_tokens":1}}}"#,
        "\n\n",
        "event: content_block_delta\n",
        r#"data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","usage":{"output_tokens":40}}"#,
        "\n\n",
        "event: message_delta\n",
        r#"data: {"type":"message_delta","usage":{"output_tokens":137}}"#,
        "\n\n",
        "event: message_stop\n",
        r#"data: {"type":"message_stop"}"#,
        "\n\n",
    );

    #[test]
    fn reads_usage_and_model_from_an_anthropic_stream() {
        let mut observer = SseObserver::new(Dialect::Anthropic);
        observer.push(ANTHROPIC_STREAM.as_bytes());
        let obs = observer.finish();
        assert_eq!(obs.served_model.as_deref(), Some("claude-opus-4-6"));
        let usage = obs.usage.expect("usage observed");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_tokens, 98_000);
        assert_eq!(usage.output_tokens, 137, "cumulative deltas must not sum");
    }

    #[test]
    fn byte_by_byte_delivery_yields_the_same_result() {
        // Real streams split frames arbitrarily; the parser must not depend on
        // chunk boundaries lining up with frames.
        let mut observer = SseObserver::new(Dialect::Anthropic);
        for byte in ANTHROPIC_STREAM.as_bytes() {
            observer.push(&[*byte]);
        }
        let usage = observer.finish().usage.expect("usage observed");
        assert_eq!(usage.output_tokens, 137);
        assert_eq!(usage.input_tokens, 12);
    }

    #[test]
    fn handles_crlf_framing() {
        let stream = ANTHROPIC_STREAM.replace('\n', "\r\n");
        let mut observer = SseObserver::new(Dialect::Anthropic);
        observer.push(stream.as_bytes());
        assert_eq!(
            observer
                .finish()
                .usage
                .expect("usage observed")
                .output_tokens,
            137
        );
    }

    #[test]
    fn malformed_frames_reduce_knowledge_rather_than_breaking() {
        let mut observer = SseObserver::new(Dialect::Anthropic);
        observer.push(b"data: {not json at all\n\n");
        observer.push(b"event: ping\n\n");
        observer.push(b"data: [DONE]\n\n");
        observer.push(&[0xff, 0xfe, b'\n', b'\n']); // invalid UTF-8
        let obs = observer.finish();
        assert!(obs.is_empty(), "garbage must not synthesise an observation");
    }

    #[test]
    fn an_oversized_frame_is_skipped_and_the_stream_resynchronises() {
        let mut observer = SseObserver::new(Dialect::Anthropic);
        let giant = vec![b'x'; MAX_FRAME_BYTES + 1];
        observer.push(&giant);
        assert!(observer.truncated());
        // The stream keeps working afterwards.
        observer.push(b"\n\n");
        observer.push(ANTHROPIC_STREAM.as_bytes());
        let obs = observer.finish();
        assert_eq!(obs.served_model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn reads_usage_from_an_openai_responses_stream() {
        let stream = concat!(
            "event: response.created\n",
            r#"data: {"type":"response.created","response":{"model":"gpt-5.6"}}"#,
            "\n\n",
            "event: response.completed\n",
            r#"data: {"type":"response.completed","response":{"model":"gpt-5.6","usage":{"input_tokens":900,"input_tokens_details":{"cached_tokens":800},"output_tokens":50}}}"#,
            "\n\n",
        );
        let mut observer = SseObserver::new(Dialect::OpenAiResponses);
        observer.push(stream.as_bytes());
        let obs = observer.finish();
        assert_eq!(obs.served_model.as_deref(), Some("gpt-5.6"));
        let usage = obs.usage.expect("usage observed");
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.input_tokens, 100);
    }

    #[test]
    fn a_stream_that_ends_without_a_trailing_boundary_is_still_read() {
        let mut observer = SseObserver::new(Dialect::Anthropic);
        observer.push(br#"data: {"type":"message_start","message":{"model":"claude-opus-4-6"}}"#);
        assert_eq!(
            observer.finish().served_model.as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn multi_line_data_fields_concatenate() {
        let mut observer = SseObserver::new(Dialect::Anthropic);
        observer
            .push(b"data: {\"type\":\"message_start\",\ndata: \"message\":{\"model\":\"m\"}}\n\n");
        assert_eq!(observer.finish().served_model.as_deref(), Some("m"));
    }
}

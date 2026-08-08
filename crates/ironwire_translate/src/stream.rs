//! Streaming translation: Chat Completions SSE in, Anthropic Messages SSE out.
//!
//! Claude Code always streams, so a translated lane that only handled the
//! non-streaming shape would be a translated lane that never runs.
//!
//! Text is forwarded incrementally — the user watches tokens appear, which is
//! the whole point of streaming. Tool calls are **buffered**: Chat Completions
//! streams function arguments as fragments of a JSON string, and Anthropic's
//! `tool_use` block carries a parsed object, so there is nothing meaningful to
//! emit until the arguments are complete. Buffering a tool call costs no
//! perceived latency (the agent cannot act on half a call either way).

use serde_json::{Value, json};

use crate::response::{finish_reason_to_stop_reason, tool_call_to_block, usage_to_anthropic};

/// Accumulates a tool call arriving as `delta.tool_calls` fragments.
#[derive(Debug, Default, Clone)]
struct PartialToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Translates a Chat Completions event stream into an Anthropic event stream.
///
/// Feed it SSE chunks with [`ChatToAnthropicStream::push`]; it returns the
/// Anthropic SSE bytes to forward. Call [`ChatToAnthropicStream::finish`] when
/// the upstream ends.
#[derive(Debug)]
pub struct ChatToAnthropicStream {
    requested_model: String,
    buffer: Vec<u8>,
    /// Emitted `message_start` yet?
    started: bool,
    /// Index of the open text block, if one is open.
    text_block_open: bool,
    /// Next content-block index to allocate.
    next_index: usize,
    /// Tool calls under construction, in the order the upstream introduced them.
    tool_calls: Vec<PartialToolCall>,
    finish_reason: Option<String>,
    usage: Option<Value>,
    /// Set once the terminal events have been written, so a duplicate `finish`
    /// or a trailing `[DONE]` cannot emit a second `message_stop`.
    closed: bool,
}

impl ChatToAnthropicStream {
    /// New translator reporting `requested_model` back to the client.
    #[must_use]
    pub fn new(requested_model: impl Into<String>) -> Self {
        Self {
            requested_model: requested_model.into(),
            buffer: Vec::new(),
            started: false,
            text_block_open: false,
            next_index: 0,
            tool_calls: Vec::new(),
            finish_reason: None,
            usage: None,
            closed: false,
        }
    }

    /// Feed upstream bytes; returns Anthropic SSE bytes to forward downstream.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some(pos) = find_boundary(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..pos).collect();
            self.consume_frame(&frame, &mut out);
        }
        out
    }

    /// Close the stream, emitting whatever terminal events are still owed.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if !self.buffer.is_empty() {
            let frame = std::mem::take(&mut self.buffer);
            self.consume_frame(&frame, &mut out);
        }
        self.close(&mut out);
        out
    }

    fn consume_frame(&mut self, frame: &[u8], out: &mut Vec<u8>) {
        let Ok(text) = std::str::from_utf8(frame) else {
            return;
        };
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                data.push_str(rest.trim_start());
            }
        }
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.close(out);
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(&data) else {
            return;
        };

        // A usage-only chunk (the `include_usage` tail) carries no choices.
        if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
            self.usage = Some(usage.clone());
        }

        let Some(choice) = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|c| c.first())
        else {
            return;
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return;
        };

        self.ensure_started(out);

        if let Some(text) = delta
            .get("content")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            self.ensure_text_block(out);
            write_event(
                out,
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                }),
            );
        }

        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.accumulate_tool_call(call);
            }
        }
    }

    /// Chat Completions identifies a streamed call by array index, and only the
    /// first fragment carries the id and name.
    fn accumulate_tool_call(&mut self, call: &Value) {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        if self.tool_calls.len() <= index {
            self.tool_calls
                .resize(index + 1, PartialToolCall::default());
        }
        let slot = &mut self.tool_calls[index];
        if let Some(id) = call.get("id").and_then(Value::as_str)
            && !id.is_empty()
        {
            slot.id = id.to_string();
        }
        if let Some(name) = call.pointer("/function/name").and_then(Value::as_str)
            && !name.is_empty()
        {
            slot.name = name.to_string();
        }
        if let Some(fragment) = call.pointer("/function/arguments").and_then(Value::as_str) {
            slot.arguments.push_str(fragment);
        }
    }

    fn ensure_started(&mut self, out: &mut Vec<u8>) {
        if self.started {
            return;
        }
        self.started = true;
        write_event(
            out,
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": "msg_ironwire",
                    "type": "message",
                    "role": "assistant",
                    "model": self.requested_model,
                    "content": [],
                    "stop_reason": Value::Null,
                    "stop_sequence": Value::Null,
                    "usage": {"input_tokens": 0, "output_tokens": 0},
                },
            }),
        );
    }

    fn ensure_text_block(&mut self, out: &mut Vec<u8>) {
        if self.text_block_open {
            return;
        }
        self.text_block_open = true;
        self.next_index = 1;
        write_event(
            out,
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""},
            }),
        );
    }

    fn close(&mut self, out: &mut Vec<u8>) {
        if self.closed {
            return;
        }
        self.closed = true;
        // A stream that produced nothing at all still owes the client a
        // well-formed message; an agent waiting on `message_stop` otherwise
        // hangs until its own timeout.
        self.ensure_started(out);

        if self.text_block_open {
            write_event(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": 0}),
            );
            self.text_block_open = false;
        }

        // Tool calls are complete only now, so this is the first point at which
        // they can be emitted as Anthropic blocks.
        let calls = std::mem::take(&mut self.tool_calls);
        for partial in calls {
            if partial.name.is_empty() {
                continue;
            }
            let index = self.next_index;
            self.next_index += 1;
            let block = tool_call_to_block(&json!({
                "id": partial.id,
                "function": {
                    "name": partial.name,
                    "arguments": if partial.arguments.is_empty() { "{}".to_string() } else { partial.arguments },
                },
            }));
            write_event(
                out,
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": index,
                    "content_block": block,
                }),
            );
            write_event(
                out,
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            );
        }

        write_event(
            out,
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": finish_reason_to_stop_reason(self.finish_reason.as_deref()),
                    "stop_sequence": Value::Null,
                },
                "usage": usage_to_anthropic(self.usage.as_ref()),
            }),
        );
        write_event(out, "message_stop", &json!({"type": "message_stop"}));
    }
}

fn write_event(out: &mut Vec<u8>, name: &str, payload: &Value) {
    out.extend_from_slice(format!("event: {name}\ndata: {payload}\n\n").as_bytes());
}

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

    fn chunk(payload: Value) -> String {
        format!("data: {payload}\n\n")
    }

    /// Parse the emitted Anthropic stream into (event name, payload) pairs.
    fn events(bytes: &[u8]) -> Vec<(String, Value)> {
        let text = String::from_utf8(bytes.to_vec()).expect("utf8");
        text.split("\n\n")
            .filter(|frame| !frame.trim().is_empty())
            .map(|frame| {
                let mut name = String::new();
                let mut data = String::new();
                for line in frame.lines() {
                    if let Some(rest) = line.strip_prefix("event: ") {
                        name = rest.to_string();
                    } else if let Some(rest) = line.strip_prefix("data: ") {
                        data.push_str(rest);
                    }
                }
                (
                    name,
                    serde_json::from_str(&data).expect("valid JSON payload"),
                )
            })
            .collect()
    }

    #[test]
    fn a_text_answer_streams_as_a_well_formed_anthropic_message() {
        let mut s = ChatToAnthropicStream::new("claude-opus-4-6");
        let mut out = Vec::new();
        out.extend(s.push(
            chunk(json!({"choices": [{"index": 0, "delta": {"role": "assistant", "content": "Hel"}}]}))
                .as_bytes(),
        ));
        out.extend(s.push(
            chunk(json!({"choices": [{"index": 0, "delta": {"content": "lo"}}]})).as_bytes(),
        ));
        out.extend(
            s.push(
                chunk(json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}))
                    .as_bytes(),
            ),
        );
        out.extend(
            s.push(
                chunk(
                    json!({"choices": [], "usage": {"prompt_tokens": 30, "completion_tokens": 2}}),
                )
                .as_bytes(),
            ),
        );
        out.extend(s.push(b"data: [DONE]\n\n"));

        let events = events(&out);
        let names: Vec<&str> = events.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[0].1["message"]["model"], "claude-opus-4-6");
        assert_eq!(events[2].1["delta"]["text"], "Hel");
        assert_eq!(events[3].1["delta"]["text"], "lo");
        assert_eq!(events[5].1["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5].1["usage"]["output_tokens"], 2);
    }

    #[test]
    fn a_streamed_tool_call_is_reassembled_into_one_tool_use_block() {
        // Chat Completions fragments the arguments string; Anthropic needs the
        // parsed object, so there is nothing to emit until it is whole.
        let mut s = ChatToAnthropicStream::new("m");
        let mut out = Vec::new();
        for delta in [
            json!({"tool_calls": [{"index": 0, "id": "call_1",
                                   "function": {"name": "Bash", "arguments": ""}}]}),
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"comm"}}]}),
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "and\":\"ls\"}"}}]}),
        ] {
            out.extend(
                s.push(chunk(json!({"choices": [{"index": 0, "delta": delta}]})).as_bytes()),
            );
        }
        out.extend(
            s.push(
                chunk(
                    json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                )
                .as_bytes(),
            ),
        );
        out.extend(s.finish());

        let events = events(&out);
        let block = events
            .iter()
            .find(|(n, p)| n == "content_block_start" && p["content_block"]["type"] == "tool_use")
            .map(|(_, p)| p["content_block"].clone())
            .expect("a tool_use block");
        assert_eq!(block["name"], "Bash");
        assert_eq!(block["input"]["command"], "ls");
        assert!(block["id"].as_str().expect("id").starts_with("toolu_"));

        let stop = events
            .iter()
            .find(|(n, _)| n == "message_delta")
            .expect("message_delta");
        assert_eq!(stop.1["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn parallel_tool_calls_become_distinct_indexed_blocks() {
        let mut s = ChatToAnthropicStream::new("m");
        let mut out = Vec::new();
        out.extend(
            s.push(
                chunk(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_a", "function": {"name": "Read", "arguments": "{}"}},
                    {"index": 1, "id": "call_b", "function": {"name": "Bash", "arguments": "{}"}}
                ]}}]}))
                .as_bytes(),
            ),
        );
        out.extend(s.finish());

        let events = events(&out);
        let blocks: Vec<Value> = events
            .iter()
            .filter(|(n, p)| n == "content_block_start" && p["content_block"]["type"] == "tool_use")
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["content_block"]["name"], "Read");
        assert_eq!(blocks[1]["content_block"]["name"], "Bash");
        assert_ne!(blocks[0]["index"], blocks[1]["index"]);
    }

    #[test]
    fn text_and_a_tool_call_get_distinct_block_indices() {
        let mut s = ChatToAnthropicStream::new("m");
        let mut out = Vec::new();
        out.extend(s.push(
            chunk(json!({"choices": [{"index": 0, "delta": {"content": "checking"}}]})).as_bytes(),
        ));
        out.extend(
            s.push(
                chunk(json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "call_a", "function": {"name": "Bash", "arguments": "{}"}}
                ]}}]}))
                .as_bytes(),
            ),
        );
        out.extend(s.finish());

        let events = events(&out);
        let text_index = events
            .iter()
            .find(|(n, p)| n == "content_block_start" && p["content_block"]["type"] == "text")
            .map(|(_, p)| p["index"].clone())
            .expect("text block");
        let tool_index = events
            .iter()
            .find(|(n, p)| n == "content_block_start" && p["content_block"]["type"] == "tool_use")
            .map(|(_, p)| p["index"].clone())
            .expect("tool block");
        assert_eq!(text_index, 0);
        assert_eq!(tool_index, 1);
    }

    #[test]
    fn byte_by_byte_delivery_produces_the_same_stream() {
        let source = concat!(
            r#"data: {"choices":[{"index":0,"delta":{"content":"hi"}}]}"#,
            "\n\n",
            r#"data: {"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "\n\n",
            "data: [DONE]\n\n",
        );
        let mut whole = ChatToAnthropicStream::new("m");
        let expected = {
            let mut out = whole.push(source.as_bytes());
            out.extend(whole.finish());
            out
        };
        let mut split = ChatToAnthropicStream::new("m");
        let mut got = Vec::new();
        for byte in source.as_bytes() {
            got.extend(split.push(&[*byte]));
        }
        got.extend(split.finish());
        assert_eq!(got, expected);
    }

    #[test]
    fn an_upstream_that_dies_silently_still_closes_the_client_stream() {
        // Otherwise the agent waits on message_stop until its own timeout.
        let mut s = ChatToAnthropicStream::new("m");
        let out = s.finish();
        let names: Vec<String> = events(&out).into_iter().map(|(n, _)| n).collect();
        assert_eq!(
            names,
            vec!["message_start", "message_delta", "message_stop"]
        );
    }

    #[test]
    fn a_done_marker_followed_by_finish_does_not_double_close() {
        let mut s = ChatToAnthropicStream::new("m");
        let mut out = s.push(b"data: [DONE]\n\n");
        out.extend(s.finish());
        let stops = events(&out)
            .into_iter()
            .filter(|(n, _)| n == "message_stop")
            .count();
        assert_eq!(stops, 1);
    }

    #[test]
    fn garbage_frames_are_skipped_without_breaking_the_stream() {
        let mut s = ChatToAnthropicStream::new("m");
        let mut out = s.push(b"data: {not json\n\n: comment\n\n");
        out.extend(s.push(
            chunk(json!({"choices": [{"index": 0, "delta": {"content": "ok"}}]})).as_bytes(),
        ));
        out.extend(s.finish());
        let events = events(&out);
        assert!(
            events
                .iter()
                .any(|(n, p)| n == "content_block_delta" && p["delta"]["text"] == "ok")
        );
    }
}

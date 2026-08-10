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
    /// Set when we refused a fragment because the accumulated arguments grew
    /// past what we accept. Such a call is dropped rather than emitted with
    /// truncated JSON.
    overflowed: bool,
}

/// Largest SSE frame we will accumulate before giving up on finding a boundary.
///
/// The buffer holds bytes until a `\n\n` arrives. An upstream that never sends
/// one — broken, or hostile — would otherwise grow it without limit. Real Chat
/// Completions frames are a few kilobytes; a megabyte is far past anything
/// legitimate and still small enough that discarding one costs nothing.
const MAX_FRAME_BYTES: usize = 1 << 20;

/// Most parallel tool calls a single response may declare.
///
/// The index comes from the upstream and drives a `Vec::resize`, so without a
/// bound a single frame saying `"index": 4000000000` allocates until the
/// process dies. IronWire lets a user point at an arbitrary
/// OpenAI-compatible endpoint, which makes that reachable rather than
/// theoretical. No model emits anywhere near this many.
const MAX_TOOL_CALLS: usize = 256;

/// Most bytes of accumulated arguments across all tool calls in one response.
///
/// Arguments arrive as fragments that are concatenated, so this is the third
/// place an upstream controls how much we allocate.
const MAX_TOOL_ARGUMENT_BYTES: usize = 4 << 20;

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
    /// Bytes of tool-call arguments accumulated across the whole response.
    tool_argument_bytes: usize,
    /// Set after discarding an oversized frame: the bytes we dropped may have
    /// been the middle of one, so everything until the next boundary is
    /// unusable too. Clearing the buffer alone is not enough — the surviving
    /// junk prefix would be glued to the next real frame and swallow it.
    resyncing: bool,
    /// p(chosen token) for each generated token, in order, when the request
    /// asked for logprobs. Empty otherwise, which is the default.
    ///
    /// Accumulated but never forwarded: the Anthropic Messages shape has
    /// nowhere to put these, and the client does not need them. They exist for
    /// local reduction into aggregates.
    token_probabilities: Vec<f32>,
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
            tool_argument_bytes: 0,
            resyncing: false,
            token_probabilities: Vec::new(),
        }
    }

    /// Probability of each token the model emitted, in order.
    ///
    /// Empty unless the request asked for logprobs and the backend honoured
    /// it. This is `exp(logprob)`, the quantity the confidence aggregates are
    /// defined over.
    #[must_use]
    pub fn token_probabilities(&self) -> &[f32] {
        &self.token_probabilities
    }

    /// Feed upstream bytes; returns Anthropic SSE bytes to forward downstream.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<u8> {
        self.buffer.extend_from_slice(chunk);
        let mut out = Vec::new();

        // Discard through the next boundary before parsing anything: after an
        // oversized frame we are mid-garbage, and treating the remainder as a
        // frame would glue it to the next real one.
        if self.resyncing {
            match find_boundary(&self.buffer) {
                Some(pos) => {
                    self.buffer.drain(..pos);
                    self.resyncing = false;
                }
                None => {
                    self.buffer.clear();
                    return out;
                }
            }
        }

        while let Some(pos) = find_boundary(&self.buffer) {
            let frame: Vec<u8> = self.buffer.drain(..pos).collect();
            self.consume_frame(&frame, &mut out);
        }

        // No boundary in sight and the buffer is past anything a real frame
        // could be: the upstream is broken or hostile. Drop what we have and
        // pick up at the next boundary rather than growing without limit — a
        // lost frame is a lost delta, and an OOM is every conversation on the
        // machine.
        if self.buffer.len() > MAX_FRAME_BYTES {
            tracing::warn!(
                bytes = self.buffer.len(),
                "discarding an oversized SSE frame with no boundary"
            );
            self.buffer.clear();
            self.resyncing = true;
        }
        out
    }

    /// Close the stream, emitting whatever terminal events are still owed.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        // A tail we were discarding is not a frame; parsing it would be reading
        // the middle of one.
        if self.resyncing {
            self.buffer.clear();
        }
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
        // Before the delta check on purpose: `logprobs` is a sibling of
        // `delta`, and the chunk carrying the finish reason often has one
        // without the other. Accumulating after would drop the final token.
        self.accumulate_logprobs(choice);
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
    /// Record p(chosen) for each token in this chunk's `logprobs`.
    ///
    /// A value that does not exponentiate into a probability is dropped rather
    /// than repaired: clamping one would bias the mean toward whichever bound
    /// the repair chose, and a backend emitting them is wrong in a way we
    /// should not paper over.
    fn accumulate_logprobs(&mut self, choice: &Value) {
        let Some(content) = choice
            .get("logprobs")
            .and_then(|l| l.get("content"))
            .and_then(Value::as_array)
        else {
            return;
        };
        for entry in content {
            let Some(logprob) = entry.get("logprob").and_then(Value::as_f64) else {
                continue;
            };
            let probability = logprob.exp() as f32;
            if probability.is_finite() && (0.0..=1.0).contains(&probability) {
                self.token_probabilities.push(probability);
            }
        }
    }

    fn accumulate_tool_call(&mut self, call: &Value) {
        let index = call
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .unwrap_or(0);
        // The index is upstream-controlled and drives the resize below. Without
        // this check a single frame claiming `"index": 4000000000` allocates
        // until the process dies, and IronWire will point at any
        // OpenAI-compatible endpoint a user names.
        if index >= MAX_TOOL_CALLS {
            tracing::warn!(index, "ignoring a tool call with an implausible index");
            return;
        }
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
            // Third upstream-controlled growth path. Truncating would produce a
            // `tool_use` block with unparseable input, which the client would
            // hand to a tool; refusing the fragment and letting `close` drop
            // the call is the lesser failure.
            if self.tool_argument_bytes + fragment.len() > MAX_TOOL_ARGUMENT_BYTES {
                if !slot.overflowed {
                    tracing::warn!("tool-call arguments exceeded the accepted size");
                }
                slot.overflowed = true;
                return;
            }
            self.tool_argument_bytes += fragment.len();
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
            // A call whose arguments we refused is not a call we can emit: the
            // client would pass truncated JSON to a tool. Dropping it makes the
            // turn visibly incomplete rather than silently wrong.
            if partial.overflowed {
                tracing::warn!(
                    name = %partial.name,
                    "dropping a tool call whose arguments exceeded the accepted size"
                );
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
    fn probabilities_are_empty_without_capture() {
        let mut s = ChatToAnthropicStream::new("m");
        s.push(chunk(json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]})).as_bytes());
        assert!(
            s.token_probabilities().is_empty(),
            "no logprobs requested means nothing accumulated"
        );
    }

    #[test]
    fn probabilities_accumulate_across_chunks_in_order() {
        let mut s = ChatToAnthropicStream::new("m");
        for (text, logprob) in [("a", 0.5f64.ln()), ("b", 0.25f64.ln())] {
            s.push(
                chunk(json!({"choices": [{
                    "index": 0,
                    "delta": {"content": text},
                    "logprobs": {"content": [{"token": text, "logprob": logprob}]}
                }]}))
                .as_bytes(),
            );
        }
        let probs = s.token_probabilities();
        assert_eq!(probs.len(), 2);
        assert!((probs[0] - 0.5).abs() < 1e-5, "got {}", probs[0]);
        assert!((probs[1] - 0.25).abs() < 1e-5, "got {}", probs[1]);
    }

    /// `logprobs` is a sibling of `delta`, so a chunk carrying a distribution
    /// and a finish reason but no delta must still be counted. Accumulating
    /// after the delta check would silently drop the final token.
    #[test]
    fn probabilities_survive_a_chunk_with_no_delta() {
        let mut s = ChatToAnthropicStream::new("m");
        s.push(
            chunk(json!({"choices": [{
                "index": 0,
                "finish_reason": "stop",
                "logprobs": {"content": [{"token": "!", "logprob": 0.5f64.ln()}]}
            }]}))
            .as_bytes(),
        );
        assert_eq!(s.token_probabilities().len(), 1);
    }

    /// A backend that returns a malformed logprob must not corrupt the
    /// aggregate. Dropping beats repairing, which biases the mean.
    #[test]
    fn malformed_logprobs_are_dropped() {
        let mut s = ChatToAnthropicStream::new("m");
        s.push(
            chunk(json!({"choices": [{
                "index": 0,
                "delta": {"content": "x"},
                "logprobs": {"content": [
                    {"token": "ok", "logprob": 0.5f64.ln()},
                    {"token": "bad", "logprob": 5.0},
                    {"token": "worse", "logprob": "not a number"}
                ]}
            }]}))
            .as_bytes(),
        );
        let probs = s.token_probabilities();
        assert_eq!(probs.len(), 1, "only the well-formed token survives");
        assert!((probs[0] - 0.5).abs() < 1e-5);
    }

    /// Capture must not change a single downstream byte. The whole safety
    /// argument for asking for logprobs is that the client sees the same
    /// stream it would have seen.
    #[test]
    fn capture_does_not_alter_the_downstream_stream() {
        let frames = |with_logprobs: bool| {
            let mut s = ChatToAnthropicStream::new("m");
            let mut out = Vec::new();
            let mut delta = json!({"index": 0, "delta": {"content": "Hello"}});
            if with_logprobs {
                delta["logprobs"] = json!({"content": [{"token": "Hello", "logprob": -0.1}]});
            }
            out.extend_from_slice(&s.push(chunk(json!({"choices": [delta]})).as_bytes()));
            out.extend_from_slice(&s.finish());
            out
        };
        assert_eq!(
            frames(false),
            frames(true),
            "requesting logprobs changed what the client receives"
        );
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

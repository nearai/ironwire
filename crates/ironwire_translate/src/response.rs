//! OpenAI Chat Completions → Anthropic Messages (the response half).
//!
//! Both directions matter, but this one is where a mistake is expensive: the
//! client stores whatever we hand it and replays it forever. A malformed
//! `stop_reason` or a tool id in the wrong namespace poisons every later turn
//! of the conversation, not just this one.

use serde_json::{Value, json};

use crate::tool_ids;

/// Translate a non-streaming Chat Completions response into an Anthropic
/// Messages response.
#[must_use]
pub fn chat_completion_to_anthropic(response: &Value, requested_model: &str) -> Value {
    let choice = response
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());
    let message = choice.and_then(|c| c.get("message"));

    let mut content: Vec<Value> = Vec::new();
    if let Some(text) = message
        .and_then(|m| m.get("content"))
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
    {
        content.push(json!({"type": "text", "text": text}));
    }
    if let Some(calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for call in calls {
            content.push(tool_call_to_block(call));
        }
    }

    let finish = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);
    json!({
        "id": response.get("id").and_then(Value::as_str).unwrap_or("msg_ironwire"),
        "type": "message",
        "role": "assistant",
        // The model the client asked for, not the one that served it. The
        // served model is reported separately; putting a foreign slug here
        // would make the client's own bookkeeping incoherent.
        "model": requested_model,
        "content": content,
        "stop_reason": finish_reason_to_stop_reason(finish),
        "stop_sequence": Value::Null,
        "usage": usage_to_anthropic(response.get("usage")),
    })
}

/// Build an Anthropic `tool_use` block from a Chat Completions tool call.
#[must_use]
pub fn tool_call_to_block(call: &Value) -> Value {
    let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
    let name = call
        .pointer("/function/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = call
        .pointer("/function/arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    json!({
        "type": "tool_use",
        "id": tool_ids::to_anthropic(id),
        "name": name,
        // Anthropic carries input as an object; Chat Completions as a string.
        // A model that emits malformed JSON must not take the whole turn down —
        // an empty object lets the agent see a failed call and recover.
        "input": serde_json::from_str::<Value>(arguments).unwrap_or_else(|_| json!({})),
    })
}

/// Map a Chat Completions `finish_reason` onto an Anthropic `stop_reason`.
///
/// `tool_calls` → `tool_use` is the one that matters: get it wrong and the
/// agent stops instead of executing the call it was just handed.
#[must_use]
pub fn finish_reason_to_stop_reason(finish: Option<&str>) -> Value {
    match finish {
        Some("stop") => json!("end_turn"),
        Some("length") => json!("max_tokens"),
        Some("tool_calls" | "function_call") => json!("tool_use"),
        Some("content_filter") => json!("refusal"),
        // Still generating, or a dialect we do not know. `null` is the honest
        // answer and the one Anthropic itself uses mid-stream.
        _ => Value::Null,
    }
}

/// Map Chat Completions usage onto Anthropic's shape.
#[must_use]
pub fn usage_to_anthropic(usage: Option<&Value>) -> Value {
    let n = |key: &str| {
        usage
            .and_then(|u| u.get(key))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let cached = usage
        .and_then(|u| u.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    json!({
        "input_tokens": n("prompt_tokens").saturating_sub(cached),
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": cached,
        "output_tokens": n("completion_tokens"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_text_answer_becomes_an_anthropic_message() {
        let response = json!({
            "id": "chatcmpl-1",
            "choices": [{"index": 0, "finish_reason": "stop",
                         "message": {"role": "assistant", "content": "done"}}],
            "usage": {"prompt_tokens": 120, "completion_tokens": 8},
        });
        let out = chat_completion_to_anthropic(&response, "claude-opus-4-6");
        assert_eq!(out["type"], "message");
        assert_eq!(out["role"], "assistant");
        assert_eq!(out["model"], "claude-opus-4-6");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "done");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 120);
        assert_eq!(out["usage"]["output_tokens"], 8);
    }

    #[test]
    fn a_tool_call_becomes_a_tool_use_block_with_object_input() {
        let response = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "Bash", "arguments": "{\"command\":\"cargo test\"}"}
                }]
            }}],
        });
        let out = chat_completion_to_anthropic(&response, "claude-opus-4-6");
        assert_eq!(out["stop_reason"], "tool_use");
        let block = &out["content"][0];
        assert_eq!(block["type"], "tool_use");
        assert_eq!(block["name"], "Bash");
        // Object, not a string — Anthropic clients index into it.
        assert_eq!(block["input"]["command"], "cargo test");
        assert!(
            block["id"].as_str().expect("id").starts_with("toolu_"),
            "id must be valid in the client's namespace: {block}"
        );
    }

    #[test]
    fn text_and_a_tool_call_in_one_turn_both_survive() {
        let response = json!({
            "choices": [{"finish_reason": "tool_calls", "message": {
                "content": "Let me check.",
                "tool_calls": [{"id": "call_1", "function": {"name": "Bash", "arguments": "{}"}}]
            }}],
        });
        let out = chat_completion_to_anthropic(&response, "m");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][1]["type"], "tool_use");
    }

    #[test]
    fn malformed_tool_arguments_do_not_take_the_turn_down() {
        // A small model emitting broken JSON should surface as a failed tool
        // call the agent can retry, not as a crashed request.
        let call = json!({"id": "call_1", "function": {"name": "Bash", "arguments": "{not json"}});
        let block = tool_call_to_block(&call);
        assert_eq!(block["input"], json!({}));
        assert_eq!(block["name"], "Bash");
    }

    #[test]
    fn finish_reasons_map_to_the_stop_reasons_agents_branch_on() {
        assert_eq!(finish_reason_to_stop_reason(Some("stop")), "end_turn");
        assert_eq!(finish_reason_to_stop_reason(Some("length")), "max_tokens");
        assert_eq!(finish_reason_to_stop_reason(Some("tool_calls")), "tool_use");
        assert_eq!(
            finish_reason_to_stop_reason(Some("content_filter")),
            "refusal"
        );
        assert!(finish_reason_to_stop_reason(None).is_null());
        assert!(finish_reason_to_stop_reason(Some("something_new")).is_null());
    }

    #[test]
    fn cached_prompt_tokens_are_not_double_counted() {
        let usage = json!({
            "prompt_tokens": 1000,
            "prompt_tokens_details": {"cached_tokens": 800},
            "completion_tokens": 50,
        });
        let out = usage_to_anthropic(Some(&usage));
        assert_eq!(out["input_tokens"], 200);
        assert_eq!(out["cache_read_input_tokens"], 800);
        assert_eq!(out["output_tokens"], 50);
    }

    #[test]
    fn an_empty_response_still_produces_a_valid_envelope() {
        let out = chat_completion_to_anthropic(&json!({}), "m");
        assert_eq!(out["type"], "message");
        assert!(out["content"].as_array().expect("array").is_empty());
        assert!(out["stop_reason"].is_null());
    }
}

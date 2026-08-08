//! Bounded, non-destructive inspection of a request body.
//!
//! The native lane forwards bytes without re-encoding them, so everything
//! routing needs must come from a *peek*: parse once, read the handful of
//! fields policy depends on, and then forward the original bytes
//! (`docs/PROTOCOL.md` §2). Nothing in this module ever produces a body.

use serde_json::Value;

use crate::capability::{ReasoningNeed, RequestRequirements};
use crate::protocol::Protocol;

/// Rough bytes-per-token used to size prompts without tokenising.
///
/// Deliberately crude: this feeds cache-sacrifice and context-fit decisions
/// where being within ~30% is enough, and a real tokeniser would cost more than
/// the decision is worth. Every place this is used tolerates the error.
const BYTES_PER_TOKEN_ESTIMATE: usize = 4;

/// The identifying prefix Claude Code puts in its first system block. Its
/// presence is how the Claude-subscription backend decides a request is
/// genuinely Claude Code (`docs/TRUST.md` §3) — IronWire reads this signal, and
/// never writes it.
pub const CLAUDE_CODE_SYSTEM_PREFIX: &str = "You are Claude Code";

/// What policy learned from one look at the body.
#[derive(Debug, Clone, PartialEq)]
pub struct RequestPeek {
    /// The model the client asked for, verbatim.
    pub requested_model: Option<String>,
    /// Whether the client asked for SSE.
    pub stream: bool,
    /// What the request needs a backend to preserve.
    pub requirements: RequestRequirements,
    /// Whether the request carries the originating product's own client
    /// identity — the eligibility signal for subscription backends.
    pub carries_client_identity: bool,
    /// Number of messages, for conversation-key derivation and logging.
    pub message_count: usize,
}

impl RequestPeek {
    /// Inspect a parsed request body for the given protocol.
    ///
    /// Never fails: an unrecognised body yields a peek with no requirements,
    /// which the router treats as "anything can serve this". That is the right
    /// default — a shape we do not understand is one we must not claim to have
    /// analysed.
    #[must_use]
    pub fn inspect(protocol: Protocol, body: &Value, raw_len: usize) -> Self {
        match protocol {
            Protocol::AnthropicMessages => Self::inspect_anthropic(body, raw_len),
            Protocol::OpenAiResponses | Protocol::OpenAiChat => Self::inspect_openai(body, raw_len),
        }
    }

    fn inspect_anthropic(body: &Value, raw_len: usize) -> Self {
        let messages = body.get("messages").and_then(Value::as_array);
        let system = body.get("system");

        let mut requirements = RequestRequirements {
            tools: body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|t| !t.is_empty()),
            // Anthropic emits parallel tool calls unless explicitly disabled.
            parallel_tool_calls: !body
                .pointer("/tool_choice/disable_parallel_tool_use")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            images: false,
            reasoning: if body.get("thinking").is_some() {
                ReasoningNeed::Requested
            } else {
                ReasoningNeed::None
            },
            prompt_cache: false,
            cached_prefix_tokens: 0,
            structured_output: false,
            min_context_tokens: estimate_tokens(raw_len),
        };

        // A signed thinking block anywhere in the replayed history pins this
        // conversation to the Anthropic family for good.
        let mut cached_prefix_bytes = 0usize;
        if let Some(messages) = messages {
            for message in messages {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                for block in blocks {
                    match block.get("type").and_then(Value::as_str) {
                        Some("image") => requirements.images = true,
                        Some("thinking" | "redacted_thinking") => {
                            if block.get("signature").is_some() || block.get("data").is_some() {
                                requirements.reasoning = ReasoningNeed::LoadBearing;
                            }
                        }
                        _ => {}
                    }
                    if block.get("cache_control").is_some() {
                        requirements.prompt_cache = true;
                        // Everything up to and including this breakpoint is
                        // cacheable; approximate it by the bytes seen so far.
                        cached_prefix_bytes = cached_prefix_bytes.max(serialized_len_hint(message));
                    }
                }
            }
        }
        if let Some(system) = system
            && json_contains_key(system, "cache_control")
        {
            requirements.prompt_cache = true;
            cached_prefix_bytes = cached_prefix_bytes.max(serialized_len_hint(system));
        }
        // The cacheable prefix is everything before the last breakpoint, which
        // in practice is nearly the whole body for a long coding session.
        if requirements.prompt_cache {
            requirements.cached_prefix_tokens =
                estimate_tokens(cached_prefix_bytes.max(raw_len / 2));
        }
        if body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| tools.iter().any(|t| json_contains_key(t, "cache_control")))
        {
            requirements.prompt_cache = true;
        }

        Self {
            requested_model: body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            carries_client_identity: anthropic_system_prefix(system)
                .is_some_and(|s| s.starts_with(CLAUDE_CODE_SYSTEM_PREFIX)),
            message_count: messages.map_or(0, Vec::len),
            requirements,
        }
    }

    fn inspect_openai(body: &Value, raw_len: usize) -> Self {
        // `input` is the Responses API; `messages` is Chat Completions.
        let items = body
            .get("input")
            .and_then(Value::as_array)
            .or_else(|| body.get("messages").and_then(Value::as_array));

        let mut requirements = RequestRequirements {
            tools: body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|t| !t.is_empty()),
            parallel_tool_calls: body
                .get("parallel_tool_calls")
                .and_then(Value::as_bool)
                .unwrap_or(true),
            images: false,
            reasoning: if body.get("reasoning").is_some() {
                ReasoningNeed::Requested
            } else {
                ReasoningNeed::None
            },
            // OpenAI caches automatically; there are no client breakpoints to
            // preserve, so there is nothing here for the gate to protect.
            prompt_cache: false,
            cached_prefix_tokens: 0,
            structured_output: body
                .pointer("/response_format/type")
                .and_then(Value::as_str)
                .is_some_and(|t| t == "json_schema")
                || body.pointer("/text/format/type").and_then(Value::as_str) == Some("json_schema"),
            min_context_tokens: estimate_tokens(raw_len),
        };

        if let Some(items) = items {
            for item in items {
                // Encrypted reasoning state: same permanence as a signed
                // Anthropic thinking block, opposite direction.
                if item.get("type").and_then(Value::as_str) == Some("reasoning")
                    && item.get("encrypted_content").is_some()
                {
                    requirements.reasoning = ReasoningNeed::LoadBearing;
                }
                if json_contains_type(item, "input_image") || json_contains_type(item, "image_url")
                {
                    requirements.images = true;
                }
            }
        }

        Self {
            requested_model: body
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string),
            stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
            // Codex identifies itself with an `originator`/`instructions` pair;
            // the presence of its instructions block is the reliable half.
            carries_client_identity: body
                .get("instructions")
                .and_then(Value::as_str)
                .is_some_and(|s| s.contains("Codex")),
            message_count: items.map_or(0, Vec::len),
            requirements,
        }
    }
}

/// Anthropic's `system` is either a string or an array of blocks. Return the
/// text of the first block either way.
fn anthropic_system_prefix(system: Option<&Value>) -> Option<&str> {
    match system? {
        Value::String(s) => Some(s.as_str()),
        Value::Array(blocks) => blocks
            .first()
            .and_then(|b| b.get("text"))
            .and_then(Value::as_str),
        _ => None,
    }
}

fn estimate_tokens(bytes: usize) -> u32 {
    u32::try_from(bytes / BYTES_PER_TOKEN_ESTIMATE).unwrap_or(u32::MAX)
}

/// Cheap size hint without re-serialising: string lengths dominate a request
/// body, so summing them tracks the real size closely enough for the decisions
/// that consume it.
fn serialized_len_hint(value: &Value) -> usize {
    match value {
        Value::String(s) => s.len(),
        Value::Array(items) => items.iter().map(serialized_len_hint).sum(),
        Value::Object(map) => map
            .iter()
            .map(|(k, v)| k.len() + serialized_len_hint(v))
            .sum(),
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(_) => 8,
    }
}

fn json_contains_key(value: &Value, key: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.contains_key(key) || map.values().any(|v| json_contains_key(v, key))
        }
        Value::Array(items) => items.iter().any(|v| json_contains_key(v, key)),
        _ => false,
    }
}

fn json_contains_type(value: &Value, ty: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get("type").and_then(Value::as_str) == Some(ty)
                || map.values().any(|v| json_contains_type(v, ty))
        }
        Value::Array(items) => items.iter().any(|v| json_contains_type(v, ty)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn peek_anthropic(body: serde_json::Value) -> RequestPeek {
        let raw = body.to_string();
        RequestPeek::inspect(Protocol::AnthropicMessages, &body, raw.len())
    }

    #[test]
    fn detects_claude_code_identity_from_string_system() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "messages": [],
        }));
        assert!(p.carries_client_identity);
    }

    #[test]
    fn detects_claude_code_identity_from_block_system() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "system": [{"type": "text", "text": "You are Claude Code, Anthropic's official CLI."}],
            "messages": [],
        }));
        assert!(p.carries_client_identity);
    }

    #[test]
    fn a_third_party_client_does_not_carry_claude_code_identity() {
        // This is the signal that keeps Aider off the Claude subscription
        // without us forging anything (TRUST.md §3).
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "system": "You are a helpful coding assistant.",
            "messages": [],
        }));
        assert!(!p.carries_client_identity);
    }

    #[test]
    fn signed_thinking_in_history_is_load_bearing() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "thinking", "thinking": "...", "signature": "abc123"},
                    {"type": "text", "text": "done"}
                ]
            }],
        }));
        assert_eq!(p.requirements.reasoning, ReasoningNeed::LoadBearing);
    }

    #[test]
    fn requesting_thinking_without_history_is_only_requested() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "thinking": {"type": "enabled", "budget_tokens": 4000},
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert_eq!(p.requirements.reasoning, ReasoningNeed::Requested);
    }

    #[test]
    fn cache_control_anywhere_marks_the_prefix() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "system": [{"type": "text", "text": "big prompt", "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": "hi"}],
        }));
        assert!(p.requirements.prompt_cache);
        assert!(p.requirements.cached_prefix_tokens > 0);
    }

    #[test]
    fn images_are_detected_in_message_blocks() {
        let p = peek_anthropic(json!({
            "model": "claude-opus-4-6",
            "messages": [{
                "role": "user",
                "content": [{"type": "image", "source": {"type": "base64", "data": "AAA"}}]
            }],
        }));
        assert!(p.requirements.images);
    }

    #[test]
    fn parallel_tools_default_on_unless_disabled() {
        let on = peek_anthropic(json!({"model": "m", "messages": []}));
        assert!(on.requirements.parallel_tool_calls);

        let off = peek_anthropic(json!({
            "model": "m",
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
        }));
        assert!(!off.requirements.parallel_tool_calls);
    }

    #[test]
    fn encrypted_openai_reasoning_is_load_bearing() {
        let body = json!({
            "model": "gpt-5.6",
            "input": [
                {"type": "reasoning", "encrypted_content": "gAAAA..."},
                {"type": "message", "role": "user", "content": "hi"}
            ],
        });
        let raw = body.to_string();
        let p = RequestPeek::inspect(Protocol::OpenAiResponses, &body, raw.len());
        assert_eq!(p.requirements.reasoning, ReasoningNeed::LoadBearing);
    }

    #[test]
    fn an_unrecognised_body_claims_nothing() {
        let body = json!({"something": "entirely else"});
        let p = RequestPeek::inspect(Protocol::AnthropicMessages, &body, 32);
        assert_eq!(p.requested_model, None);
        assert!(!p.stream);
        assert!(!p.carries_client_identity);
        assert!(!p.requirements.tools);
        assert_eq!(p.message_count, 0);
    }

    #[test]
    fn stream_flag_round_trips() {
        assert!(peek_anthropic(json!({"model": "m", "stream": true, "messages": []})).stream);
        assert!(!peek_anthropic(json!({"model": "m", "messages": []})).stream);
    }
}

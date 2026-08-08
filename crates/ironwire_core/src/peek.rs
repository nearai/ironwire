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
///
/// Compiled-in default. The live value can be refreshed through the signed
/// quirks channel (`docs/UPDATES.md`), because a marker string is exactly the
/// kind of thing that changes without notice.
pub const CLAUDE_CODE_SYSTEM_PREFIX: &str = "You are Claude Code";

/// Markers used to recognise a client's own identity, so the caller can supply
/// refreshed ones without this crate depending on the quirks channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityMarkers {
    /// Prefix of Claude Code's first system block.
    pub claude_code_system_prefix: String,
    /// Substring identifying Codex in a Responses `instructions` field.
    pub codex_instructions_marker: String,
}

impl Default for IdentityMarkers {
    fn default() -> Self {
        Self {
            claude_code_system_prefix: CLAUDE_CODE_SYSTEM_PREFIX.to_string(),
            codex_instructions_marker: "Codex".to_string(),
        }
    }
}

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
        Self::inspect_with(protocol, body, raw_len, &IdentityMarkers::default())
    }

    /// Inspect with caller-supplied identity markers.
    #[must_use]
    pub fn inspect_with(
        protocol: Protocol,
        body: &Value,
        raw_len: usize,
        markers: &IdentityMarkers,
    ) -> Self {
        match protocol {
            Protocol::AnthropicMessages => Self::inspect_anthropic(body, raw_len, markers),
            Protocol::OpenAiResponses | Protocol::OpenAiChat => {
                Self::inspect_openai(body, raw_len, markers)
            }
        }
    }

    fn inspect_anthropic(body: &Value, raw_len: usize, markers: &IdentityMarkers) -> Self {
        let messages = body.get("messages").and_then(Value::as_array);
        let system = body.get("system");

        let mut requirements = RequestRequirements {
            tools: body
                .get("tools")
                .and_then(Value::as_array)
                .is_some_and(|t| !t.is_empty()),
            // Set below from the history: what matters is whether the client
            // *depends* on parallel calls, not whether it permits them. Anthropic
            // permits them by default, so the permissive reading would refuse
            // every serial backend for every request.
            parallel_tool_calls: false,
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
            mid_tool_loop: anthropic_mid_tool_loop(messages),
        };

        // A signed thinking block anywhere in the replayed history means a family
        // change would lose reasoning continuity — recorded so the route can say
        // so, but not a refusal (see `capability::ReasoningNeed`).
        let mut cached_prefix_bytes = 0usize;
        if let Some(messages) = messages {
            for message in messages {
                let Some(blocks) = message.get("content").and_then(Value::as_array) else {
                    continue;
                };
                let mut tool_uses_in_turn = 0usize;
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
                    if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                        tool_uses_in_turn += 1;
                    }
                    if block.get("cache_control").is_some() {
                        requirements.prompt_cache = true;
                        // Everything up to and including this breakpoint is
                        // cacheable; approximate it by the bytes seen so far.
                        cached_prefix_bytes = cached_prefix_bytes.max(serialized_len_hint(message));
                    }
                }
                if tool_uses_in_turn > 1 {
                    requirements.parallel_tool_calls = true;
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
                .is_some_and(|s| s.starts_with(&markers.claude_code_system_prefix)),
            message_count: messages.map_or(0, Vec::len),
            requirements,
        }
    }

    fn inspect_openai(body: &Value, raw_len: usize, markers: &IdentityMarkers) -> Self {
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
            // As on the Anthropic side: set from the history, not from the
            // permissive default.
            parallel_tool_calls: false,
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
            mid_tool_loop: openai_mid_tool_loop(items),
        };

        if let Some(items) = items {
            let mut pending_calls = 0usize;
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    // Responses API: consecutive calls before their outputs.
                    Some("function_call") => pending_calls += 1,
                    Some("function_call_output") => pending_calls = 0,
                    _ => {}
                }
                if pending_calls > 1 {
                    requirements.parallel_tool_calls = true;
                }
                // Chat Completions: several calls on one assistant message.
                if item
                    .get("tool_calls")
                    .and_then(Value::as_array)
                    .is_some_and(|calls| calls.len() > 1)
                {
                    requirements.parallel_tool_calls = true;
                }
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
                .is_some_and(|s| s.contains(&markers.codex_instructions_marker)),
            message_count: items.map_or(0, Vec::len),
            requirements,
        }
    }
}

/// Whether the conversation is mid tool loop: the last message replays tool
/// results, so the model is expected to continue an exchange already in flight.
///
/// This is the cross-family switch point (`docs/PROTOCOL.md` §6). It is
/// deliberately keyed on the *last* message rather than "any tool use in
/// history" — a long session is mid-loop only between a tool call and the turn
/// that consumes its result, and a rule keyed on history would never let a
/// tool-using agent change families at all.
fn anthropic_mid_tool_loop(messages: Option<&Vec<Value>>) -> bool {
    let Some(last) = messages.and_then(|m| m.last()) else {
        return false;
    };
    if last.get("role").and_then(Value::as_str) != Some("user") {
        return false;
    }
    last.get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        })
}

/// The OpenAI equivalent: a trailing `function_call_output` item (Responses) or
/// a trailing `role: "tool"` message (Chat Completions).
fn openai_mid_tool_loop(items: Option<&Vec<Value>>) -> bool {
    let Some(last) = items.and_then(|i| i.last()) else {
        return false;
    };
    last.get("type").and_then(Value::as_str) == Some("function_call_output")
        || last.get("role").and_then(Value::as_str) == Some("tool")
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
    fn parallel_tools_are_flagged_only_when_the_history_actually_uses_them() {
        // Anthropic permits parallel calls by default, so reading "permitted"
        // as "required" would refuse every serial backend for every request.
        let permitted =
            peek_anthropic(json!({"model": "m", "tools": [{"name": "Read"}], "messages": []}));
        assert!(!permitted.requirements.parallel_tool_calls);

        let used = peek_anthropic(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}},
                    {"type": "tool_use", "id": "toolu_2", "name": "Read", "input": {}}
                ]
            }],
        }));
        assert!(used.requirements.parallel_tool_calls);

        let single = peek_anthropic(json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "Read", "input": {}}]
            }],
        }));
        assert!(!single.requirements.parallel_tool_calls);
    }

    #[test]
    fn a_replayed_tool_result_marks_the_conversation_mid_tool_loop() {
        // This is the cross-family switch gate — see capability::eligible.
        let mid = peek_anthropic(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "fix it"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "failed"}
                ]}
            ],
        }));
        assert!(mid.requirements.mid_tool_loop);
    }

    #[test]
    fn a_fresh_user_turn_is_a_clean_boundary_even_after_many_tool_calls() {
        // Keyed on the last message, not on history: a long tool-using session
        // must still be able to change families between turns.
        let boundary = peek_anthropic(json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "fix it"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
                ]},
                {"role": "assistant", "content": [{"type": "text", "text": "done"}]},
                {"role": "user", "content": "now add a test"}
            ],
        }));
        assert!(!boundary.requirements.mid_tool_loop);
    }

    #[test]
    fn openai_mid_loop_is_detected_in_both_dialects() {
        let responses = json!({
            "model": "gpt-5.6",
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "bash"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
            ],
        });
        let raw = responses.to_string();
        assert!(
            RequestPeek::inspect(Protocol::OpenAiResponses, &responses, raw.len())
                .requirements
                .mid_tool_loop
        );

        let chat = json!({
            "model": "m",
            "messages": [
                {"role": "assistant", "tool_calls": [{"id": "call_1"}]},
                {"role": "tool", "tool_call_id": "call_1", "content": "ok"}
            ],
        });
        let raw = chat.to_string();
        assert!(
            RequestPeek::inspect(Protocol::OpenAiChat, &chat, raw.len())
                .requirements
                .mid_tool_loop
        );
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

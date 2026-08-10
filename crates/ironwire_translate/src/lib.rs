//! Cross-family protocol translation — IronWire's fallback lane.
//!
//! The native lane forwards bytes and is exact. This crate is what runs when
//! there is no same-wire capacity left: it re-expresses a request that arrived
//! on one protocol onto another, and maps the answer back, so a session can
//! keep working instead of stopping at a rate limit.
//!
//! Everything goes through a **pivot IR** ([`ir`]) rather than a translator per
//! pair. Three protocols make six ordered pairs and eighteen mappings done
//! pairwise; the pivot makes it three parsers in and three emitters out per
//! layer, and — the part that actually matters — puts tool-call buffering, SSE
//! framing, id identity and usage accounting in one place instead of six.
//! `docs/TRANSLATION.md` is the design.
//!
//! Three rules shape everything here:
//!
//! 1. **Eligibility is not decided in this crate.** By the time a body reaches
//!    these functions, [`ironwire_core::capability::eligible`] has already
//!    decided the route is legal. The one cross-family correctness rule —
//!    switch at a turn boundary, never mid tool loop — lives there, because it
//!    is a routing decision rather than a mapping one.
//! 2. **Parsing is lossless; emitting reports the loss.** What a client sent is
//!    a fact; what a target can carry is a fact about that target.
//! 3. **Nothing is dropped silently.** [`ir::Dropped`] names what did not
//!    survive so the route can say what it cost.
#![warn(missing_docs)]

pub mod anthropic;
pub mod chat;
pub mod ir;
pub mod responses;
pub mod stream;
pub mod tool_ids;

use ironwire_core::protocol::Protocol;
use serde_json::Value;

pub use ir::{Completion, Conversation, Delta, Dropped, StopReason, Usage};
pub use stream::Translator;

/// Parse a request that arrived on `protocol`.
///
/// Total: every wire parses, and anything not modelled is carried whole rather
/// than judged here.
#[must_use]
pub fn parse_request(protocol: Protocol, body: &Value) -> Conversation {
    match protocol {
        Protocol::AnthropicMessages => anthropic::parse_request(body),
        Protocol::OpenAiResponses => responses::parse_request(body),
        Protocol::OpenAiChat => chat::parse_request(body),
    }
}

/// Write a request for `protocol`, reporting what the target could not carry.
#[must_use]
pub fn emit_request(
    protocol: Protocol,
    conversation: &Conversation,
    model: &str,
) -> (Value, Dropped) {
    match protocol {
        Protocol::AnthropicMessages => anthropic::emit_request(conversation, model),
        Protocol::OpenAiResponses => responses::emit_request(conversation, model),
        Protocol::OpenAiChat => chat::emit_request(conversation, model),
    }
}

/// Parse a non-streaming answer that arrived on `protocol`.
#[must_use]
pub fn parse_completion(protocol: Protocol, response: &Value) -> Completion {
    match protocol {
        Protocol::AnthropicMessages => anthropic::parse_completion(response),
        Protocol::OpenAiResponses => responses::parse_completion(response),
        Protocol::OpenAiChat => chat::parse_completion(response),
    }
}

/// Write a non-streaming answer for `protocol`.
///
/// `requested_model` is the model the client asked for, never the one that
/// served it: a foreign slug makes the client's own bookkeeping incoherent.
#[must_use]
pub fn emit_completion(
    protocol: Protocol,
    completion: &Completion,
    requested_model: &str,
) -> (Value, Dropped) {
    match protocol {
        Protocol::AnthropicMessages => anthropic::emit_completion(completion, requested_model),
        Protocol::OpenAiResponses => responses::emit_completion(completion, requested_model),
        Protocol::OpenAiChat => chat::emit_completion(completion, requested_model),
    }
}

/// The path a translated request is sent to on `protocol`.
#[must_use]
pub fn endpoint_path(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::AnthropicMessages => "/v1/messages",
        Protocol::OpenAiResponses => "/v1/responses",
        Protocol::OpenAiChat => "/v1/chat/completions",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const EVERY: [Protocol; 3] = [
        Protocol::AnthropicMessages,
        Protocol::OpenAiResponses,
        Protocol::OpenAiChat,
    ];

    /// Every pair, in both directions, produces something the target could
    /// plausibly read — and never a signature from the wrong provider.
    ///
    /// The point of a matrix test is the pairs nobody thought to write a test
    /// for: this is what stops the fifteen new lanes from being fifteen new
    /// places to be silently wrong.
    #[test]
    fn every_pair_translates_without_leaking_foreign_state() {
        let sources = [
            (
                Protocol::AnthropicMessages,
                json!({"model": "claude-opus-4-6", "max_tokens": 100, "stream": true,
                       "system": "be brief",
                       "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
                       "messages": [
                           {"role": "user", "content": "hi"},
                           {"role": "assistant", "content": [
                               {"type": "thinking", "thinking": "hmm", "signature": "SECRET"},
                               {"type": "tool_use", "id": "toolu_1", "name": "Bash", "input": {}}]},
                           {"role": "user", "content": [
                               {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}]}]}),
            ),
            (
                Protocol::OpenAiResponses,
                json!({"model": "gpt-5.6", "stream": true, "instructions": "be brief",
                       "tools": [{"type": "function", "name": "shell", "parameters": {}}],
                       "input": [
                           {"type": "message", "role": "user",
                            "content": [{"type": "input_text", "text": "hi"}]},
                           {"type": "reasoning", "id": "rs_1", "encrypted_content": "SECRET"},
                           {"type": "function_call", "call_id": "call_1", "name": "shell",
                            "arguments": "{}"},
                           {"type": "function_call_output", "call_id": "call_1", "output": "ok"}]}),
            ),
            (
                Protocol::OpenAiChat,
                json!({"model": "qwen3", "stream": true,
                       "tools": [{"type": "function", "function": {"name": "ls", "parameters": {}}}],
                       "messages": [
                           {"role": "system", "content": "be brief"},
                           {"role": "user", "content": "hi"},
                           {"role": "assistant", "content": null, "tool_calls": [
                               {"id": "call_1", "type": "function",
                                "function": {"name": "ls", "arguments": "{}"}}]},
                           {"role": "tool", "tool_call_id": "call_1", "content": "ok"}]}),
            ),
        ];

        for (from, body) in &sources {
            let ir = parse_request(*from, body);
            for to in EVERY {
                let (out, dropped) = emit_request(to, &ir, "target-model");
                assert!(
                    dropped.unknown_blocks.is_empty(),
                    "{from} → {to} refused a block it should have understood: {dropped:?}"
                );
                let text = out.to_string();
                assert!(
                    text.contains("target-model"),
                    "{from} → {to} lost the model"
                );
                assert!(
                    text.contains("hi"),
                    "{from} → {to} lost the user's message: {text}"
                );
                assert!(
                    text.contains("ok"),
                    "{from} → {to} lost the tool result: {text}"
                );
                if *from == to {
                    assert!(
                        dropped.is_empty(),
                        "{from} → {to} is the same wire and dropped {dropped:?}"
                    );
                    assert!(text.contains("SECRET") || *from == Protocol::OpenAiChat);
                } else {
                    assert!(
                        !text.contains("SECRET"),
                        "{from} → {to} leaked provider-private state: {text}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_translated_request_goes_to_the_targets_own_endpoint() {
        assert_eq!(endpoint_path(Protocol::AnthropicMessages), "/v1/messages");
        assert_eq!(endpoint_path(Protocol::OpenAiResponses), "/v1/responses");
        assert_eq!(endpoint_path(Protocol::OpenAiChat), "/v1/chat/completions");
    }
}

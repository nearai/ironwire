//! Cross-family protocol translation — IronWire's fallback lane.
//!
//! The native lane forwards bytes and is exact. This crate is what runs when
//! there is no same-family capacity left: it maps an Anthropic Messages request
//! onto OpenAI Chat Completions and maps the answer back, so a Claude Code
//! session can keep working on NEAR AI (or any OpenAI-compatible endpoint)
//! instead of stopping at a rate limit.
//!
//! Two rules shape everything here:
//!
//! 1. **The eligibility decision is not made in this crate.** By the time a
//!    body reaches these functions, [`ironwire_core::capability::eligible`] has
//!    already decided the route is legal. The one cross-family correctness rule
//!    — switch at a turn boundary, never mid tool loop — lives there, because
//!    it is a routing decision rather than a mapping one.
//! 2. **Nothing is dropped silently.** [`request::Dropped`] names what did not
//!    survive so the route can say what it cost.
#![warn(missing_docs)]

pub mod request;
pub mod response;
pub mod stream;
pub mod tool_ids;

pub use request::{
    Dropped, MAX_TOP_LOGPROBS, anthropic_to_chat_completions, anthropic_to_chat_completions_with,
};
pub use response::chat_completion_to_anthropic;
pub use stream::ChatToAnthropicStream;

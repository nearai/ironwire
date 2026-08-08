//! Upstream backends.
//!
//! The native lane's contract (`docs/PROTOCOL.md` §2): for a request whose
//! inbound protocol matches the backend's, IronWire mutates the URL, the auth
//! headers, the hop-by-hop headers and — only when policy changed it — the
//! `model` key. Nothing else. The body is otherwise forwarded as the bytes the
//! client sent, and the response is forwarded back frame-for-frame.
//!
//! That is what makes fidelity 1.0 by construction, including for provider
//! features that did not exist when this shipped: unknown fields survive
//! because nothing here looks at them.
#![warn(missing_docs)]

pub mod anthropic;
pub mod backend;
pub mod breaker;
pub mod headers;
pub mod observe;
pub mod openai_chat;
pub mod openai_responses;
pub mod sse;

pub use backend::{Backend, BackendStatus, UpstreamError, UpstreamRequest, UpstreamResponse};
pub use observe::{Observation, RateLimitReading, UsageReading};

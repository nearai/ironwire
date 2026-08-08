//! Wire protocols, façades, backend identity, and the tier abstraction.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A wire protocol IronWire can speak, on either side.
///
/// Two backends sharing a `Protocol` can serve each other's traffic through the
/// native lane — no translation, no fidelity loss. That property is what the
/// fidelity ladder's rung 2 is built on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Protocol {
    /// Anthropic Messages API (`POST /v1/messages`).
    AnthropicMessages,
    /// OpenAI Responses API (`POST /v1/responses`). What Codex speaks.
    OpenAiResponses,
    /// OpenAI Chat Completions (`POST /v1/chat/completions`).
    OpenAiChat,
}

impl Protocol {
    /// The API family. Used for reporting, not for deciding a route.
    ///
    /// Two protocols can share a family and still be different wires: Responses
    /// and Chat Completions do. Routing on this instead of on the protocol
    /// itself is what once let a Codex Responses body be forwarded to a Chat
    /// Completions backend as though the native lane applied — see
    /// [`Self::translates_to`].
    #[must_use]
    pub fn family(self) -> &'static str {
        match self {
            Self::AnthropicMessages => "anthropic",
            Self::OpenAiResponses | Self::OpenAiChat => "openai",
        }
    }

    /// Whether a request that arrived on `self` can be re-expressed on `other`.
    ///
    /// This is one arm because `ironwire_translate` implements one mapping:
    /// Anthropic Messages onto Chat Completions. Anything else — including
    /// Responses onto Chat Completions, which *looks* like the same family —
    /// has no translator, so a backend speaking it cannot serve the request at
    /// all. Saying so here keeps the routing policy from inventing a lane that
    /// does not exist.
    #[must_use]
    pub fn translates_to(self, other: Self) -> bool {
        matches!((self, other), (Self::AnthropicMessages, Self::OpenAiChat))
    }
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::AnthropicMessages => "anthropic.messages",
            Self::OpenAiResponses => "openai.responses",
            Self::OpenAiChat => "openai.chat",
        };
        f.write_str(s)
    }
}

/// An inbound API surface IronWire presents on loopback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Facade {
    /// `/anthropic/*` — what `ANTHROPIC_BASE_URL` points at.
    Anthropic,
    /// `/openai/*` — what a Codex custom provider points at.
    OpenAi,
}

impl Facade {
    /// URL prefix this façade is mounted under.
    #[must_use]
    pub fn mount_path(self) -> &'static str {
        match self {
            Self::Anthropic => "/anthropic",
            Self::OpenAi => "/openai",
        }
    }
}

/// What kind of capacity a backend draws on. Drives both routing preference
/// (marginal cost) and the consent requirements in `docs/TRUST.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    /// A subscription accessed with a credential the official client stored.
    /// Marginal cost zero, capacity scarce, and gated on explicit consent.
    Subscription,
    /// A metered API key. Unbounded capacity, real per-token cost.
    ApiKey,
    /// NEAR AI credits.
    Credits,
    /// A model running on this machine or LAN. Free, capacity-bounded.
    Local,
}

impl BackendKind {
    /// Whether using this backend requires a recorded consent (`TRUST.md` §2).
    #[must_use]
    pub fn requires_consent(self) -> bool {
        matches!(self, Self::Subscription)
    }

    /// Whether using this backend costs money per token.
    ///
    /// A subscription and local capacity are already paid for; credits are
    /// bought up front, so spending them is not a surprise on a bill.
    #[must_use]
    pub fn is_metered(self) -> bool {
        matches!(self, Self::ApiKey)
    }

    /// Routing preference under equal fidelity: lower sorts first.
    #[must_use]
    pub fn marginal_cost_rank(self) -> u8 {
        match self {
            Self::Local => 0,
            Self::Subscription => 1,
            Self::Credits => 2,
            Self::ApiKey => 3,
        }
    }
}

/// Stable identifier for a configured backend, e.g. `claude-sub`,
/// `anthropic-key`, `codex-sub`, `nearai`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BackendId(pub String);

impl BackendId {
    /// Borrow the identifier as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for BackendId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Quality tier. The client's model string is a *hint* that maps to a tier, not
/// a selection — Claude Code cannot type `ironwire/auto` (`docs/CRITIQUE.md` §5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTier {
    /// Cheap and quick: haiku-class, gpt-5-mini-class.
    Fast,
    /// The everyday workhorse: sonnet-class.
    Balanced,
    /// Best available: opus-class, gpt-5-class with high reasoning.
    Frontier,
}

impl ModelTier {
    /// Map a client-supplied model slug to the tier it is asking for.
    ///
    /// Unknown slugs resolve to [`ModelTier::Frontier`]: guessing *low* on an
    /// unrecognised model silently downgrades the user's work, which is the one
    /// failure mode we cannot let them discover after the fact.
    #[must_use]
    pub fn from_model_hint(model: &str) -> Self {
        let m = model.to_ascii_lowercase();
        if m.contains("haiku") || m.contains("mini") || m.contains("flash") || m.contains("small") {
            Self::Fast
        } else if m.contains("sonnet") || m.contains("gpt-4o") || m.contains("medium") {
            Self::Balanced
        } else {
            Self::Frontier
        }
    }
}

impl fmt::Display for ModelTier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Frontier => "frontier",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_family_protocols_share_a_family_tag() {
        assert_eq!(
            Protocol::OpenAiResponses.family(),
            Protocol::OpenAiChat.family()
        );
        assert_ne!(
            Protocol::AnthropicMessages.family(),
            Protocol::OpenAiChat.family()
        );
    }

    #[test]
    fn unknown_model_hints_resolve_upward_not_downward() {
        // The safe default is to over-serve. Under-serving is invisible.
        assert_eq!(
            ModelTier::from_model_hint("some-future-model-v9"),
            ModelTier::Frontier
        );
        assert_eq!(
            ModelTier::from_model_hint("claude-haiku-4-5"),
            ModelTier::Fast
        );
        assert_eq!(
            ModelTier::from_model_hint("claude-sonnet-4-6"),
            ModelTier::Balanced
        );
        assert_eq!(
            ModelTier::from_model_hint("claude-opus-4-6"),
            ModelTier::Frontier
        );
    }

    #[test]
    fn only_subscriptions_require_consent() {
        assert!(BackendKind::Subscription.requires_consent());
        assert!(!BackendKind::ApiKey.requires_consent());
        assert!(!BackendKind::Credits.requires_consent());
        assert!(!BackendKind::Local.requires_consent());
    }

    #[test]
    fn free_capacity_outranks_metered() {
        assert!(
            BackendKind::Subscription.marginal_cost_rank()
                < BackendKind::ApiKey.marginal_cost_rank()
        );
    }
}

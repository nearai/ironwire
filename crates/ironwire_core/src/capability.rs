//! The capability gate.
//!
//! IronWire routes a request to a backend **only if that backend can preserve
//! the request's semantics**. Anything less is a refusal, not a downgrade
//! (`docs/DESIGN.md` §2). This module is where that rule is enforced, and it is
//! deliberately the smallest, most testable thing in the codebase: everything
//! about routing quality is a heuristic, but everything here is a hard fact
//! about what the wire can carry.

use serde::{Deserialize, Serialize};

use crate::protocol::Protocol;

/// How much the request depends on model reasoning state.
///
/// The distinction that matters is [`ReasoningNeed::LoadBearing`]: once a
/// conversation carries *signed* Anthropic thinking blocks or *encrypted*
/// OpenAI reasoning items, that state cannot be reproduced by any other
/// provider. It is cryptography, not a mapping gap, so no amount of translator
/// work will ever make a cross-family route legal for that conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReasoningNeed {
    /// No reasoning requested.
    #[default]
    None,
    /// Reasoning requested for this turn, but nothing in the history depends
    /// on provider-private state. Translatable to an effort knob.
    Requested,
    /// The conversation history contains signed or encrypted provider reasoning
    /// state that will be replayed. Cross-family routes are permanently
    /// ineligible.
    LoadBearing,
}

/// What a request needs a backend to preserve. Built by [`crate::peek`] from a
/// bounded scan of the body — never by re-serialising it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RequestRequirements {
    /// Tool definitions are present.
    pub tools: bool,
    /// The client permits (or expects) more than one tool call per turn.
    pub parallel_tool_calls: bool,
    /// Image content blocks are present.
    pub images: bool,
    /// Reasoning dependency; see [`ReasoningNeed`].
    pub reasoning: ReasoningNeed,
    /// `cache_control` breakpoints are present.
    pub prompt_cache: bool,
    /// Approximate size of the cacheable prefix, in tokens. Used to decide
    /// whether losing the cache is survivable.
    pub cached_prefix_tokens: u32,
    /// A strict JSON schema response format is requested.
    pub structured_output: bool,
    /// Minimum usable context window, in tokens.
    pub min_context_tokens: u32,
}

/// What a backend+model can preserve.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Capabilities {
    /// Protocol this backend speaks natively.
    pub protocol: Protocol,
    /// Tool calling.
    pub tools: bool,
    /// More than one tool call per assistant turn.
    pub parallel_tool_calls: bool,
    /// Image inputs.
    pub images: bool,
    /// Any form of reasoning/thinking.
    pub reasoning: bool,
    /// Prompt caching with explicit breakpoints.
    pub prompt_cache: bool,
    /// Strict structured output.
    pub structured_output: bool,
    /// Context window in tokens.
    pub context_tokens: u32,
}

impl Capabilities {
    /// A conservative unknown-backend baseline: tools and images, no reasoning,
    /// no cache, 128k context. Used only for user-configured OpenAI-compatible
    /// endpoints we have never seen.
    #[must_use]
    pub fn conservative(protocol: Protocol) -> Self {
        Self {
            protocol,
            tools: true,
            parallel_tool_calls: false,
            images: false,
            reasoning: false,
            prompt_cache: false,
            structured_output: false,
            context_tokens: 128_000,
        }
    }
}

/// Why a route was refused. Carried into logs, the control API, and the rung-3
/// announcement so a user can always find out *why* their agent is not on the
/// model they expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ineligible {
    /// The conversation replays signed/encrypted reasoning state.
    LoadBearingReasoning,
    /// Losing a large warm prefix would cost more than the route saves.
    WouldDiscardLargePromptCache,
    /// Request uses tools; backend has none.
    ToolsUnsupported,
    /// Request sends more than one tool call per turn; backend is serial.
    ParallelToolsUnsupported,
    /// Request carries images; backend is text-only.
    ImagesUnsupported,
    /// Request needs reasoning; backend has none.
    ReasoningUnsupported,
    /// Request needs a strict schema; backend cannot guarantee one.
    StructuredOutputUnsupported,
    /// Prompt does not fit.
    ContextTooSmall,
}

/// The tokens-of-warm-cache threshold above which discarding the cache makes a
/// cross-family route a net loss.
///
/// Below this, a cold start is cheap enough that the route is worth taking.
/// Above it, re-priming costs more (in money and in latency) than staying put
/// and waiting out a short rate-limit window — which is the whole reason
/// fallback has hysteresis (`docs/CRITIQUE.md` §1).
pub const CACHE_SACRIFICE_THRESHOLD_TOKENS: u32 = 4_000;

/// Decide whether `caps` can serve `req` without losing semantics.
///
/// `cross_family` is true when taking this route means translating between API
/// families. Some refusals apply only then: dropping a prompt cache is a cost
/// we accept within a family (rung 2) but not across one (rung 3), because
/// crossing families already sacrifices reasoning continuity.
///
/// Returns `Ok(())` when the route preserves the request, or the first reason
/// it does not.
///
/// # Errors
///
/// Returns [`Ineligible`] describing the first unmet requirement.
pub fn eligible(
    req: &RequestRequirements,
    caps: &Capabilities,
    cross_family: bool,
) -> Result<(), Ineligible> {
    // Hard, permanent, and independent of how good the translator gets.
    if cross_family && req.reasoning == ReasoningNeed::LoadBearing {
        return Err(Ineligible::LoadBearingReasoning);
    }
    if req.tools && !caps.tools {
        return Err(Ineligible::ToolsUnsupported);
    }
    if req.parallel_tool_calls && !caps.parallel_tool_calls {
        return Err(Ineligible::ParallelToolsUnsupported);
    }
    if req.images && !caps.images {
        return Err(Ineligible::ImagesUnsupported);
    }
    if req.reasoning != ReasoningNeed::None && !caps.reasoning {
        return Err(Ineligible::ReasoningUnsupported);
    }
    if req.structured_output && !caps.structured_output {
        return Err(Ineligible::StructuredOutputUnsupported);
    }
    if req.min_context_tokens > caps.context_tokens {
        return Err(Ineligible::ContextTooSmall);
    }
    // Economic rather than semantic, but it belongs in the same gate: a route
    // that "works" while costing the user 10x is not a route we should take
    // silently.
    if cross_family
        && req.prompt_cache
        && !caps.prompt_cache
        && req.cached_prefix_tokens > CACHE_SACRIFICE_THRESHOLD_TOKENS
    {
        return Err(Ineligible::WouldDiscardLargePromptCache);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_caps() -> Capabilities {
        Capabilities {
            protocol: Protocol::AnthropicMessages,
            tools: true,
            parallel_tool_calls: true,
            images: true,
            reasoning: true,
            prompt_cache: true,
            structured_output: true,
            context_tokens: 200_000,
        }
    }

    #[test]
    fn load_bearing_reasoning_is_permanently_cross_family_ineligible() {
        let req = RequestRequirements {
            reasoning: ReasoningNeed::LoadBearing,
            ..Default::default()
        };
        // Even against a backend that supports everything.
        assert_eq!(
            eligible(&req, &full_caps(), true),
            Err(Ineligible::LoadBearingReasoning)
        );
        // But staying inside the family is fine — the state round-trips.
        assert_eq!(eligible(&req, &full_caps(), false), Ok(()));
    }

    #[test]
    fn large_warm_cache_blocks_cross_family_but_not_same_family() {
        let req = RequestRequirements {
            prompt_cache: true,
            cached_prefix_tokens: 120_000,
            ..Default::default()
        };
        let no_cache = Capabilities {
            prompt_cache: false,
            ..full_caps()
        };
        assert_eq!(
            eligible(&req, &no_cache, true),
            Err(Ineligible::WouldDiscardLargePromptCache)
        );
        assert_eq!(eligible(&req, &no_cache, false), Ok(()));
    }

    #[test]
    fn small_cache_is_worth_sacrificing() {
        let req = RequestRequirements {
            prompt_cache: true,
            cached_prefix_tokens: 500,
            ..Default::default()
        };
        let no_cache = Capabilities {
            prompt_cache: false,
            ..full_caps()
        };
        assert_eq!(eligible(&req, &no_cache, true), Ok(()));
    }

    #[test]
    fn missing_modalities_are_refusals_not_downgrades() {
        let req = RequestRequirements {
            images: true,
            ..Default::default()
        };
        let text_only = Capabilities {
            images: false,
            ..full_caps()
        };
        assert_eq!(
            eligible(&req, &text_only, false),
            Err(Ineligible::ImagesUnsupported)
        );
    }

    #[test]
    fn context_must_actually_fit() {
        let req = RequestRequirements {
            min_context_tokens: 400_000,
            ..Default::default()
        };
        assert_eq!(
            eligible(&req, &full_caps(), false),
            Err(Ineligible::ContextTooSmall)
        );
    }

    #[test]
    fn an_empty_request_fits_a_conservative_backend() {
        let req = RequestRequirements::default();
        let caps = Capabilities::conservative(Protocol::OpenAiChat);
        assert_eq!(eligible(&req, &caps, true), Ok(()));
    }
}

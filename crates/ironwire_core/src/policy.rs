//! Routing policy: the fidelity ladder, conversation affinity, and hysteresis.
//!
//! The decision this module makes is *per conversation*, not per request. A
//! coding agent's conversation carries a large warm prompt cache and, often,
//! provider-private reasoning state; moving it costs real money and real
//! latency, and moving it across API families can cost correctness
//! (`docs/CRITIQUE.md` §1). So a route change is a state transition taken under
//! sustained pressure, not a fresh choice made 200 times an hour.

use std::collections::HashMap;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::capability::{Capabilities, Ineligible, RequestRequirements, eligible};
use crate::peek::RequestPeek;
use crate::protocol::{BackendId, BackendKind, ModelTier, Protocol};
use crate::quota::QuotaSnapshot;

/// How far down the fidelity ladder a route sits, relative to the ideal.
///
/// Rungs 0–2 are silent because nothing the user can observe changes. Rung 3
/// changes how their agent behaves, so it is announced — pretending otherwise
/// is how we lose their trust the first time it matters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rung {
    /// Preferred backend, preferred model. Cache warm, reasoning intact.
    Preferred,
    /// Same account, smaller model. Cache mostly warm, reasoning intact.
    SmallerModel,
    /// Same wire format, different credential. Cache cold, reasoning intact,
    /// zero translation.
    AlternateCredential,
    /// Different API family. Cache cold, reasoning dropped, translation
    /// required. The user is told.
    CrossFamily,
}

impl Rung {
    /// Whether descending to this rung is worth surfacing to the user.
    #[must_use]
    pub fn is_user_visible(self) -> bool {
        self == Self::CrossFamily
    }
}

/// Opaque per-conversation identity derived from request content.
///
/// IronWire gets no session ID from the client, so it derives one from the
/// stable head of the conversation. Keys are memory-only and never persisted
/// alongside content.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ConversationKey(pub u64);

impl ConversationKey {
    /// Derive a key from the parts of a request that stay fixed as a
    /// conversation grows: the façade, the system preamble and the tool set.
    ///
    /// Deliberately *not* including the message list — that changes every turn,
    /// and a key that changes every turn is not affinity, it is noise.
    #[must_use]
    pub fn derive(protocol: Protocol, system_prefix: &str, tool_names: &[&str]) -> Self {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut hasher = DefaultHasher::new();
        protocol.family().hash(&mut hasher);
        // Bound the prefix: system prompts embed a live timestamp and a working
        // directory in some clients, and hashing the whole thing would split
        // one conversation into many.
        system_prefix
            .as_bytes()
            .iter()
            .take(512)
            .for_each(|b| b.hash(&mut hasher));
        let mut names: Vec<&str> = tool_names.to_vec();
        names.sort_unstable();
        names.hash(&mut hasher);
        Self(hasher.finish())
    }
}

/// A candidate backend, as the router sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Which backend.
    pub id: BackendId,
    /// Capacity kind, for marginal-cost preference.
    pub kind: BackendKind,
    /// What it can preserve.
    pub caps: Capabilities,
    /// Observed capacity state.
    pub quota: QuotaSnapshot,
    /// Whether the credential is currently usable.
    pub healthy: bool,
    /// Whether the user has consented to this backend, where consent is
    /// required (`docs/TRUST.md` §2).
    pub consented: bool,
    /// Whether this backend requires the inbound request to carry the
    /// originating product's client identity. True for subscription backends;
    /// see `docs/TRUST.md` §3.
    pub requires_client_identity: bool,
    /// Models this backend offers, best-first, with the tier each satisfies.
    pub models: Vec<(String, ModelTier)>,
}

/// The router's answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RouteDecision {
    /// Chosen backend.
    pub backend: BackendId,
    /// Model to request from it. `None` means forward the client's choice
    /// untouched — the native lane's preferred case, since it means the body
    /// needs no edit at all.
    pub model: Option<String>,
    /// How far down the ladder this sits.
    pub rung: Rung,
    /// Whether serving this needs protocol translation.
    pub translated: bool,
    /// Short reason, for logs and the control API.
    pub reason: String,
}

/// Why no route was possible. Every variant is something we can explain to the
/// user in one sentence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum NoRoute {
    /// Nothing is configured for this façade.
    NoBackendsConfigured,
    /// Every backend is rate-limited or unhealthy.
    AllExhausted,
    /// Backends exist but none can preserve the request's semantics.
    AllIneligible {
        /// Per-backend refusal reasons.
        reasons: Vec<(BackendId, Ineligible)>,
    },
    /// The only backends that could serve this need a client identity the
    /// request does not carry.
    RequiresClientIdentity,
}

/// Sticky per-conversation route state.
#[derive(Debug, Clone)]
pub struct Affinity {
    /// Where this conversation is pinned.
    pub backend: BackendId,
    /// Model in use.
    pub model: Option<String>,
    /// Rung it settled on.
    pub rung: Rung,
    /// When the affinity was established.
    pub since: DateTime<Utc>,
    /// First time we saw sustained pressure on the current backend. Cleared
    /// when pressure lifts; a descent needs this to be older than the debounce.
    pub pressure_since: Option<DateTime<Utc>>,
}

/// How long pressure must persist before a conversation descends a rung.
///
/// A single 429 with a three-second `retry-after` must not throw away a
/// 200k-token warm cache. Waiting is almost always cheaper than moving.
pub const DESCENT_DEBOUNCE: Duration = Duration::seconds(20);

/// Routing policy over a set of candidate backends.
#[derive(Debug, Default)]
pub struct Policy {
    affinities: HashMap<ConversationKey, Affinity>,
    /// User-forced backend/model (`ironwire pin`). Overrides everything except
    /// eligibility — we will not serve a pin that would corrupt the request.
    pin: Option<(BackendId, Option<String>)>,
}

impl Policy {
    /// New policy with no affinities and no pin.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force all conversations onto a backend (and optionally a model).
    pub fn set_pin(&mut self, backend: Option<BackendId>, model: Option<String>) {
        self.pin = backend.map(|b| (b, model));
    }

    /// Current pin, if any.
    #[must_use]
    pub fn pin(&self) -> Option<&(BackendId, Option<String>)> {
        self.pin.as_ref()
    }

    /// Forget a conversation's affinity.
    pub fn forget(&mut self, key: &ConversationKey) {
        self.affinities.remove(key);
    }

    /// Number of conversations currently tracked.
    #[must_use]
    pub fn tracked_conversations(&self) -> usize {
        self.affinities.len()
    }

    /// Choose where a request goes.
    ///
    /// `inbound` is the façade's protocol; a candidate speaking a different
    /// family means translation, and is only reachable at [`Rung::CrossFamily`].
    ///
    /// # Errors
    ///
    /// Returns [`NoRoute`] when nothing can serve the request, with a reason
    /// specific enough to show the user.
    pub fn decide(
        &mut self,
        key: ConversationKey,
        inbound: Protocol,
        peek: &RequestPeek,
        candidates: &[Candidate],
        now: DateTime<Utc>,
    ) -> Result<RouteDecision, NoRoute> {
        if candidates.is_empty() {
            return Err(NoRoute::NoBackendsConfigured);
        }

        let tier = peek
            .requested_model
            .as_deref()
            .map_or(ModelTier::Frontier, ModelTier::from_model_hint);

        // A pin bypasses preference but not eligibility: an unusable route is
        // still unusable, and silently corrupting a request because the user
        // asked for a backend is not obedience, it is a bug.
        if let Some((pinned, model)) = self.pin.clone()
            && let Some(candidate) = candidates.iter().find(|c| c.id == pinned)
        {
            let cross = candidate.caps.protocol.family() != inbound.family();
            eligible(&peek.requirements, &candidate.caps, cross).map_err(|why| {
                NoRoute::AllIneligible {
                    reasons: vec![(candidate.id.clone(), why)],
                }
            })?;
            return Ok(RouteDecision {
                backend: candidate.id.clone(),
                model: model.or_else(|| pick_model(candidate, tier)),
                rung: if cross {
                    Rung::CrossFamily
                } else {
                    Rung::Preferred
                },
                translated: cross,
                reason: "pinned by user".to_string(),
            });
        }

        // Sticky: if this conversation already has a home and that home is
        // still fine, stay. Moving costs a cache; staying costs nothing.
        if let Some(affinity) = self.affinities.get(&key).cloned()
            && let Some(candidate) = candidates.iter().find(|c| c.id == affinity.backend)
            && usable(candidate, peek, inbound, now).is_ok()
        {
            let pressured = candidate.quota.is_pressured(now);
            let entry = self.affinities.get_mut(&key).expect("just found above");
            if pressured {
                let since = *entry.pressure_since.get_or_insert(now);
                // Hysteresis: only descend once pressure has persisted.
                if now - since < DESCENT_DEBOUNCE {
                    return Ok(RouteDecision {
                        backend: affinity.backend,
                        model: affinity.model,
                        rung: affinity.rung,
                        translated: affinity.rung == Rung::CrossFamily,
                        reason: "sticky: pressure not yet sustained".to_string(),
                    });
                }
            } else {
                entry.pressure_since = None;
                return Ok(RouteDecision {
                    backend: affinity.backend,
                    model: affinity.model,
                    rung: affinity.rung,
                    translated: affinity.rung == Rung::CrossFamily,
                    reason: "sticky affinity".to_string(),
                });
            }
        }

        let decision = self.select(inbound, peek, tier, candidates, now)?;
        self.affinities.insert(
            key,
            Affinity {
                backend: decision.backend.clone(),
                model: decision.model.clone(),
                rung: decision.rung,
                since: now,
                pressure_since: None,
            },
        );
        Ok(decision)
    }

    /// Fresh selection, ignoring affinity. Walks the ladder rung by rung.
    fn select(
        &self,
        inbound: Protocol,
        peek: &RequestPeek,
        tier: ModelTier,
        candidates: &[Candidate],
        now: DateTime<Utc>,
    ) -> Result<RouteDecision, NoRoute> {
        let mut ineligible = Vec::new();
        let mut identity_blocked = false;
        let mut any_available = false;

        // Rungs 0-2 are all "same family"; rung 3 is "different family". Within
        // same-family we prefer free capacity, then exact tier, then anything.
        let mut same_family: Vec<&Candidate> = Vec::new();
        let mut cross_family: Vec<&Candidate> = Vec::new();

        for candidate in candidates {
            match usable(candidate, peek, inbound, now) {
                Ok(()) => {}
                Err(Unusable::Ineligible(why)) => {
                    ineligible.push((candidate.id.clone(), why));
                    continue;
                }
                Err(Unusable::NeedsClientIdentity) => {
                    identity_blocked = true;
                    continue;
                }
                Err(Unusable::Unavailable) => continue,
            }
            any_available = true;
            if candidate.caps.protocol.family() == inbound.family() {
                same_family.push(candidate);
            } else {
                cross_family.push(candidate);
            }
        }

        let sort_key = |c: &&Candidate| {
            (
                // Prefer un-pressured capacity...
                u8::from(c.quota.is_pressured(now)),
                // ...then free over metered...
                c.kind.marginal_cost_rank(),
                // ...then a backend that can actually serve the tier.
                u8::from(pick_model(c, tier).is_none()),
            )
        };
        same_family.sort_by_key(sort_key);
        cross_family.sort_by_key(sort_key);

        if let Some(best) = same_family.first() {
            let model = pick_model(best, tier);
            let served_tier = model.as_deref().map_or(tier, ModelTier::from_model_hint);
            // Same credential, lesser model is rung 1; a different credential
            // that speaks the same wire is rung 2.
            let rung = if served_tier < tier {
                Rung::SmallerModel
            } else {
                Rung::Preferred
            };
            return Ok(RouteDecision {
                backend: best.id.clone(),
                // Forward the client's own model string when the backend
                // already offers exactly it: no body edit, no risk.
                model: model.filter(|m| Some(m.as_str()) != peek.requested_model.as_deref()),
                rung,
                translated: false,
                reason: format!("native lane, {} capacity", kind_label(best.kind)),
            });
        }

        if let Some(best) = cross_family.first() {
            return Ok(RouteDecision {
                backend: best.id.clone(),
                model: pick_model(best, tier),
                rung: Rung::CrossFamily,
                translated: true,
                reason: "no same-family capacity available".to_string(),
            });
        }

        if !ineligible.is_empty() {
            return Err(NoRoute::AllIneligible {
                reasons: ineligible,
            });
        }
        if identity_blocked && !any_available {
            return Err(NoRoute::RequiresClientIdentity);
        }
        Err(NoRoute::AllExhausted)
    }
}

/// Why a candidate cannot serve this request right now.
enum Unusable {
    Unavailable,
    NeedsClientIdentity,
    Ineligible(Ineligible),
}

fn usable(
    candidate: &Candidate,
    peek: &RequestPeek,
    inbound: Protocol,
    now: DateTime<Utc>,
) -> Result<(), Unusable> {
    if !candidate.healthy || !candidate.quota.is_available(now) {
        return Err(Unusable::Unavailable);
    }
    if candidate.kind.requires_consent() && !candidate.consented {
        return Err(Unusable::Unavailable);
    }
    // TRUST.md §3: we serve a subscription only for the client it belongs to,
    // and we never synthesize that client's identity to unlock it.
    if candidate.requires_client_identity && !peek.carries_client_identity {
        return Err(Unusable::NeedsClientIdentity);
    }
    let cross = candidate.caps.protocol.family() != inbound.family();
    eligible(&peek.requirements, &candidate.caps, cross).map_err(Unusable::Ineligible)
}

/// Best model this candidate offers at or below `tier`, preferring an exact
/// match. `None` when the backend advertises no models (forward as-is).
fn pick_model(candidate: &Candidate, tier: ModelTier) -> Option<String> {
    candidate
        .models
        .iter()
        .find(|(_, t)| *t == tier)
        .or_else(|| {
            candidate
                .models
                .iter()
                .filter(|(_, t)| *t < tier)
                .max_by_key(|(_, t)| *t)
        })
        .map(|(m, _)| m.clone())
}

fn kind_label(kind: BackendKind) -> &'static str {
    match kind {
        BackendKind::Subscription => "subscription",
        BackendKind::ApiKey => "metered",
        BackendKind::Credits => "credit",
        BackendKind::Local => "local",
    }
}

/// Convenience: requirements that demand nothing.
#[must_use]
pub fn trivial_requirements() -> RequestRequirements {
    RequestRequirements::default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::ReasoningNeed;
    use crate::quota::Headroom;

    fn t(offset: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000 + offset, 0).expect("valid timestamp")
    }

    fn caps(protocol: Protocol) -> Capabilities {
        Capabilities {
            protocol,
            tools: true,
            parallel_tool_calls: true,
            images: true,
            reasoning: true,
            prompt_cache: true,
            structured_output: true,
            context_tokens: 200_000,
        }
    }

    fn candidate(id: &str, kind: BackendKind, protocol: Protocol) -> Candidate {
        Candidate {
            id: BackendId::from(id),
            kind,
            caps: caps(protocol),
            quota: QuotaSnapshot::default(),
            healthy: true,
            consented: true,
            requires_client_identity: false,
            models: vec![
                ("claude-opus-4-6".to_string(), ModelTier::Frontier),
                ("claude-sonnet-4-6".to_string(), ModelTier::Balanced),
            ],
        }
    }

    fn peek(model: &str) -> RequestPeek {
        RequestPeek {
            requested_model: Some(model.to_string()),
            stream: true,
            requirements: RequestRequirements::default(),
            carries_client_identity: true,
            message_count: 3,
        }
    }

    fn key() -> ConversationKey {
        ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are Claude Code",
            &["Read"],
        )
    }

    #[test]
    fn prefers_free_capacity_over_metered() {
        let mut policy = Policy::new();
        let candidates = vec![
            candidate(
                "anthropic-key",
                BackendKind::ApiKey,
                Protocol::AnthropicMessages,
            ),
            candidate(
                "claude-sub",
                BackendKind::Subscription,
                Protocol::AnthropicMessages,
            ),
        ];
        let d = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &candidates,
                t(0),
            )
            .expect("a route exists");
        assert_eq!(d.backend.as_str(), "claude-sub");
        assert_eq!(d.rung, Rung::Preferred);
        assert!(!d.translated);
    }

    #[test]
    fn forwards_the_clients_model_untouched_when_the_backend_has_it() {
        // No body edit is the safest possible native-lane request.
        let mut policy = Policy::new();
        let candidates = vec![candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        )];
        let d = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &candidates,
                t(0),
            )
            .expect("a route exists");
        assert_eq!(d.model, None);
    }

    #[test]
    fn affinity_survives_a_brief_rate_limit_rather_than_dumping_the_cache() {
        let mut policy = Policy::new();
        let mut sub = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        let fallback = candidate(
            "anthropic-key",
            BackendKind::ApiKey,
            Protocol::AnthropicMessages,
        );

        let first = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[sub.clone(), fallback.clone()],
                t(0),
            )
            .expect("a route exists");
        assert_eq!(first.backend.as_str(), "claude-sub");

        // Pressure appears; within the debounce we must stay put.
        sub.quota.primary = Headroom::Observed {
            used_pct: 97.0,
            resets_at: None,
            observed_at: t(1),
        };
        let during = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[sub.clone(), fallback.clone()],
                t(5),
            )
            .expect("a route exists");
        assert_eq!(during.backend.as_str(), "claude-sub");

        // Sustained pressure past the debounce descends to the API key.
        let after = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[sub, fallback],
                t(40),
            )
            .expect("a route exists");
        assert_eq!(after.backend.as_str(), "anthropic-key");
    }

    #[test]
    fn pressure_that_lifts_resets_the_debounce() {
        let mut policy = Policy::new();
        let mut sub = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        let fallback = candidate(
            "anthropic-key",
            BackendKind::ApiKey,
            Protocol::AnthropicMessages,
        );
        let all = |s: &Candidate| vec![s.clone(), fallback.clone()];

        policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &all(&sub),
                t(0),
            )
            .expect("route");
        sub.quota.primary = Headroom::Observed {
            used_pct: 95.0,
            resets_at: None,
            observed_at: t(1),
        };
        policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &all(&sub),
                t(5),
            )
            .expect("route");
        // Pressure lifts.
        sub.quota.primary = Headroom::Observed {
            used_pct: 40.0,
            resets_at: None,
            observed_at: t(10),
        };
        policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &all(&sub),
                t(10),
            )
            .expect("route");
        // Returns; the clock must have restarted, so we stay put at t=25.
        sub.quota.primary = Headroom::Observed {
            used_pct: 95.0,
            resets_at: None,
            observed_at: t(20),
        };
        let d = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &all(&sub),
                t(25),
            )
            .expect("route");
        assert_eq!(d.backend.as_str(), "claude-sub");
    }

    #[test]
    fn a_subscription_is_not_unlocked_for_a_client_that_is_not_its_own() {
        // TRUST.md §3 — Aider does not get to be Claude Code.
        let mut policy = Policy::new();
        let mut sub = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        sub.requires_client_identity = true;
        let mut third_party = peek("claude-opus-4-6");
        third_party.carries_client_identity = false;

        let err = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &third_party,
                &[sub],
                t(0),
            )
            .expect_err("subscription must be refused");
        assert_eq!(err, NoRoute::RequiresClientIdentity);
    }

    #[test]
    fn cross_family_is_a_last_resort_and_is_flagged() {
        let mut policy = Policy::new();
        let mut exhausted = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        exhausted.quota.primary = Headroom::Exhausted { until: t(3600) };
        let near = candidate("nearai", BackendKind::Credits, Protocol::OpenAiChat);

        let d = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[exhausted, near],
                t(0),
            )
            .expect("a route exists");
        assert_eq!(d.backend.as_str(), "nearai");
        assert_eq!(d.rung, Rung::CrossFamily);
        assert!(d.translated);
        assert!(d.rung.is_user_visible());
    }

    #[test]
    fn load_bearing_reasoning_refuses_cross_family_entirely() {
        let mut policy = Policy::new();
        let mut exhausted = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        exhausted.quota.primary = Headroom::Exhausted { until: t(3600) };
        let near = candidate("nearai", BackendKind::Credits, Protocol::OpenAiChat);

        let mut p = peek("claude-opus-4-6");
        p.requirements.reasoning = ReasoningNeed::LoadBearing;

        let err = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &p,
                &[exhausted, near],
                t(0),
            )
            .expect_err("cross-family must be refused");
        match err {
            NoRoute::AllIneligible { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|(_, why)| *why == Ineligible::LoadBearingReasoning)
                );
            }
            other => panic!("expected ineligibility, got {other:?}"),
        }
    }

    #[test]
    fn a_pin_still_cannot_corrupt_a_request() {
        let mut policy = Policy::new();
        let mut near = candidate("nearai", BackendKind::Credits, Protocol::OpenAiChat);
        near.caps.images = false;
        policy.set_pin(Some(BackendId::from("nearai")), None);

        let mut p = peek("claude-opus-4-6");
        p.requirements.images = true;

        let err = policy
            .decide(key(), Protocol::AnthropicMessages, &p, &[near], t(0))
            .expect_err("pin must not override eligibility");
        assert!(matches!(err, NoRoute::AllIneligible { .. }));
    }

    #[test]
    fn conversation_key_is_stable_as_the_conversation_grows() {
        // Same system prompt and tools, different turn counts: one key.
        let a = ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are Claude Code",
            &["Read", "Bash"],
        );
        let b = ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are Claude Code",
            &["Bash", "Read"],
        );
        assert_eq!(a, b, "tool order must not split a conversation");

        let other = ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are a helpful bot",
            &["Read", "Bash"],
        );
        assert_ne!(a, other);
    }

    #[test]
    fn no_backends_is_reported_as_such() {
        let mut policy = Policy::new();
        let err = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[],
                t(0),
            )
            .expect_err("no candidates");
        assert_eq!(err, NoRoute::NoBackendsConfigured);
    }

    #[test]
    fn an_unconsented_subscription_is_never_used() {
        let mut policy = Policy::new();
        let mut sub = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        sub.consented = false;
        let err = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[sub],
                t(0),
            )
            .expect_err("unconsented backend must not be routed to");
        assert_eq!(err, NoRoute::AllExhausted);
    }
}

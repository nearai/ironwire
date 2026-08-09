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
    /// Whether [`Self::models`] came from the provider rather than from a list
    /// compiled into this binary.
    ///
    /// The difference decides what an unknown model means. Against a list the
    /// provider gave us, "not in the catalogue" means the backend genuinely
    /// cannot serve it, and descending to something it can is correct. Against
    /// a compiled-in guess it means only that this build is older than the
    /// model — and substituting our newest known name for the client's is how
    /// `claude-opus-5` became `claude-opus-4-6` and failed.
    pub catalogue_from_provider: bool,
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
    /// Every backend that could have served this is stopped by a spend cap the
    /// user set.
    ///
    /// Distinct from [`Self::AllExhausted`] on purpose: a cap the user set and
    /// then cannot recognise in the error is worse than no cap, so the message
    /// names their own number and the key they set it with.
    SpendCapReached {
        /// A backend that was capped, for the message.
        backend: String,
        /// Spent against the cap.
        spent_usd: f64,
        /// The cap.
        cap_usd: f64,
    },
    /// An `X-IronWire-Route` header named a backend that does not exist.
    ///
    /// An error rather than a fall-through: the caller asked for something
    /// specific, and quietly serving them from somewhere else would be worse
    /// than saying we could not.
    UnknownRoute {
        /// What they asked for.
        requested: String,
        /// What exists, so the answer is actionable.
        available: Vec<String>,
    },
}

/// One turn's routing inputs, travelling together because they always do.
#[derive(Clone, Copy)]
struct Turn<'a> {
    inbound: Protocol,
    peek: &'a RequestPeek,
    tier: ModelTier,
    candidates: &'a [Candidate],
    now: DateTime<Utc>,
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
    /// Last time a request actually routed under this affinity.
    ///
    /// Distinct from [`Self::since`], which is set once and never moves. Using
    /// `since` to decide what to evict would throw out the longest-lived
    /// healthy sessions first — exactly backwards, and invisible in a test
    /// short enough that the two never diverge.
    pub last_seen: DateTime<Utc>,
    /// First time we saw sustained pressure on the current backend. Cleared
    /// when pressure lifts; a descent needs this to be older than the debounce.
    pub pressure_since: Option<DateTime<Utc>>,
    /// First time a better rung was available again. The mirror of
    /// [`Self::pressure_since`], cleared whenever pressure returns, and a
    /// promotion needs it older than [`PROMOTION_DEBOUNCE`].
    pub recovery_since: Option<DateTime<Utc>>,
}

/// How long pressure must persist before a conversation descends a rung.
///
/// A single 429 with a three-second `retry-after` must not throw away a
/// 200k-token warm cache. Waiting is almost always cheaper than moving.
pub const DESCENT_DEBOUNCE: Duration = Duration::seconds(20);

/// How long a better rung must stay available before a conversation climbs back
/// to it.
///
/// Deliberately far longer than [`DESCENT_DEBOUNCE`], and the asymmetry is the
/// design rather than an oversight. Descending is urgent: the alternative is a
/// failed turn. Promoting is not, because the conversation is working. And
/// `Headroom::is_pressured` is a step function at 90% with no hysteresis band
/// of its own, so this debounce is the only thing standing between a provider
/// hovering at that threshold and a conversation that discards a warm prompt
/// cache every twenty seconds — which would be strictly worse than never
/// promoting at all.
pub const PROMOTION_DEBOUNCE: Duration = Duration::minutes(5);

/// How long a conversation may go unheard from before its route is forgotten.
///
/// A coding session is minutes to hours; a key untouched for a day is a session
/// that ended. Forgetting is cheap and safe — the next request re-selects, and
/// `select` is deterministic given the same candidates, so under unchanged
/// conditions it lands on the same backend and pays one cold prompt cache.
pub const AFFINITY_TTL: Duration = Duration::hours(24);

/// Hard ceiling on tracked conversations, regardless of age.
///
/// The same number, for the same reason, as `PrivacyFilter::MAX_SALTS`: a
/// daemon meant to run under launchd or systemd for weeks must not accumulate
/// state for its whole life, and the thing being dropped costs one cold cache.
pub const MAX_AFFINITIES: usize = 512;

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

    /// Where a conversation is currently pinned, if anywhere.
    ///
    /// Read *before* deciding, so the caller can tell a genuine route change
    /// from a request that simply stayed put. Announcing every request as a
    /// "route" would drown the one announcement that matters.
    #[must_use]
    pub fn current_backend(&self, key: &ConversationKey) -> Option<BackendId> {
        self.affinities.get(key).map(|a| a.backend.clone())
    }

    /// Forget a conversation's affinity.
    pub fn forget(&mut self, key: &ConversationKey) {
        self.affinities.remove(key);
    }

    /// Number of conversations currently tracked: those routed within
    /// [`AFFINITY_TTL`], capped at [`MAX_AFFINITIES`].
    ///
    /// A description of the present, which is what `ironwire status` renders it
    /// as. Before the map was bounded this was a lifetime counter that only
    /// ever went up, and meant nothing after a day of use.
    #[must_use]
    pub fn tracked_conversations(&self) -> usize {
        self.affinities.len()
    }

    /// Drop conversations that have gone quiet, then any excess over the cap.
    ///
    /// A full `retain` per request is O(n) with n capped at 512 — microseconds,
    /// lost entirely in the noise of an HTTP round trip, and this runs under
    /// the lock that `AppState` holds across a decision. The alternative is an
    /// LRU list, which `PrivacyFilter` already considered and rejected for the
    /// same trade-off; a second, more complicated answer to the same question
    /// in the same daemon is worse than a slightly slower one.
    fn sweep(&mut self, now: DateTime<Utc>) {
        self.affinities
            .retain(|_, affinity| now - affinity.last_seen < AFFINITY_TTL);

        // Oldest use first, so a busy conversation outlives an idle one even
        // when both are inside the TTL.
        while self.affinities.len() > MAX_AFFINITIES {
            let Some(stalest) = self
                .affinities
                .iter()
                .min_by_key(|(_, affinity)| affinity.last_seen)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.affinities.remove(&stalest);
        }
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
        self.decide_with_override(key, inbound, peek, candidates, now, None)
    }

    /// [`Self::decide`], with a per-request route override from
    /// `X-IronWire-Route`.
    ///
    /// The override outranks a daemon-wide pin: it is the more specific
    /// instruction, and it came with this request.
    ///
    /// # Errors
    ///
    /// [`NoRoute::UnknownRoute`] when the override names a backend that does
    /// not exist, and [`NoRoute::AllIneligible`] when it names one that cannot
    /// serve this request.
    pub fn decide_with_override(
        &mut self,
        key: ConversationKey,
        inbound: Protocol,
        peek: &RequestPeek,
        candidates: &[Candidate],
        now: DateTime<Utc>,
        route_override: Option<(BackendId, Option<String>)>,
    ) -> Result<RouteDecision, NoRoute> {
        if candidates.is_empty() {
            return Err(NoRoute::NoBackendsConfigured);
        }

        // A named backend that does not exist is an error, not a fall-through.
        if let Some((requested, _)) = &route_override
            && !candidates.iter().any(|c| c.id == *requested)
        {
            return Err(NoRoute::UnknownRoute {
                requested: requested.to_string(),
                available: candidates.iter().map(|c| c.id.to_string()).collect(),
            });
        }

        let tier = peek
            .requested_model
            .as_deref()
            .map_or(ModelTier::Frontier, ModelTier::from_model_hint);

        // A pin bypasses preference but not eligibility: an unusable route is
        // still unusable, and silently corrupting a request because the user
        // asked for a backend is not obedience, it is a bug.
        let forced = route_override.clone().or_else(|| self.pin.clone());
        if let Some((pinned, model)) = forced
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
                reason: if route_override.is_some() {
                    "X-IronWire-Route".to_string()
                } else {
                    "pinned by user".to_string()
                },
            });
        }

        // Sticky: if this conversation already has a home and that home is
        // still fine, stay. Moving costs a cache; staying costs nothing.
        //
        // One exception. A conversation that descended a rung earlier is
        // sitting on degraded capacity, and a compaction turn is the worst
        // possible turn to spend it on: the summary becomes the conversation.
        // So a compaction turn re-selects rather than inheriting a degraded
        // affinity, and pays the cache cost to do it.
        let inherit_affinity = !peek.likely_compaction
            || self
                .affinities
                .get(&key)
                .is_none_or(|a| a.rung == Rung::Preferred);

        if inherit_affinity
            && let Some(affinity) = self.affinities.get(&key).cloned()
            && let Some(candidate) = candidates.iter().find(|c| c.id == affinity.backend)
            && incumbent_is_usable(candidate, peek, inbound, now)
        {
            let pressured = candidate.quota.is_pressured(now);
            let entry = self.affinities.get_mut(&key).expect("just found above");
            // Both early returns below leave through here, so this is the one
            // place that has to be right: miss it on the unpressured branch —
            // the common one — and every healthy conversation ages out while
            // actively in use.
            entry.last_seen = now;
            // Scoped so the mutable borrow ends before `select` needs `&self`
            // below. Everything the rest of this block depends on is read out
            // here.
            let pressure_started = if pressured {
                // A return to pressure discards accumulated recovery: a backend
                // hovering at the 90% threshold must not creep towards a
                // promotion one unpressured turn at a time.
                entry.recovery_since = None;
                Some(*entry.pressure_since.get_or_insert(now))
            } else {
                entry.pressure_since = None;
                None
            };

            match pressure_started {
                // Hysteresis: only descend once pressure has persisted.
                Some(since) if now - since < DESCENT_DEBOUNCE => {
                    return Ok(RouteDecision {
                        backend: affinity.backend,
                        model: affinity.model,
                        rung: affinity.rung,
                        translated: affinity.rung == Rung::CrossFamily,
                        reason: "sticky: pressure not yet sustained".to_string(),
                    });
                }
                // Sustained pressure: fall through to a fresh selection.
                Some(_) => {}
                None => {
                    if let Some(promoted) = self.consider_promotion(
                        &key,
                        &affinity,
                        Turn {
                            inbound,
                            peek,
                            tier,
                            candidates,
                            now,
                        },
                    ) {
                        return Ok(promoted);
                    }
                    return Ok(RouteDecision {
                        backend: affinity.backend,
                        model: affinity.model,
                        rung: affinity.rung,
                        translated: affinity.rung == Rung::CrossFamily,
                        reason: "sticky affinity".to_string(),
                    });
                }
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
                last_seen: now,
                pressure_since: None,
                recovery_since: None,
            },
        );
        // After the insert, not before: sweeping first leaves room for one more
        // and the map settles at the cap plus one. This is also the only point
        // the map grows, and it is safely clear of the `get`/`get_mut` pair
        // above, where an eviction could have pulled the ground out from under
        // that `expect`.
        self.sweep(now);
        Ok(decision)
    }

    /// Climb back up the ladder, if a better rung has been available long
    /// enough to believe in.
    ///
    /// Descent is a one-way door without this: a conversation that fell to a
    /// cross-family backend at nine in the morning finds it perfectly usable
    /// and unpressured all afternoon, and stays there on a cold cache while the
    /// capacity the user pays for sits idle. A state machine with no path back
    /// is not a transition, it is a trap.
    ///
    /// `None` means stay put, which is the answer on the overwhelming majority
    /// of turns — an undegraded conversation does not even reach here.
    fn consider_promotion(
        &mut self,
        key: &ConversationKey,
        affinity: &Affinity,
        turn: Turn<'_>,
    ) -> Option<RouteDecision> {
        let Turn {
            inbound,
            peek,
            tier,
            candidates,
            now,
        } = turn;
        // A conversation already at the top has nowhere to climb, and this is
        // the common case: no `select` runs for it.
        if affinity.rung == Rung::Preferred {
            return None;
        }

        // Ask the ladder rather than asking whether the preferred backend's
        // quota recovered. `select` already encodes eligibility, consent,
        // client identity, circuit state, catalogue and tier fit; a
        // reimplementation here would drift from it and would miss a backend
        // whose quota came back while its circuit stayed open.
        //
        // Compared by *rung*, never by backend id: rung 1 to rung 0 can be a
        // model change on the very same backend.
        let better = self
            .select(inbound, peek, tier, candidates, now)
            .ok()
            .filter(|decision| decision.rung < affinity.rung);
        let Some(candidate) = better else {
            // The improvement went away again. Recovery accumulated so far is
            // discarded rather than banked: a backend crossing in and out of
            // availability must not creep towards a promotion one good turn at
            // a time, because each promotion costs a warm prompt cache.
            if let Some(entry) = self.affinities.get_mut(key) {
                entry.recovery_since = None;
            }
            return None;
        };

        let entry = self.affinities.get_mut(key)?;
        // Recovery starts the first turn a better rung is available, whether or
        // not this turn can act on it.
        let recovering_since = *entry.recovery_since.get_or_insert(now);
        if now - recovering_since < PROMOTION_DEBOUNCE {
            return None;
        }

        // The same rule as the descent that created this route
        // (`docs/PROTOCOL.md` §6), and for the same reason rather than out of
        // symmetry: the recent assistant turns were produced by the foreign
        // family and carry none of this family's signed reasoning state, so
        // replaying that history mid-loop is the rejection risk the gate
        // exists to prevent. Blocked is not cancelled — `recovery_since`
        // stands, so the next turn boundary promotes rather than restarting
        // the wait.
        if affinity.rung == Rung::CrossFamily && peek.requirements.mid_tool_loop {
            return None;
        }

        *entry = Affinity {
            backend: candidate.backend.clone(),
            model: candidate.model.clone(),
            rung: candidate.rung,
            since: now,
            last_seen: now,
            pressure_since: None,
            recovery_since: None,
        };
        // The decision `select` produced, not a patched copy of the old one:
        // `translated` has to come from the new route. Updating the rung while
        // leaving `translated` derived from the stale one would translate a
        // request to a native backend and corrupt it.
        Some(RouteDecision {
            reason: format!("recovered to {:?}", candidate.rung).to_lowercase(),
            ..candidate
        })
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
        let mut capped: Vec<(BackendId, f64, f64)> = Vec::new();

        // Rungs 0-2 all forward the request's own bytes, so they need a backend
        // speaking the *same wire* — not merely one in the same family, which
        // would put a Chat Completions backend on a Responses request. Rung 3
        // is the translated lane. Within each we prefer free capacity, then
        // exact tier, then anything.
        let mut same_wire: Vec<&Candidate> = Vec::new();
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
                Err(Unusable::Unavailable) => {
                    if let crate::quota::Headroom::CapReached {
                        spent_usd, cap_usd, ..
                    } = candidate.quota.primary
                    {
                        capped.push((candidate.id.clone(), spent_usd, cap_usd));
                    }
                    continue;
                }
            }
            any_available = true;
            if candidate.caps.protocol == inbound {
                same_wire.push(candidate);
            } else {
                cross_family.push(candidate);
            }
        }

        // On an ordinary turn: free capacity first, then quality. On a
        // compaction turn the two swap, because the output of a compaction turn
        // *becomes the conversation* — it is written into the client's
        // permanent history and resent every turn afterwards
        // (`docs/PROTOCOL.md` §8). Saving money there buys one cheaper request
        // and pays for it for the rest of the session.
        let sort_key = |c: &&Candidate| {
            let pressured = u8::from(c.quota.is_pressured(now));
            let cost = c.kind.marginal_cost_rank();
            // Local capacity is the cheapest there is — rank 0, ahead of a
            // subscription — and descending a rung is normally fine, because
            // rung 1 means "same account, smaller model". A local backend
            // breaks that assumption: descending onto it means a 30B model on
            // the user's laptop taking work they asked Opus for, silently,
            // because it sorted cheapest. So a local backend that cannot serve
            // the requested tier yields to anything that can. It is still
            // tried when it is all there is — being last is not being refused.
            let beyond_its_tier = u8::from(c.kind == BackendKind::Local && !serves_tier(c, tier));
            if peek.likely_compaction {
                // `pick_model` falls back to a lesser tier rather than refusing,
                // so "has no model at all" is the wrong question here — what
                // matters is whether this backend can serve the tier the client
                // actually asked for, without descending.
                (
                    pressured,
                    beyond_its_tier,
                    u8::from(!serves_tier(c, tier)),
                    cost,
                )
            } else {
                (
                    pressured,
                    beyond_its_tier,
                    cost,
                    u8::from(pick_model(c, tier).is_none()),
                )
            }
        };
        same_wire.sort_by_key(sort_key);
        cross_family.sort_by_key(sort_key);

        if let Some(best) = same_wire.first() {
            // A model missing from a catalogue we only *guessed* is not a model
            // that does not exist — it is a build older than the model. The
            // providers ship faster than we do, so on the native lane, with
            // capacity to spare, forward the client's own string and let the
            // provider be the authority. Substituting the newest name we happen
            // to know is how `claude-opus-5` became `claude-opus-4-6` and
            // failed. Once the provider has told us its catalogue, that list is
            // authoritative and a missing model means what it says.
            let unrecognised = !best.catalogue_from_provider
                && peek.requested_model.as_deref().is_some_and(|requested| {
                    !best.models.iter().any(|(known, _)| known == requested)
                });
            if unrecognised && !best.quota.is_pressured(now) {
                return Ok(RouteDecision {
                    backend: best.id.clone(),
                    model: None,
                    rung: Rung::Preferred,
                    translated: false,
                    reason: format!("native lane, {} capacity", kind_label(best.kind)),
                });
            }

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

        // A cap is reported ahead of a generic exhaustion, and only when it is
        // the *reason*: with `on_breach = "descend"` there is other capacity to
        // fall to and we never reach here at all.
        if let Some((backend, spent_usd, cap_usd)) = capped.first().cloned() {
            return Err(NoRoute::SpendCapReached {
                backend: backend.to_string(),
                spent_usd,
                cap_usd,
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

/// Whether the backend a conversation is *already on* can serve this turn.
///
/// Everything `usable` refuses, except the mid-tool-loop rule — which does not
/// apply to the incumbent, and applying it here inverts the rule it comes from.
///
/// `eligible` refuses a cross-family route mid tool loop to stop a conversation
/// *switching* families in the middle of a loop (`docs/PROTOCOL.md` §6). A
/// conversation already on that backend is not switching; it is continuing, on
/// the backend whose reasoning state the loop is built from. Treating it as
/// ineligible meant a mid-loop turn found its own backend unusable and fell
/// through to a fresh selection, which dragged it back to the native family —
/// mid tool loop, which is the exact move the rule exists to prevent.
fn incumbent_is_usable(
    candidate: &Candidate,
    peek: &RequestPeek,
    inbound: Protocol,
    now: DateTime<Utc>,
) -> bool {
    matches!(
        usable(candidate, peek, inbound, now),
        Ok(()) | Err(Unusable::Ineligible(Ineligible::MidToolLoop))
    )
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
    // "Needs translation" is a question about the wire, not about the family:
    // Responses and Chat Completions share a family and are different wires.
    let cross = candidate.caps.protocol != inbound;
    if cross && !inbound.translates_to(candidate.caps.protocol) {
        return Err(Unusable::Ineligible(Ineligible::NoTranslationPath));
    }
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

/// Whether this backend can serve the requested tier without descending.
///
/// Distinct from [`pick_model`], which deliberately falls back to a lesser model
/// rather than refusing. Both behaviours are wanted, at different moments:
/// falling back keeps an ordinary turn working, and refusing to fall back is
/// what a compaction turn needs (`docs/PROTOCOL.md` §8).
fn serves_tier(candidate: &Candidate, tier: ModelTier) -> bool {
    candidate.models.iter().any(|(_, t)| *t >= tier)
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
            catalogue_from_provider: true,
        }
    }

    fn peek(model: &str) -> RequestPeek {
        RequestPeek {
            requested_model: Some(model.to_string()),
            stream: true,
            requirements: RequestRequirements::default(),
            carries_client_identity: true,
            message_count: 3,
            likely_compaction: false,
        }
    }

    fn key() -> ConversationKey {
        ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are Claude Code",
            &["Read"],
        )
    }

    /// The test the local-backend feature exists under, written before it.
    ///
    /// `BackendKind::Local` has `marginal_cost_rank() == 0` — cheaper than a
    /// subscription — so the *only* thing keeping a 30B model on someone's
    /// laptop from taking every frontier request is the tier its slugs carry.
    /// `from_model_hint` resolves anything it does not recognise to `Frontier`,
    /// which is right for a hosted catalogue and catastrophic for a local one.
    /// Local models are therefore `Fast` unless the user says otherwise, and
    /// this asserts the consequence rather than the mechanism.
    #[test]
    fn a_frontier_request_does_not_land_on_a_local_model() {
        let mut local = candidate("ollama", BackendKind::Local, Protocol::AnthropicMessages);
        local.models = vec![("qwen3-coder:30b".to_string(), ModelTier::Fast)];
        let subscription = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );

        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[local, subscription],
                t(0),
            )
            .expect("routes");
        assert_eq!(
            decision.backend.as_str(),
            "claude-sub",
            "a frontier request went to a local model because it sorted cheapest"
        );
    }

    /// The other half: a local backend is not decoration. Asked for something
    /// it can serve, it wins on cost exactly as intended.
    #[test]
    fn a_fast_request_does_land_on_a_local_model() {
        let mut local = candidate("ollama", BackendKind::Local, Protocol::AnthropicMessages);
        local.models = vec![("qwen3-coder:30b".to_string(), ModelTier::Fast)];
        let mut subscription = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        subscription.models = vec![("claude-haiku-4-5".to_string(), ModelTier::Fast)];

        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-haiku-4-5"),
                &[local, subscription],
                t(0),
            )
            .expect("routes");
        assert_eq!(decision.backend.as_str(), "ollama");
    }

    /// Descent without promotion is a trap rather than a transition: a
    /// conversation that fell to a cross-family backend in the morning stays
    /// there all day, cold cache and all, while the capacity the user pays for
    /// sits idle.
    mod promotion {
        use super::*;

        /// The preferred backend, and the foreign-family one below it.
        ///
        /// Exhausted rather than merely pressured: a same-wire backend that is
        /// *usable* always wins its partition, so a cross-family descent only
        /// happens once the preferred one is genuinely unavailable. Pressure
        /// alone moves a conversation down a model tier, not across a family.
        fn ladder(exhausted_until: Option<DateTime<Utc>>) -> Vec<Candidate> {
            let mut preferred = candidate(
                "claude-sub",
                BackendKind::Subscription,
                Protocol::AnthropicMessages,
            );
            if let Some(until) = exhausted_until {
                preferred.quota.primary = Headroom::Exhausted { until };
            }
            let fallback = candidate("nearai", BackendKind::Credits, Protocol::OpenAiChat);
            vec![preferred, fallback]
        }

        fn route(
            policy: &mut Policy,
            peek: &RequestPeek,
            candidates: &[Candidate],
            at: DateTime<Utc>,
        ) -> RouteDecision {
            policy
                .decide(key(), Protocol::AnthropicMessages, peek, candidates, at)
                .expect("routes")
        }

        /// A conversation stranded on the foreign family, as a morning rate
        /// limit would leave it.
        fn stranded() -> (Policy, DateTime<Utc>) {
            let mut policy = Policy::new();
            let recovers_at = t(600);
            let decision = route(
                &mut policy,
                &peek("claude-opus-4-6"),
                &ladder(Some(recovers_at)),
                t(0),
            );
            assert_eq!(decision.backend.as_str(), "nearai");
            assert_eq!(decision.rung, Rung::CrossFamily);
            assert!(decision.translated);
            (policy, recovers_at)
        }

        #[test]
        fn a_recovered_backend_takes_the_conversation_back() {
            let (mut policy, recovers_at) = stranded();
            let ordinary = peek("claude-opus-4-6");
            // The first turn after recovery starts the clock; it does not move.
            route(&mut policy, &ordinary, &ladder(None), recovers_at);
            let decision = route(
                &mut policy,
                &ordinary,
                &ladder(None),
                recovers_at + PROMOTION_DEBOUNCE,
            );
            assert_eq!(decision.backend.as_str(), "claude-sub");
            assert_eq!(decision.rung, Rung::Preferred);
            assert!(
                !decision.translated,
                "a promoted route must not still be marked translated"
            );
        }

        #[test]
        fn it_does_not_climb_back_the_moment_pressure_lifts() {
            let (mut policy, recovers_at) = stranded();
            let ordinary = peek("claude-opus-4-6");
            route(&mut policy, &ordinary, &ladder(None), recovers_at);
            let decision = route(
                &mut policy,
                &ordinary,
                &ladder(None),
                recovers_at + Duration::seconds(30),
            );
            assert_eq!(
                decision.backend.as_str(),
                "nearai",
                "promoted before the debounce elapsed"
            );
        }

        /// The debounce is the only damping there is — `is_pressured` is a step
        /// function at 90% — so a backend crossing the line repeatedly must not
        /// accumulate its way to a promotion one good turn at a time.
        #[test]
        fn a_flapping_backend_never_promotes() {
            let (mut policy, recovers_at) = stranded();
            let ordinary = peek("claude-opus-4-6");
            let mut at = recovers_at;
            for _ in 0..10 {
                // Available: recovery starts accruing.
                route(&mut policy, &ordinary, &ladder(None), at);
                at += Duration::minutes(2);
                // Gone again before the debounce elapsed: it starts over.
                route(
                    &mut policy,
                    &ordinary,
                    &ladder(Some(at + Duration::hours(1))),
                    at,
                );
                at += Duration::minutes(2);
            }
            assert_eq!(
                policy.current_backend(&key()).map(|b| b.to_string()),
                Some("nearai".to_string()),
                "a flapping backend promoted a conversation"
            );
        }

        /// Leaving the foreign family is the same hazard as entering it: the
        /// recent assistant turns carry none of this family's signed reasoning
        /// state, so replaying them mid-loop is the rejection risk the gate
        /// exists to prevent (`docs/PROTOCOL.md` §6).
        #[test]
        fn a_cross_family_promotion_waits_for_a_turn_boundary() {
            let (mut policy, recovers_at) = stranded();
            let mut mid_loop = peek("claude-opus-4-6");
            mid_loop.requirements.mid_tool_loop = true;

            route(&mut policy, &mid_loop, &ladder(None), recovers_at);
            let blocked = route(
                &mut policy,
                &mid_loop,
                &ladder(None),
                recovers_at + PROMOTION_DEBOUNCE,
            );
            assert_eq!(
                blocked.backend.as_str(),
                "nearai",
                "promoted across families mid tool loop"
            );

            // The next clean turn promotes immediately: being blocked is not
            // being reset, or a busy tool loop would postpone recovery forever.
            let promoted = route(
                &mut policy,
                &peek("claude-opus-4-6"),
                &ladder(None),
                recovers_at + PROMOTION_DEBOUNCE + Duration::seconds(1),
            );
            assert_eq!(promoted.backend.as_str(), "claude-sub");
        }

        #[test]
        fn a_conversation_at_the_top_stays_where_it_is() {
            let mut policy = Policy::new();
            let ordinary = peek("claude-opus-4-6");
            let first = route(&mut policy, &ordinary, &ladder(None), t(0));
            assert_eq!(first.rung, Rung::Preferred);
            let later = route(
                &mut policy,
                &ordinary,
                &ladder(None),
                t(0) + PROMOTION_DEBOUNCE * 10,
            );
            assert_eq!(later.reason, "sticky affinity");
            assert_eq!(later.backend.as_str(), "claude-sub");
        }
    }

    /// A distinct conversation per index, so a test can fill the map.
    fn key_n(n: usize) -> ConversationKey {
        ConversationKey::derive(
            Protocol::AnthropicMessages,
            &format!("You are Claude Code, session {n}"),
            &["Read"],
        )
    }

    fn route(policy: &mut Policy, key: ConversationKey, at: DateTime<Utc>) {
        policy
            .decide(
                key,
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[candidate(
                    "claude-sub",
                    BackendKind::Subscription,
                    Protocol::AnthropicMessages,
                )],
                at,
            )
            .expect("routes");
    }

    #[test]
    fn the_affinity_map_is_bounded() {
        // Left unbounded this grew one entry per distinct conversation for the
        // life of a daemon that is meant to run for weeks.
        let mut policy = Policy::new();
        for n in 0..1000 {
            route(&mut policy, key_n(n), t(0));
        }
        assert!(
            policy.tracked_conversations() <= MAX_AFFINITIES,
            "kept {} affinities",
            policy.tracked_conversations()
        );
    }

    #[test]
    fn a_conversation_that_goes_quiet_is_forgotten() {
        let mut policy = Policy::new();
        route(&mut policy, key(), t(0));
        assert_eq!(policy.tracked_conversations(), 1);

        // Advance the clock rather than sleep: `now` is a parameter precisely
        // so this is testable.
        route(&mut policy, key_n(99), t(AFFINITY_TTL.num_seconds() + 1));
        assert_eq!(
            policy.tracked_conversations(),
            1,
            "the idle conversation should have been swept, leaving only the new one"
        );
    }

    /// The `since`-versus-`last_seen` trap. A conversation established days ago
    /// but used a second ago is the *most* alive thing in the map; evicting on
    /// `since` would drop it first.
    #[test]
    fn a_conversation_still_in_use_is_never_aged_out() {
        let mut policy = Policy::new();
        let hourly = AFFINITY_TTL.num_seconds() / 2;
        for step in 0..6 {
            route(&mut policy, key(), t(step * hourly));
        }
        assert_eq!(
            policy.tracked_conversations(),
            1,
            "an actively used conversation was evicted by age"
        );
        assert_eq!(
            policy.current_backend(&key()).map(|b| b.to_string()),
            Some("claude-sub".to_string())
        );
    }

    /// Over the cap, the entry that goes is the least recently *used* — not
    /// whatever `HashMap` iteration happens to surface first.
    #[test]
    fn the_stalest_conversation_is_the_one_evicted() {
        let mut policy = Policy::new();
        for n in 0..MAX_AFFINITIES {
            route(&mut policy, key_n(n), t(0));
        }
        // Touch every key except one, so that one is unambiguously the stalest.
        for n in 1..MAX_AFFINITIES {
            route(&mut policy, key_n(n), t(60));
        }
        assert!(
            policy.current_backend(&key_n(0)).is_some(),
            "not yet evicted"
        );

        // One more conversation puts us over the cap.
        route(&mut policy, key_n(MAX_AFFINITIES), t(120));
        assert!(
            policy.current_backend(&key_n(0)).is_none(),
            "the stalest conversation survived"
        );
        assert!(
            policy.current_backend(&key_n(1)).is_some(),
            "a recently used conversation was evicted instead"
        );
    }

    /// Eviction is invisible: the next request re-selects and, conditions
    /// unchanged, lands in the same place.
    #[test]
    fn an_evicted_conversation_simply_re_establishes_itself() {
        let mut policy = Policy::new();
        route(&mut policy, key(), t(0));
        let later = t(AFFINITY_TTL.num_seconds() + 1);
        route(&mut policy, key_n(1), later);
        assert!(policy.current_backend(&key()).is_none(), "was swept");

        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek("claude-opus-4-6"),
                &[candidate(
                    "claude-sub",
                    BackendKind::Subscription,
                    Protocol::AnthropicMessages,
                )],
                later,
            )
            .expect("routes again without error");
        assert_eq!(decision.backend.as_str(), "claude-sub");
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
    fn a_conversation_mid_tool_loop_waits_for_the_turn_boundary() {
        // The corrected rule: a family change is deferred to the next clean
        // turn, not refused for the life of the conversation.
        let mut policy = Policy::new();
        let mut exhausted = candidate(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
        );
        exhausted.quota.primary = Headroom::Exhausted { until: t(3600) };
        let near = candidate("nearai", BackendKind::Credits, Protocol::OpenAiChat);

        let mut mid_loop = peek("claude-opus-4-6");
        mid_loop.requirements.reasoning = ReasoningNeed::LoadBearing;
        mid_loop.requirements.mid_tool_loop = true;

        let err = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &mid_loop,
                &[exhausted.clone(), near.clone()],
                t(0),
            )
            .expect_err("a family change mid tool loop must be refused");
        match err {
            NoRoute::AllIneligible { reasons } => {
                assert!(
                    reasons
                        .iter()
                        .any(|(_, why)| *why == Ineligible::MidToolLoop)
                );
            }
            other => panic!("expected ineligibility, got {other:?}"),
        }

        // Same conversation, next turn boundary: NEAR AI is now eligible.
        let mut boundary = mid_loop;
        boundary.requirements.mid_tool_loop = false;
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &boundary,
                &[exhausted, near],
                t(0),
            )
            .expect("a turn boundary is a clean switch point");
        assert_eq!(decision.backend.as_str(), "nearai");
        assert!(decision.translated);
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

#[cfg(test)]
mod compaction_tests {
    //! `docs/PROTOCOL.md` §8 — a compaction turn's output *becomes* the
    //! conversation, so fidelity outranks cost on exactly that turn.

    use super::*;
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

    /// A backend offering exactly the models it is given.
    fn backend(
        id: &str,
        kind: BackendKind,
        protocol: Protocol,
        models: &[(&str, ModelTier)],
    ) -> Candidate {
        Candidate {
            id: BackendId::from(id),
            kind,
            caps: caps(protocol),
            quota: QuotaSnapshot::default(),
            healthy: true,
            consented: true,
            requires_client_identity: false,
            models: models
                .iter()
                .map(|(m, tier)| ((*m).to_string(), *tier))
                .collect(),
            // These fixtures state what a backend really offers, so they stand
            // for a catalogue the provider gave us, not one we guessed.
            catalogue_from_provider: true,
        }
    }

    fn peek(compaction: bool) -> RequestPeek {
        RequestPeek {
            requested_model: Some("claude-opus-4-6".to_string()),
            stream: true,
            requirements: RequestRequirements::default(),
            carries_client_identity: true,
            message_count: 40,
            likely_compaction: compaction,
        }
    }

    fn key() -> ConversationKey {
        ConversationKey::derive(
            Protocol::AnthropicMessages,
            "You are Claude Code",
            &["Read"],
        )
    }

    /// A free subscription that can only serve a lesser model, and a metered
    /// key that can serve the one the client asked for. The interesting case:
    /// the two orderings disagree.
    fn split_candidates() -> Vec<Candidate> {
        vec![
            backend(
                "claude-sub",
                BackendKind::Subscription,
                Protocol::AnthropicMessages,
                &[("claude-sonnet-4-6", ModelTier::Balanced)],
            ),
            backend(
                "anthropic-key",
                BackendKind::ApiKey,
                Protocol::AnthropicMessages,
                &[("claude-opus-4-6", ModelTier::Frontier)],
            ),
        ]
    }

    #[test]
    fn an_ordinary_turn_prefers_free_capacity_over_the_exact_model() {
        // Unchanged behaviour, asserted so the compaction change cannot quietly
        // alter what every other turn does.
        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(false),
                &split_candidates(),
                t(0),
            )
            .expect("routes");
        assert_eq!(decision.backend.as_str(), "claude-sub");
    }

    #[test]
    fn a_compaction_turn_pays_for_the_model_the_client_asked_for() {
        // The summary is written into the client's permanent history and
        // resent every turn afterwards. A cheaper summary is not a saving.
        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(true),
                &split_candidates(),
                t(0),
            )
            .expect("routes");
        assert_eq!(decision.backend.as_str(), "anthropic-key");
        assert_eq!(decision.rung, Rung::Preferred);
    }

    #[test]
    fn a_compaction_turn_still_avoids_pressured_capacity_first() {
        // Fidelity outranks cost, not availability. Sending a compaction turn
        // at an exhausted backend fails the turn, which is strictly worse than
        // a cheaper summary.
        let mut candidates = split_candidates();
        candidates[1].quota.primary = Headroom::Exhausted { until: t(3600) };
        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(true),
                &candidates,
                t(0),
            )
            .expect("routes");
        assert_eq!(decision.backend.as_str(), "claude-sub");
    }

    #[test]
    fn a_compaction_turn_leaves_a_degraded_affinity_behind() {
        // A conversation that descended earlier is sitting on degraded
        // capacity. Inheriting that for the one turn whose output is permanent
        // is the mistake this rule exists to prevent.
        let mut policy = Policy::new();

        // Establish an affinity on the cheaper backend via an ordinary turn.
        let first = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(false),
                &split_candidates(),
                t(0),
            )
            .expect("routes");
        assert_eq!(first.backend.as_str(), "claude-sub");
        assert_eq!(first.rung, Rung::SmallerModel, "the affinity is degraded");

        let compaction = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(true),
                &split_candidates(),
                t(60),
            )
            .expect("routes");
        assert_eq!(
            compaction.backend.as_str(),
            "anthropic-key",
            "a compaction turn must not inherit a degraded affinity"
        );
    }

    #[test]
    fn a_compaction_turn_keeps_an_undegraded_affinity() {
        // The rule is about *degraded* affinity. A conversation already on a
        // full-fidelity backend should stay there — moving would throw away a
        // warm prompt cache for nothing.
        let candidates = vec![backend(
            "claude-sub",
            BackendKind::Subscription,
            Protocol::AnthropicMessages,
            &[("claude-opus-4-6", ModelTier::Frontier)],
        )];
        let mut policy = Policy::new();
        let first = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(false),
                &candidates,
                t(0),
            )
            .expect("routes");
        assert_eq!(first.rung, Rung::Preferred);

        let compaction = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(true),
                &candidates,
                t(60),
            )
            .expect("routes");
        assert_eq!(compaction.backend.as_str(), "claude-sub");
        assert_eq!(compaction.reason, "sticky affinity");
    }

    #[test]
    fn a_compaction_turn_is_still_served_when_only_a_foreign_family_is_left() {
        // Fidelity is a preference, not a veto. Refusing the turn would leave
        // the user unable to compact at all, which means the session cannot
        // continue — strictly worse than a summary from another family.
        let candidates = vec![backend(
            "nearai",
            BackendKind::Credits,
            Protocol::OpenAiChat,
            &[("deepseek-v3", ModelTier::Frontier)],
        )];
        let mut policy = Policy::new();
        let decision = policy
            .decide(
                key(),
                Protocol::AnthropicMessages,
                &peek(true),
                &candidates,
                t(0),
            )
            .expect("a compaction turn must still be served");
        assert_eq!(decision.rung, Rung::CrossFamily);
        assert!(decision.translated);
    }
}

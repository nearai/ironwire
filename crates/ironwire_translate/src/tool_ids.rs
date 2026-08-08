//! Tool-call identity across a family boundary.
//!
//! The client replays whatever id we returned, forever — so an id we mint on
//! the way back from a foreign provider has to be recognisable, valid in the
//! client's namespace, and reversible when the client sends it out again.
//!
//! The obvious design is a per-conversation map. We deliberately do not use
//! one: a map is state that can be lost (daemon restart, eviction), and losing
//! it produces *invalid ids* — a failure the user cannot diagnose and we cannot
//! recover from. A reversible encoding has no such failure mode, costs nothing,
//! survives restarts, and reads plainly in a log.

/// Marker for an id IronWire minted while translating a foreign tool call.
///
/// `toolu_` keeps it valid in Anthropic's namespace; `xw_` distinguishes it
/// from an id Anthropic itself minted, so the reverse direction knows whether
/// to strip anything.
const MINTED_PREFIX: &str = "toolu_xw_";

/// Convert a foreign provider's tool-call id into one the Anthropic-facing
/// client can carry.
///
/// **Always** prefixes, including when the foreign id already begins with the
/// marker. An earlier version skipped the prefix in that case, reasoning that
/// the id was "already ours" — but this is only ever called on ids arriving
/// *from* a foreign provider, so it never was. What that guard actually did was
/// break the round trip for any provider that happened to mint an id starting
/// with `toolu_xw_`: the reverse direction stripped a prefix nobody had added,
/// and the client's replayed `tool_use_id` no longer matched the call it
/// belonged to.
///
/// Prefixing unconditionally makes this a bijection, which is a stronger and
/// simpler property than idempotence — and idempotence was never the one
/// needed.
#[must_use]
pub fn to_anthropic(foreign_id: &str) -> String {
    format!("{MINTED_PREFIX}{foreign_id}")
}

/// Recover the foreign id the client is replaying.
///
/// An id we did not mint passes through unchanged: the foreign provider accepts
/// arbitrary strings, so an Anthropic-native `toolu_*` from an earlier
/// same-family turn is a perfectly good id on the other side.
#[must_use]
pub fn to_foreign(anthropic_id: &str) -> String {
    anthropic_id
        .strip_prefix(MINTED_PREFIX)
        .unwrap_or(anthropic_id)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_minted_id_round_trips() {
        let foreign = "call_abc123XYZ";
        let anthropic = to_anthropic(foreign);
        assert!(anthropic.starts_with("toolu_"), "{anthropic}");
        assert_eq!(to_foreign(&anthropic), foreign);
    }

    #[test]
    fn a_client_minted_id_passes_through_untouched() {
        // Anthropic's own ids are already valid on the far side.
        let native = "toolu_01ABCdef";
        assert_eq!(to_foreign(native), native);
    }

    #[test]
    fn the_round_trip_holds_for_ids_that_look_like_ours() {
        // The case that motivated removing the "already ours" guard. A foreign
        // provider minting `toolu_xw_call_1` used to come back as `call_1`, so
        // the client's replayed `tool_use_id` no longer matched the call it
        // belonged to — a mismatch neither side can diagnose.
        for id in ["toolu_xw_call_1", "toolu_xw_", "toolu_xw_toolu_xw_nested"] {
            assert_eq!(to_foreign(&to_anthropic(id)), id, "round trip lost {id}");
        }
    }

    #[test]
    fn one_translation_adds_exactly_one_prefix() {
        // A bijection, not an idempotent function. The distinction is the fix:
        // idempotence would mean `to_anthropic` could not tell a foreign id
        // that looks like ours from one that is.
        assert_eq!(to_anthropic("call_1"), "toolu_xw_call_1");
        assert_eq!(to_anthropic("toolu_xw_call_1"), "toolu_xw_toolu_xw_call_1");
    }

    #[test]
    fn the_mapping_needs_no_state_and_so_survives_a_restart() {
        // The property that motivates the encoding: two independent processes
        // agree without sharing anything.
        assert_eq!(to_foreign(&to_anthropic("call_9")), "call_9");
    }

    #[test]
    fn odd_ids_survive() {
        for id in [
            "",
            "call_-_/=+",
            "toolu_",
            "toolu_xw",
            "TOOLU_XW_upper",
            "call with spaces",
            "日本語の識別子",
            "x".repeat(2000).as_str(),
        ] {
            assert_eq!(to_foreign(&to_anthropic(id)), id, "round trip lost {id:?}");
        }
    }

    #[test]
    fn every_minted_id_is_valid_in_the_clients_namespace() {
        // The client replays whatever we return, forever. An id Anthropic would
        // reject is a conversation that cannot continue.
        for id in ["call_1", "", "日本語"] {
            assert!(to_anthropic(id).starts_with("toolu_"), "{id:?}");
        }
    }
}

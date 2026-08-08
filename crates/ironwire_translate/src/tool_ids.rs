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
#[must_use]
pub fn to_anthropic(foreign_id: &str) -> String {
    if foreign_id.starts_with(MINTED_PREFIX) {
        // Already ours — do not double-wrap on a re-translation.
        return foreign_id.to_string();
    }
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
    fn translating_twice_does_not_double_wrap() {
        let once = to_anthropic("call_1");
        assert_eq!(to_anthropic(&once), once);
        assert_eq!(to_foreign(&once), "call_1");
    }

    #[test]
    fn the_mapping_needs_no_state_and_so_survives_a_restart() {
        // The property that motivates the encoding: two independent processes
        // agree without sharing anything.
        assert_eq!(to_foreign(&to_anthropic("call_9")), "call_9");
    }

    #[test]
    fn odd_ids_survive() {
        for id in ["", "call_-_/=+", "toolu_", "x".repeat(200).as_str()] {
            assert_eq!(to_foreign(&to_anthropic(id)), id);
        }
    }
}

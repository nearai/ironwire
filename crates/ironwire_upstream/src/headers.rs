//! Header handling for the native lane.
//!
//! The exact mutation set is enumerated in `docs/PROTOCOL.md` §2 and tested
//! here. Anything not named is forwarded untouched — including provider headers
//! we have never heard of, which is how a new `anthropic-beta` value keeps
//! working without an IronWire release.

/// Headers that describe *this* hop and must never be forwarded to the next
/// one. Forwarding `transfer-encoding` in particular corrupts the response.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Headers carrying the *client's* credential. Stripped so a stray key in a
/// client config can never reach a provider under IronWire's routing, and
/// replaced with the credential the router actually chose.
const AUTH: &[&str] = &["authorization", "x-api-key", "api-key"];

/// Headers we replace because they describe the connection to IronWire rather
/// than the connection to the provider.
const REWRITTEN: &[&str] = &["host", "content-length", "accept-encoding"];

/// Whether an inbound request header should be forwarded upstream.
#[must_use]
pub fn forward_request_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !HOP_BY_HOP.contains(&lower.as_str())
        && !AUTH.contains(&lower.as_str())
        && !REWRITTEN.contains(&lower.as_str())
}

/// Whether an upstream response header should be forwarded to the client.
///
/// Content encoding is included deliberately: we do not decompress, so the
/// client must be told what it is receiving.
#[must_use]
pub fn forward_response_header(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !HOP_BY_HOP.contains(&lower.as_str()) && lower != "content-length"
}

/// Select the inbound headers to forward.
#[must_use]
pub fn select_forwarded<'a, I>(headers: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (&'a str, &'a str)>,
{
    headers
        .into_iter()
        .filter(|(name, _)| forward_request_header(name))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_headers_we_have_never_seen_are_forwarded() {
        // This is the whole point of the native lane: a beta flag shipped
        // after us must keep working without an IronWire release.
        assert!(forward_request_header("anthropic-beta"));
        assert!(forward_request_header("anthropic-version"));
        assert!(forward_request_header("anthropic-some-future-thing"));
        assert!(forward_request_header("x-stainless-lang"));
    }

    #[test]
    fn the_clients_own_credentials_never_ride_along() {
        assert!(!forward_request_header("authorization"));
        assert!(!forward_request_header("Authorization"));
        assert!(!forward_request_header("x-api-key"));
        assert!(!forward_request_header("X-Api-Key"));
    }

    #[test]
    fn hop_by_hop_headers_are_dropped_in_both_directions() {
        for name in ["connection", "Transfer-Encoding", "keep-alive", "upgrade"] {
            assert!(!forward_request_header(name), "{name} leaked upstream");
            assert!(!forward_response_header(name), "{name} leaked downstream");
        }
    }

    #[test]
    fn rate_limit_headers_reach_us_intact() {
        // Observation depends on these surviving the response path.
        assert!(forward_response_header(
            "anthropic-ratelimit-unified-status"
        ));
        assert!(forward_response_header("retry-after"));
    }

    #[test]
    fn content_length_is_recomputed_not_forwarded() {
        // A policy-driven model edit changes the body length; forwarding the
        // original would truncate the request.
        assert!(!forward_request_header("content-length"));
        assert!(!forward_response_header("content-length"));
    }

    #[test]
    fn selection_normalises_names_and_keeps_order() {
        let selected = select_forwarded([
            ("Anthropic-Version", "2023-06-01"),
            ("Authorization", "Bearer secret"),
            ("Content-Type", "application/json"),
        ]);
        assert_eq!(
            selected,
            vec![
                ("anthropic-version".to_string(), "2023-06-01".to_string()),
                ("content-type".to_string(), "application/json".to_string()),
            ]
        );
    }
}

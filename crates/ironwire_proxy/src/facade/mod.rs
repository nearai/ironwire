//! Provider façades: the native API surfaces IronWire presents on loopback.

pub mod anthropic;
pub mod error;
pub mod openai;

pub use error::FacadeError;

/// The client's own session id, when it sent one.
///
/// Read from `x-ironwire-session-id` when the client sent it, and otherwise
/// from the native header for the facade's protocol. The neutral header is
/// the one the client chose to address to IronWire, so it takes precedence
/// whenever both are present.
///
/// Bounded and sanitised before it reaches the ledger: this is client-supplied
/// text, and it is written to a local database that `ironwire log` renders to a
/// terminal. Anything that is not a plain identifier is dropped rather than
/// stored -- a row addressed by nothing is strictly better than a row that can
/// smuggle control characters onto a user's screen.
pub(crate) fn client_session_id(
    headers: &axum::http::HeaderMap,
    protocol: ironwire_core::protocol::Protocol,
) -> Option<String> {
    // The header addressed to us wins; the native one is the fallback. A
    // neutral header that fails validation is a client that sent us garbage,
    // and the answer is nothing -- not its native id filed instead.
    let name = if headers.contains_key(ironwire_upstream::headers::NEUTRAL_SESSION_HEADER) {
        ironwire_upstream::headers::NEUTRAL_SESSION_HEADER
    } else {
        ironwire_upstream::headers::client_session_header(protocol)
    };
    let raw = headers.get(name)?.to_str().ok()?;
    let ok = !raw.is_empty()
        && raw.len() <= 200
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':' | '.'));
    ok.then(|| raw.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;
    use ironwire_core::protocol::Protocol;

    fn with(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if let Ok(v) = axum::http::HeaderValue::from_str(value) {
            headers.insert(name, v);
        }
        headers
    }

    #[test]
    fn a_real_session_id_is_recorded() {
        let headers = with(
            "x-claude-code-session-id",
            "5db811ed-ce4a-45a7-ab00-56890e111668",
        );
        assert_eq!(
            client_session_id(&headers, Protocol::AnthropicMessages).as_deref(),
            Some("5db811ed-ce4a-45a7-ab00-56890e111668")
        );
        let headers = with("session-id", "01a05a5d-243a-7ac0-bdcd-ba06b4309c36");
        assert_eq!(
            client_session_id(&headers, Protocol::OpenAiResponses).as_deref(),
            Some("01a05a5d-243a-7ac0-bdcd-ba06b4309c36")
        );
    }

    #[test]
    fn an_absent_header_records_nothing_rather_than_an_empty_string() {
        assert!(client_session_id(&HeaderMap::new(), Protocol::AnthropicMessages).is_none());
        let headers = with("x-claude-code-session-id", "");
        assert!(client_session_id(&headers, Protocol::AnthropicMessages).is_none());
    }

    #[test]
    fn a_session_id_that_is_not_an_identifier_is_dropped() {
        // Client-supplied text that `ironwire log` renders to a terminal. A row
        // addressed by nothing beats a row carrying an escape sequence.
        for hostile in [
            "not a session id",
            "\u{1b}[2Jcleared",
            "a/../../etc/passwd",
            "<script>",
        ] {
            let headers = with("x-claude-code-session-id", hostile);
            assert!(
                client_session_id(&headers, Protocol::AnthropicMessages).is_none(),
                "should have dropped {hostile:?}"
            );
        }
        let headers = with("x-claude-code-session-id", &"a".repeat(201));
        assert!(client_session_id(&headers, Protocol::AnthropicMessages).is_none());
    }
}

#[cfg(test)]
mod join_contract {
    use super::*;
    use axum::http::HeaderMap;
    use ironwire_core::protocol::Protocol;

    fn with(name: &'static str, value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            name,
            axum::http::HeaderValue::from_str(value).expect("header"),
        );
        headers
    }

    /// A consumer joins on this, so the header a façade reads is part of the
    /// contract rather than an implementation detail. `docs/PROTOCOL.md` §3
    /// states these two; a change here is a change there.
    #[test]
    fn each_facade_reads_the_header_protocol_md_names() {
        assert_eq!(
            ironwire_upstream::headers::client_session_header(Protocol::AnthropicMessages),
            "x-claude-code-session-id"
        );
        assert_eq!(
            ironwire_upstream::headers::client_session_header(Protocol::OpenAiResponses),
            "session-id"
        );
        assert_eq!(
            ironwire_upstream::headers::client_session_header(Protocol::OpenAiChat),
            "session-id"
        );
    }

    /// Verbatim, and that is the whole contract: a consumer holding the same
    /// session under a different spelling must map it itself, because
    /// IronWire never sees the other spelling and cannot.
    ///
    /// The values here are the two real shapes we have seen: Claude Code
    /// sends a bare UUID, and a client whose own identifier carries a prefix
    /// sends it with the prefix intact.
    #[test]
    fn the_session_id_is_stored_verbatim() {
        let bare = "5db811ed-ce4a-45a7-ab00-56890e111668";
        assert_eq!(
            client_session_id(
                &with("x-claude-code-session-id", bare),
                Protocol::AnthropicMessages
            )
            .as_deref(),
            Some(bare),
            "no normalisation, no case folding"
        );

        let prefixed = "rollout-2026-09-02T10-14-22-5db811ed-ce4a-45a7-ab00-56890e111668";
        assert_eq!(
            client_session_id(&with("session-id", prefixed), Protocol::OpenAiChat).as_deref(),
            Some(prefixed),
            "a prefix is not stripped -- a consumer expecting a bare UUID here \
             would join nothing, and silently"
        );
    }

    /// A client that names no session yields no attribution, rather than a
    /// fabricated one. Same rule as usage and capacity.
    #[test]
    fn no_header_is_none_not_a_placeholder() {
        assert_eq!(
            client_session_id(&HeaderMap::new(), Protocol::AnthropicMessages),
            None
        );
    }

    /// A client with no native session header can still name its session.
    #[test]
    fn the_neutral_header_is_read_on_every_facade() {
        let id = "aider-2026-09-03-0001";
        for protocol in [
            Protocol::AnthropicMessages,
            Protocol::OpenAiResponses,
            Protocol::OpenAiChat,
        ] {
            assert_eq!(
                client_session_id(&with("x-ironwire-session-id", id), protocol).as_deref(),
                Some(id),
                "{protocol:?} must read the neutral header"
            );
        }
    }

    /// The header the client addressed to us wins over the one it addresses
    /// to the provider. A wrapper that sets ours has said which id it wants
    /// the row filed under.
    #[test]
    fn the_neutral_header_takes_precedence_over_the_native_one() {
        let mut headers = with("x-claude-code-session-id", "native-id");
        headers.insert(
            "x-ironwire-session-id",
            axum::http::HeaderValue::from_static("chosen-id"),
        );
        assert_eq!(
            client_session_id(&headers, Protocol::AnthropicMessages).as_deref(),
            Some("chosen-id")
        );
    }

    /// The same validation applies: a hostile neutral header is dropped, and
    /// does not fall through to the native one either.
    #[test]
    fn a_hostile_neutral_header_is_dropped_not_bypassed() {
        let mut headers = with("x-claude-code-session-id", "native-id");
        headers.insert(
            "x-ironwire-session-id",
            axum::http::HeaderValue::from_static("not a session id"),
        );
        assert_eq!(
            client_session_id(&headers, Protocol::AnthropicMessages),
            None,
            "a client that sent us garbage does not get its native id filed instead"
        );
    }
}

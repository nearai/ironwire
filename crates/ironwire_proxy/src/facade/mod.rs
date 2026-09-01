//! Provider façades: the native API surfaces IronWire presents on loopback.

pub mod anthropic;
pub mod error;
pub mod openai;

pub use error::FacadeError;

/// The client's own session id, when it sent one.
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
    let raw = headers
        .get(ironwire_upstream::headers::client_session_header(protocol))?
        .to_str()
        .ok()?;
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

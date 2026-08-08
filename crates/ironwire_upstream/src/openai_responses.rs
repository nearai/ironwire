//! Responses-API backends: the ChatGPT/Codex subscription and the metered
//! OpenAI API.
//!
//! Both speak the same wire, which makes them rungs 0–2 of one ladder — falling
//! from the subscription to a key costs nothing but money (`docs/DESIGN.md` §3).
//!
//! The subscription target is `chatgpt.com/backend-api/codex`, the same private
//! surface the Codex CLI uses. It is reached with the credential Codex already
//! stored, and only for requests that arrive as Codex (`docs/TRUST.md` §3).

use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures_util::StreamExt;
use ironwire_core::capability::Capabilities;
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier, Protocol};
use ironwire_core::quota::{Headroom, QuotaSnapshot};
use ironwire_creds::codex::{CHATGPT_BASE_URL, CHATGPT_HOST, CodexCredentials, OPENAI_HOST};
use ironwire_creds::{Bearer, CredentialError};
use secrecy::{ExposeSecret, SecretString};

use crate::backend::{Backend, BackendStatus, UpstreamError, UpstreamRequest, UpstreamResponse};
use crate::headers::forward_response_header;
use crate::observe::{Observation, RateLimitReading, retry_after};

/// Default metered OpenAI endpoint.
pub const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// How a Responses backend authenticates.
#[derive(Clone)]
pub enum ResponsesAuth {
    /// A ChatGPT subscription credential, re-read from `auth.json` on every
    /// request so a background refresh by Codex is picked up.
    ///
    /// The path is explicit rather than implicit so the conformance harness can
    /// run against a fixture instead of whoever's login happens to be on the
    /// machine (`docs/PROTOCOL.md` §7.2). `None` means the real one.
    Subscription {
        /// Override for the `auth.json` location.
        auth_path: Option<std::path::PathBuf>,
    },
    /// A metered OpenAI API key.
    ApiKey(SecretString),
}

/// Capability profile for a current OpenAI reasoning model.
#[must_use]
pub fn responses_capabilities() -> Capabilities {
    Capabilities {
        protocol: Protocol::OpenAiResponses,
        tools: true,
        parallel_tool_calls: true,
        images: true,
        reasoning: true,
        // OpenAI caches automatically; there are no client breakpoints to keep.
        prompt_cache: false,
        structured_output: true,
        context_tokens: 400_000,
    }
}

/// A model catalogue: slug and the quality tier it satisfies.
pub type Catalogue = Vec<(String, ModelTier)>;

/// Models offered over the Responses API, best first.
#[must_use]
pub fn responses_models() -> Catalogue {
    vec![
        ("gpt-5.6".to_string(), ModelTier::Frontier),
        ("gpt-5.6-mini".to_string(), ModelTier::Fast),
    ]
}

/// A Responses-API backend.
pub struct ResponsesBackend {
    id: BackendId,
    name: String,
    kind: BackendKind,
    auth: ResponsesAuth,
    base_url: String,
    client: reqwest::Client,
    capabilities: Capabilities,
    /// Compiled-in catalogue, used until a probe learns better.
    models: Vec<(String, ModelTier)>,
    /// What `/models?client_version=` actually reported for this account.
    ///
    /// The Codex backend gates newer models behind the reported client
    /// version, so the compiled-in list is a guess about someone else's
    /// entitlements. Once we have asked, we use the answer.
    discovered: Arc<Mutex<Option<Catalogue>>>,
    quota: Arc<Mutex<QuotaSnapshot>>,
}

impl ResponsesBackend {
    /// Build the ChatGPT/Codex subscription backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn codex_subscription(
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Self::codex_subscription_at(None, base_url, timeout_secs)
    }

    /// Build the ChatGPT/Codex subscription backend against a specific
    /// `auth.json`. Used by the conformance harness.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn codex_subscription_at(
        auth_path: Option<std::path::PathBuf>,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            id: BackendId::from("codex-sub"),
            name: "ChatGPT subscription".to_string(),
            kind: BackendKind::Subscription,
            auth: ResponsesAuth::Subscription { auth_path },
            base_url: normalize(base_url, CHATGPT_BASE_URL),
            client: build_client(timeout_secs)?,
            capabilities: responses_capabilities(),
            models: responses_models(),
            discovered: Arc::new(Mutex::new(None)),
            quota: Arc::new(Mutex::new(QuotaSnapshot::default())),
        })
    }

    /// Build the metered OpenAI backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn openai_api_key(
        key: SecretString,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            id: BackendId::from("openai-key"),
            name: "OpenAI API".to_string(),
            kind: BackendKind::ApiKey,
            auth: ResponsesAuth::ApiKey(key),
            base_url: normalize(base_url, OPENAI_DEFAULT_BASE_URL),
            client: build_client(timeout_secs)?,
            capabilities: responses_capabilities(),
            models: responses_models(),
            discovered: Arc::new(Mutex::new(None)),
            quota: Arc::new(Mutex::new(QuotaSnapshot::default())),
        })
    }

    /// Models this backend will actually offer.
    fn effective_models(&self) -> Catalogue {
        match self.discovered.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
        .unwrap_or_else(|| self.models.clone())
    }

    /// The credential and, for the subscription, the account it belongs to.
    fn credential(&self) -> Result<(Bearer, Option<String>), CredentialError> {
        match &self.auth {
            ResponsesAuth::Subscription { auth_path } => {
                let creds = CodexCredentials::from_path(auth_path.clone())?;
                if creds.is_expired(Utc::now()) {
                    // Named plainly, because the fix is one command and a bare
                    // 401 would send the user looking at IronWire instead.
                    // IronWire does not refresh this itself — see
                    // `CodexCredentials::is_expired` for why.
                    return Err(CredentialError::NotFound {
                        product: "Codex",
                        locations: format!(
                            "{} — the stored ChatGPT token has expired. \
                             Run `codex` once to refresh it; IronWire will pick \
                             up the new token on the next request.",
                            creds.source
                        ),
                    });
                }
                Ok((creds.bearer(), creds.account_id()))
            }
            ResponsesAuth::ApiKey(key) => Ok((
                Bearer {
                    token: key.clone(),
                    issuer_host: OPENAI_HOST,
                },
                None,
            )),
        }
    }

    /// Assert the credential is going to the host that issued it
    /// (`docs/TRUST.md` I2). A guard against our own bugs.
    fn check_host(&self, bearer: &Bearer) -> Result<(), UpstreamError> {
        let target = self
            .base_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        if target == bearer.issuer_host
            || target.starts_with("127.0.0.1")
            || target.starts_with("localhost")
            || target.starts_with("[::1]")
        {
            return Ok(());
        }
        Err(UpstreamError::CredentialHostMismatch {
            issuer: bearer.issuer_host,
            attempted: target.to_string(),
        })
    }
}

#[async_trait::async_trait]
impl Backend for ResponsesBackend {
    fn id(&self) -> &BackendId {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> BackendKind {
        self.kind
    }

    fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    fn models(&self) -> Vec<(String, ModelTier)> {
        self.effective_models()
    }

    fn requires_client_identity(&self) -> bool {
        // TRUST.md §3: the subscription serves the client it belongs to. The
        // metered key serves anyone.
        matches!(self.auth, ResponsesAuth::Subscription { .. })
    }

    async fn status(&self) -> BackendStatus {
        let (authenticated, detail) = match self.credential() {
            Ok(_) => (true, None),
            Err(e) => (false, Some(e.to_string())),
        };
        BackendStatus {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            authenticated,
            detail,
            quota: self.quota(),
            models: self.effective_models(),
        }
    }

    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let (bearer, account_id) = self.credential().map_err(|e| UpstreamError::NeedsAuth {
            backend: self.id.clone(),
            detail: e.to_string(),
        })?;
        self.check_host(&bearer)?;

        let mut builder = self
            .client
            .post(format!("{}{}", self.base_url, request.path))
            .bearer_auth(bearer.token.expose_secret())
            .header("content-type", "application/json");
        // The ChatGPT backend only speaks SSE.
        if request.stream || matches!(self.auth, ResponsesAuth::Subscription { .. }) {
            builder = builder.header("accept", "text/event-stream");
        }
        for (name, value) in &request.headers {
            // Replaced above; forwarding the client's copy would double them.
            if name == "content-type" || name == "accept" || name == "authorization" {
                continue;
            }
            builder = builder.header(name, value);
        }
        // Codex sends this on its own provider path but not through a custom
        // one, so the header has to be restored here or the subscription
        // request is rejected. It is read from the credential we are presenting
        // (`ironwire_creds::codex::account_id`), never from the request — and
        // never overwritten if the client did supply its own.
        if let Some(account_id) = account_id
            && !request
                .headers
                .iter()
                .any(|(name, _)| name == "chatgpt-account-id")
        {
            builder = builder.header("chatgpt-account-id", account_id);
        }

        let response =
            builder
                .body(request.body)
                .send()
                .await
                .map_err(|e| UpstreamError::Transport {
                    backend: self.id.clone(),
                    detail: e.to_string(),
                })?;

        let status = response.status();
        let headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .filter(|(name, _)| forward_response_header(name.as_str()))
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_string(), v.to_string()))
            })
            .collect();

        let now = Utc::now();
        let observation = Observation {
            primary: chatgpt_rate_limit(&headers, "primary"),
            secondary: chatgpt_rate_limit(&headers, "secondary"),
            retry_after_secs: retry_after(&headers, now),
            ..Observation::default()
        };
        if !observation.is_empty() {
            self.record(&observation);
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(UpstreamError::RateLimited {
                backend: self.id.clone(),
                retry_after_secs: observation.retry_after_secs,
            });
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            let body = response.bytes().await.unwrap_or_default();
            return Err(UpstreamError::NeedsAuth {
                backend: self.id.clone(),
                detail: String::from_utf8_lossy(&body).chars().take(400).collect(),
            });
        }
        if status.is_server_error() {
            let body = response.bytes().await.unwrap_or_default();
            return Err(UpstreamError::Upstream {
                backend: self.id.clone(),
                status: http::StatusCode::from_u16(status.as_u16())
                    .unwrap_or(http::StatusCode::BAD_GATEWAY),
                body,
            });
        }

        let backend = self.id.clone();
        let body = response
            .bytes_stream()
            .map(move |chunk| {
                chunk.map_err(|e| UpstreamError::Transport {
                    backend: backend.clone(),
                    detail: e.to_string(),
                })
            })
            .boxed();

        Ok(UpstreamResponse {
            status: http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(http::StatusCode::BAD_GATEWAY),
            headers,
            body,
        })
    }

    fn record(&self, observation: &Observation) {
        let now = Utc::now();
        let mut quota = match self.quota.lock() {
            Ok(q) => q,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(reading) = observation.primary.clone() {
            quota.primary = reading.into_headroom(now);
        }
        if let Some(reading) = observation.secondary.clone() {
            quota.secondary = Some(reading.into_headroom(now));
        }
        if let Some(secs) = observation.retry_after_secs {
            quota.primary = Headroom::Exhausted {
                until: now + chrono::Duration::seconds(i64::try_from(secs).unwrap_or(i64::MAX)),
            };
        }
    }

    fn quota(&self) -> QuotaSnapshot {
        match self.quota.lock() {
            Ok(q) => q.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    async fn probe(&self) -> Result<(), UpstreamError> {
        let (bearer, _) = self.credential().map_err(|e| UpstreamError::NeedsAuth {
            backend: self.id.clone(),
            detail: e.to_string(),
        })?;
        self.check_host(&bearer)?;
        // Auth-only, for the same reason as the Anthropic probe: a real request
        // against the subscription would have to carry Codex's identity, and
        // synthesising that is what `docs/TRUST.md` §3 forbids.
        //
        // The client version matters here and nowhere else: the backend gates
        // newer models behind it, so asking with a stale one silently returns a
        // shorter list and IronWire would offer fewer models than Codex does
        // for the same account (`crate::codex_version`).
        let url = if matches!(self.auth, ResponsesAuth::Subscription { .. }) {
            let version = crate::codex_version::client_version().await;
            format!("{}/models?client_version={version}", self.base_url)
        } else {
            format!("{}/models", self.base_url)
        };

        let response = self
            .client
            .get(url)
            .bearer_auth(bearer.token.expose_secret())
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| UpstreamError::Transport {
                backend: self.id.clone(),
                detail: e.to_string(),
            })?;
        let status = response.status();
        if status.is_success() {
            // Remember what this account is actually entitled to, rather than
            // continuing to offer a list compiled in months ago.
            if let Ok(body) = response.bytes().await
                && let Some(models) = parse_model_list(&body)
                && !models.is_empty()
            {
                tracing::info!(
                    backend = %self.id,
                    count = models.len(),
                    "learned the model catalogue from the provider"
                );
                match self.discovered.lock() {
                    Ok(mut guard) => *guard = Some(models),
                    Err(poisoned) => *poisoned.into_inner() = Some(models),
                }
            }
            return Ok(());
        }
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(UpstreamError::NeedsAuth {
                backend: self.id.clone(),
                detail: "the stored credential was rejected; re-run `codex login`".to_string(),
            });
        }
        Err(UpstreamError::Upstream {
            backend: self.id.clone(),
            status: http::StatusCode::from_u16(status.as_u16())
                .unwrap_or(http::StatusCode::BAD_GATEWAY),
            body: response.bytes().await.unwrap_or_default(),
        })
    }
}

/// Read one of the ChatGPT backend's rate-limit windows.
///
/// It reports usage as a percentage directly, which is the shape we want —
/// unlike Anthropic, nothing has to be derived. A window the provider did not
/// report stays `None` and shows as `unknown` (`docs/CRITIQUE.md` §4).
#[must_use]
pub fn chatgpt_rate_limit(headers: &[(String, String)], window: &str) -> Option<RateLimitReading> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    let used = get(&format!("x-codex-{window}-used-percent"))?
        .parse::<f32>()
        .ok()?;
    let resets_at = get(&format!("x-codex-{window}-reset-after-seconds"))
        .and_then(|v| v.parse::<i64>().ok())
        .map(|secs| Utc::now() + chrono::Duration::seconds(secs));
    Some(RateLimitReading {
        used_pct: used.clamp(0.0, 100.0),
        resets_at,
    })
}

/// Read a `/models` response into a catalogue.
///
/// Tolerant on purpose: this endpoint is undocumented and its shape is not ours
/// to rely on. A body we cannot read leaves the compiled-in list in force,
/// which is the same position we were in before asking.
#[must_use]
pub fn parse_model_list(body: &[u8]) -> Option<Catalogue> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let items = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array())?;

    Some(
        items
            .iter()
            .filter_map(|item| {
                let id = item
                    .get("id")
                    .or_else(|| item.get("slug"))
                    .or_else(|| item.get("model"))
                    .and_then(serde_json::Value::as_str)
                    .or_else(|| item.as_str())?;
                Some((id.to_string(), ModelTier::from_model_hint(id)))
            })
            .collect(),
    )
}

fn normalize(base: Option<String>, default: &str) -> String {
    base.unwrap_or_else(|| default.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn build_client(timeout_secs: u64) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // No total-request timeout: a coding agent legitimately generates for
        // many minutes. The read timeout catches a genuinely dead connection.
        .read_timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
}

/// The host a ChatGPT subscription credential belongs to.
#[must_use]
pub fn chatgpt_host() -> &'static str {
    CHATGPT_HOST
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription(base: &str) -> ResponsesBackend {
        ResponsesBackend::codex_subscription(Some(base.to_string()), 60).expect("client builds")
    }

    #[test]
    fn only_the_subscription_demands_a_matching_client_identity() {
        assert!(subscription(CHATGPT_BASE_URL).requires_client_identity());
        let key = ResponsesBackend::openai_api_key(SecretString::from("sk-x"), None, 60)
            .expect("client builds");
        assert!(!key.requires_client_identity());
    }

    #[test]
    fn a_credential_is_refused_for_a_host_that_did_not_issue_it() {
        // TRUST.md I2 — a misconfigured base URL must not exfiltrate a token.
        let backend = subscription("https://evil.example");
        let bearer = Bearer {
            token: SecretString::from("eyJACCESS"),
            issuer_host: CHATGPT_HOST,
        };
        assert!(matches!(
            backend.check_host(&bearer),
            Err(UpstreamError::CredentialHostMismatch { .. })
        ));
    }

    #[test]
    fn the_two_backends_share_a_wire_so_they_are_one_ladder() {
        let sub = subscription(CHATGPT_BASE_URL);
        let key = ResponsesBackend::openai_api_key(SecretString::from("sk-x"), None, 60)
            .expect("client builds");
        assert_eq!(
            sub.capabilities().protocol,
            key.capabilities().protocol,
            "falling from one to the other must need no translation"
        );
        assert_eq!(sub.kind(), BackendKind::Subscription);
        assert_eq!(key.kind(), BackendKind::ApiKey);
    }

    #[test]
    fn both_rate_limit_windows_are_read_when_reported() {
        // The ChatGPT backend meters a short and a long window; a request is
        // blocked if either is exhausted.
        let headers = [
            ("x-codex-primary-used-percent", "72"),
            ("x-codex-primary-reset-after-seconds", "1800"),
            ("x-codex-secondary-used-percent", "31"),
        ]
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect::<Vec<_>>();

        let primary = chatgpt_rate_limit(&headers, "primary").expect("primary reported");
        assert!((primary.used_pct - 72.0).abs() < 0.01);
        assert!(primary.resets_at.is_some());
        let secondary = chatgpt_rate_limit(&headers, "secondary").expect("secondary reported");
        assert!((secondary.used_pct - 31.0).abs() < 0.01);
        assert!(secondary.resets_at.is_none());
    }

    #[test]
    fn an_unreported_window_is_unknown_rather_than_zero() {
        assert!(chatgpt_rate_limit(&[], "primary").is_none());
    }

    #[test]
    fn a_secondary_window_can_block_an_otherwise_healthy_backend() {
        let backend = subscription(CHATGPT_BASE_URL);
        backend.record(&Observation {
            primary: Some(RateLimitReading {
                used_pct: 10.0,
                resets_at: None,
            }),
            secondary: Some(RateLimitReading {
                used_pct: 99.0,
                resets_at: None,
            }),
            ..Observation::default()
        });
        assert!(backend.quota().is_pressured(Utc::now()));
    }
}

#[cfg(test)]
mod catalogue_tests {
    use super::*;

    #[test]
    fn the_openai_list_shape_parses() {
        let body = br#"{"object":"list","data":[{"id":"gpt-5.6"},{"id":"gpt-5.6-mini"}]}"#;
        let models = parse_model_list(body).expect("parses");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].0, "gpt-5.6");
    }

    #[test]
    fn a_bare_array_of_strings_parses_too() {
        // The Codex backend is undocumented and its shape is not ours to rely
        // on. Reading more shapes costs nothing and avoids a silent downgrade.
        let models = parse_model_list(br#"["gpt-5.6","gpt-5.6-mini"]"#).expect("parses");
        assert_eq!(models.len(), 2);
    }

    #[test]
    fn a_body_we_cannot_read_leaves_the_compiled_in_list_in_force() {
        // Which is exactly where we were before asking — no worse.
        assert!(parse_model_list(b"not json").is_none());
        assert!(parse_model_list(b"{}").is_none());
    }

    #[test]
    fn a_discovered_catalogue_replaces_the_compiled_in_one() {
        let backend = ResponsesBackend::codex_subscription(Some(CHATGPT_BASE_URL.into()), 60)
            .expect("client builds");
        let before = backend.models();
        assert!(!before.is_empty(), "there is always a fallback");

        *backend.discovered.lock().expect("lock") =
            Some(vec![("gpt-6-preview".to_string(), ModelTier::Frontier)]);
        let after = backend.models();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].0, "gpt-6-preview");
    }

    #[test]
    fn an_empty_discovery_is_ignored_rather_than_leaving_no_models() {
        // A backend offering nothing is a backend that can never be routed to.
        // Better to keep a stale list than to have none.
        let body = br#"{"data":[]}"#;
        let models = parse_model_list(body).expect("parses");
        assert!(models.is_empty(), "the parse itself is faithful");
        // ...and `probe` refuses to install an empty list; see the `!models
        // .is_empty()` guard there.
    }
}

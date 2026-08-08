//! Anthropic backends: the Claude subscription and the metered API key.
//!
//! Both speak the same wire, which is what makes them rungs 0–2 of the same
//! ladder: falling from the subscription to an API key costs a cold cache and
//! nothing else — no translation, no dropped reasoning (`docs/DESIGN.md` §3).

use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures_util::StreamExt;
use ironwire_core::capability::Capabilities;
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier, Protocol};
use ironwire_core::quota::{Headroom, QuotaSnapshot};
use ironwire_creds::claude::{ANTHROPIC_HOST, ClaudeCodeCredentials};
use ironwire_creds::{Bearer, CredentialError};
use ironwire_quirks::AnthropicQuirks;
use secrecy::{ExposeSecret, SecretString};

use crate::backend::{Backend, BackendStatus, UpstreamError, UpstreamRequest, UpstreamResponse};
use crate::headers::forward_response_header;
use crate::observe::{Observation, anthropic_rate_limit, retry_after};

/// Default base URL for the Anthropic API.
pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// How a given Anthropic backend authenticates.
#[derive(Clone)]
pub enum AnthropicAuth {
    /// A Claude Code subscription token, re-read from the credential store on
    /// every request so a background refresh by Claude Code is picked up.
    Subscription,
    /// A metered API key.
    ApiKey(SecretString),
}

/// Capability profile shared by every current Anthropic model.
#[must_use]
pub fn anthropic_capabilities() -> Capabilities {
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

/// Models this backend offers, best first.
#[must_use]
pub fn anthropic_models() -> Vec<(String, ModelTier)> {
    vec![
        ("claude-opus-4-6".to_string(), ModelTier::Frontier),
        ("claude-sonnet-4-6".to_string(), ModelTier::Balanced),
        ("claude-haiku-4-5".to_string(), ModelTier::Fast),
    ]
}

/// An Anthropic-family backend.
pub struct AnthropicBackend {
    id: BackendId,
    name: String,
    kind: BackendKind,
    auth: AnthropicAuth,
    base_url: String,
    client: reqwest::Client,
    capabilities: Capabilities,
    /// Compiled-in catalogue, used until a probe learns better.
    models: Vec<(String, ModelTier)>,
    /// What `GET /v1/models` actually reported for this account.
    ///
    /// The probe already fetches this list to check the credential; not reading
    /// it left `ironwire status` naming models from whenever the binary was
    /// built, months after the account gained newer ones.
    discovered: Arc<Mutex<Option<Vec<(String, ModelTier)>>>>,
    quota: Arc<Mutex<QuotaSnapshot>>,
    /// Protocol constants. These are the values most likely to change under us,
    /// so they come from the signed quirks channel rather than a release
    /// (`docs/UPDATES.md`). The *host* deliberately does not.
    quirks: AnthropicQuirks,
}

impl AnthropicBackend {
    /// Replace the protocol constants with a newer signed set.
    #[must_use]
    pub fn with_quirks(mut self, quirks: AnthropicQuirks) -> Self {
        self.quirks = quirks;
        self
    }

    /// Build a subscription-backed Anthropic backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn subscription(base_url: Option<String>, timeout_secs: u64) -> reqwest::Result<Self> {
        Ok(Self {
            id: BackendId::from("claude-sub"),
            name: "Claude subscription".to_string(),
            kind: BackendKind::Subscription,
            auth: AnthropicAuth::Subscription,
            base_url: normalize_base(base_url),
            client: build_client(timeout_secs)?,
            capabilities: anthropic_capabilities(),
            models: anthropic_models(),
            discovered: Arc::new(Mutex::new(None)),
            quota: Arc::new(Mutex::new(QuotaSnapshot::default())),
            quirks: AnthropicQuirks::default(),
        })
    }

    /// Build an API-key-backed Anthropic backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn api_key(
        key: SecretString,
        base_url: Option<String>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            id: BackendId::from("anthropic-key"),
            name: "Anthropic API".to_string(),
            kind: BackendKind::ApiKey,
            auth: AnthropicAuth::ApiKey(key),
            base_url: normalize_base(base_url),
            client: build_client(timeout_secs)?,
            capabilities: anthropic_capabilities(),
            models: anthropic_models(),
            discovered: Arc::new(Mutex::new(None)),
            quota: Arc::new(Mutex::new(QuotaSnapshot::default())),
            quirks: AnthropicQuirks::default(),
        })
    }

    /// Resolve the credential for this request.
    ///
    /// The subscription token is re-read every time rather than cached: Claude
    /// Code refreshes it in the background, and a cached copy would go stale in
    /// a way that looks like a rate limit.
    fn credential(&self) -> Result<Bearer, CredentialError> {
        match &self.auth {
            AnthropicAuth::Subscription => Ok(ClaudeCodeCredentials::discover()?.bearer()),
            AnthropicAuth::ApiKey(key) => Ok(Bearer {
                token: key.clone(),
                issuer_host: ANTHROPIC_HOST,
            }),
        }
    }

    /// Assert the credential is going to the host that issued it
    /// (`docs/TRUST.md` I2). This is a guard against our own bugs, not against
    /// the user.
    fn check_host(&self, bearer: &Bearer) -> Result<(), UpstreamError> {
        let target = self
            .base_url
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split('/').next())
            .unwrap_or_default();
        // A localhost base URL is a test double; the rule is about not leaking
        // a credential to a *third party*, and loopback is not one.
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
impl Backend for AnthropicBackend {
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
        match self.discovered.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
        .unwrap_or_else(|| self.models.clone())
    }

    fn catalogue_from_provider(&self) -> bool {
        match self.discovered.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    fn requires_client_identity(&self) -> bool {
        // TRUST.md §3: the subscription serves the client it belongs to. The
        // metered key serves anyone.
        matches!(self.auth, AnthropicAuth::Subscription)
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
            models: self.models(),
        }
    }

    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let bearer = self.credential().map_err(|e| UpstreamError::NeedsAuth {
            backend: self.id.clone(),
            detail: e.to_string(),
        })?;
        self.check_host(&bearer)?;

        match self.send_once(request.clone(), &bearer).await {
            // A 401 on a subscription usually means Claude Code rotated the
            // token between our read and our send. Re-read and retry once.
            //
            // We deliberately do *not* drive a refresh ourselves. Doing so
            // means writing to another product's credential store, and racing
            // its own rotation could invalidate the user's Claude Code session
            // — breaking the thing they actually care about in order to fix
            // something they did not ask us to fix. Re-reading is enough,
            // because Claude Code refreshes in the background anyway.
            Err(UpstreamError::NeedsAuth { .. })
                if matches!(self.auth, AnthropicAuth::Subscription) =>
            {
                let refreshed = self.credential().map_err(|e| UpstreamError::NeedsAuth {
                    backend: self.id.clone(),
                    detail: e.to_string(),
                })?;
                if refreshed.token.expose_secret() == bearer.token.expose_secret() {
                    // Same token, same answer. Retrying would just burn a
                    // request and delay telling the user to re-authenticate.
                    return Err(UpstreamError::NeedsAuth {
                        backend: self.id.clone(),
                        detail: "the stored Claude Code token was rejected; \
                                 re-authenticate by running `claude` and logging in"
                            .to_string(),
                    });
                }
                tracing::debug!(
                    backend = %self.id,
                    "credential rotated under us; retrying once with the fresh token"
                );
                self.send_once(request, &refreshed).await
            }
            other => other,
        }
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
        if let Some(secs) = observation.retry_after_secs {
            // A provider-stated wait always wins over a percentage: it is the
            // more specific fact, and it is the one that is actionable.
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
        let bearer = self.credential().map_err(|e| UpstreamError::NeedsAuth {
            backend: self.id.clone(),
            detail: e.to_string(),
        })?;
        self.check_host(&bearer)?;

        // `GET /v1/models` validates the credential without an inference call.
        //
        // For the subscription backend this is the *only* honest probe: a real
        // message would have to carry Claude Code's identity to be accepted,
        // and synthesising that identity is exactly what `docs/TRUST.md` §3
        // forbids. Checking auth without pretending to be another product is
        // both cheaper and the only version we are willing to ship.
        let mut builder = self
            .client
            .get(format!("{}/v1/models", self.base_url))
            .header("anthropic-version", &self.quirks.api_version)
            .timeout(std::time::Duration::from_secs(15));
        builder = match &self.auth {
            AnthropicAuth::Subscription => builder
                .header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", bearer.token.expose_secret()),
                )
                .header("anthropic-beta", &self.quirks.oauth_beta),
            AnthropicAuth::ApiKey(_) => builder.header("x-api-key", bearer.token.expose_secret()),
        };

        let response = builder.send().await.map_err(|e| UpstreamError::Transport {
            backend: self.id.clone(),
            detail: e.to_string(),
        })?;
        let status = response.status();
        if status.is_success() {
            // The list is already in our hands from the auth check; reading it
            // is what keeps `status` from naming a catalogue frozen at build
            // time. Ordered as the provider ordered it — newest first.
            if let Ok(body) = response.bytes().await
                && let Some(models) = crate::openai_responses::parse_model_list(&body)
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
            let body = response.bytes().await.unwrap_or_default();
            return Err(UpstreamError::NeedsAuth {
                backend: self.id.clone(),
                detail: String::from_utf8_lossy(&body).chars().take(200).collect(),
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

impl AnthropicBackend {
    /// One attempt with a specific credential. No retry, no failover — both are
    /// the router's decision, because only it knows whether a byte has already
    /// reached the client (`docs/PROTOCOL.md` §5).
    async fn send_once(
        &self,
        request: UpstreamRequest,
        bearer: &Bearer,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let url = format!("{}{}", self.base_url, request.path);
        let mut builder = self.client.post(&url);

        // Forward everything the client sent that survived the header filter.
        // Track whether the client supplied the protocol headers so we only
        // add defaults when it did not — overriding a client's own
        // `anthropic-version` would be a mutation we did not enumerate.
        let mut saw_version = false;
        let mut saw_beta: Option<String> = None;
        for (name, value) in &request.headers {
            if name == "anthropic-version" {
                saw_version = true;
            }
            if name == "anthropic-beta" {
                saw_beta = Some(value.clone());
                continue; // re-added below, possibly extended
            }
            builder = builder.header(name, value);
        }

        match &self.auth {
            AnthropicAuth::Subscription => {
                builder = builder.header(
                    reqwest::header::AUTHORIZATION,
                    format!("Bearer {}", bearer.token.expose_secret()),
                );
                // OAuth bearer auth is gated behind this beta flag; without it
                // the API answers 401. Append rather than replace so the
                // client's own beta flags survive.
                let beta = match saw_beta {
                    Some(existing) if existing.contains(&self.quirks.oauth_beta) => existing,
                    Some(existing) => format!("{existing},{}", self.quirks.oauth_beta),
                    None => self.quirks.oauth_beta.to_string(),
                };
                builder = builder.header("anthropic-beta", beta);
                if !saw_version {
                    builder = builder.header("anthropic-version", &self.quirks.api_version);
                }
            }
            AnthropicAuth::ApiKey(_) => {
                builder = builder.header("x-api-key", bearer.token.expose_secret());
                if let Some(beta) = saw_beta {
                    builder = builder.header("anthropic-beta", beta);
                }
                if !saw_version {
                    builder = builder.header("anthropic-version", &self.quirks.api_version);
                }
            }
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

        // Fold rate-limit state in before deciding what to do with the status:
        // a 429 tells us at least as much as a 200.
        let now = Utc::now();
        let observation = Observation {
            primary: anthropic_rate_limit(&headers),
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

        // Client errors (4xx other than the two above) are the client's own
        // request being wrong. Pass them through as a normal response so the
        // agent sees exactly what the provider said.
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
}

fn normalize_base(base: Option<String>) -> String {
    base.unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn build_client(timeout_secs: u64) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        // No total-request timeout: a coding agent legitimately generates for
        // many minutes, and cutting it off mid-stream is worse than waiting.
        // The read timeout catches a genuinely dead connection.
        .read_timeout(std::time::Duration::from_secs(timeout_secs))
        .connect_timeout(std::time::Duration::from_secs(15))
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subscription_backend(base: &str) -> AnthropicBackend {
        AnthropicBackend::subscription(Some(base.to_string()), 60).expect("client builds")
    }

    #[test]
    fn only_the_subscription_demands_a_matching_client_identity() {
        let sub = subscription_backend(DEFAULT_BASE_URL);
        assert!(sub.requires_client_identity());

        let key = AnthropicBackend::api_key(SecretString::from("sk-ant-x"), None, 60)
            .expect("client builds");
        assert!(!key.requires_client_identity());
    }

    #[test]
    fn a_credential_is_refused_for_a_host_that_did_not_issue_it() {
        // TRUST.md I2 — a misconfigured base_url must not exfiltrate a token.
        let backend = subscription_backend("https://evil.example");
        let bearer = Bearer {
            token: SecretString::from("sk-ant-oat01-x"),
            issuer_host: ANTHROPIC_HOST,
        };
        let err = backend
            .check_host(&bearer)
            .expect_err("must refuse a foreign host");
        assert!(matches!(err, UpstreamError::CredentialHostMismatch { .. }));
    }

    #[test]
    fn the_issuers_own_host_is_accepted() {
        let backend = subscription_backend(DEFAULT_BASE_URL);
        let bearer = Bearer {
            token: SecretString::from("sk-ant-oat01-x"),
            issuer_host: ANTHROPIC_HOST,
        };
        assert!(backend.check_host(&bearer).is_ok());
    }

    #[test]
    fn loopback_is_allowed_so_the_conformance_harness_can_run() {
        let backend = subscription_backend("http://127.0.0.1:9999");
        let bearer = Bearer {
            token: SecretString::from("sk-ant-oat01-x"),
            issuer_host: ANTHROPIC_HOST,
        };
        assert!(backend.check_host(&bearer).is_ok());
    }

    #[test]
    fn base_urls_normalise_to_one_form() {
        assert_eq!(normalize_base(None), DEFAULT_BASE_URL);
        assert_eq!(
            normalize_base(Some("https://example.test/".to_string())),
            "https://example.test"
        );
    }

    #[test]
    fn a_provider_stated_wait_overrides_a_percentage() {
        let backend = subscription_backend(DEFAULT_BASE_URL);
        backend.record(&Observation {
            primary: Some(crate::observe::RateLimitReading {
                used_pct: 50.0,
                resets_at: None,
            }),
            retry_after_secs: Some(120),
            ..Observation::default()
        });
        let quota = backend.quota();
        assert!(
            matches!(quota.primary, Headroom::Exhausted { .. }),
            "a concrete retry-after must win over a percentage"
        );
        assert!(!quota.is_available(Utc::now()));
    }

    #[test]
    fn quota_starts_unknown_rather_than_optimistic() {
        let backend = subscription_backend(DEFAULT_BASE_URL);
        assert_eq!(backend.quota().primary, Headroom::Unknown);
        assert!(backend.quota().is_available(Utc::now()));
    }
}

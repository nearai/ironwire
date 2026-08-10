//! OpenAI Chat Completions backends — NEAR AI, and any OpenAI-compatible
//! endpoint the user points us at.
//!
//! This is the target of the translated lane. It speaks a different family from
//! the Anthropic façade, so requests reach it only after
//! `ironwire_core::capability::eligible` has cleared a cross-family switch.

use std::sync::{Arc, Mutex};

use chrono::Utc;
use futures_util::StreamExt;
use ironwire_core::capability::Capabilities;
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier, Protocol};
use ironwire_core::quota::{Headroom, QuotaSnapshot};
use ironwire_creds::Bearer;
use secrecy::{ExposeSecret, SecretString};

use crate::backend::{Backend, BackendStatus, UpstreamError, UpstreamRequest, UpstreamResponse};
use crate::headers::forward_response_header;
use crate::observe::{Observation, retry_after};

/// NEAR AI's OpenAI-compatible inference endpoint.
pub const NEARAI_DEFAULT_BASE_URL: &str = "https://cloud-api.near.ai/v1";

/// Capability profile for a modern OSS model served over Chat Completions.
///
/// `reasoning` and `prompt_cache` are false, and that is now only a *quality*
/// signal — neither blocks a route (`ironwire_core::capability`). `images` is
/// false because a text-only model genuinely cannot see them, which does.
#[must_use]
pub fn chat_capabilities(context_tokens: u32) -> Capabilities {
    Capabilities {
        protocol: Protocol::OpenAiChat,
        tools: true,
        parallel_tool_calls: true,
        images: false,
        reasoning: false,
        prompt_cache: false,
        structured_output: false,
        context_tokens,
    }
}

/// An OpenAI-compatible Chat Completions backend.
pub struct ChatCompletionsBackend {
    id: BackendId,
    name: String,
    kind: BackendKind,
    /// `None` for a local server that takes no auth, which is most of them.
    /// Absent means no `Authorization` header at all — not an empty one.
    api_key: Option<SecretString>,
    issuer_host: String,
    base_url: String,
    client: reqwest::Client,
    capabilities: Capabilities,
    /// Configured catalogue, used until a probe learns better.
    models: Vec<(String, ModelTier)>,
    /// What the endpoint itself reported.
    ///
    /// An OpenAI-compatible endpoint is somebody else's product with somebody
    /// else's release schedule; a list configured here is a guess that goes
    /// stale silently. Once we have asked, we use the answer.
    discovered: Arc<Mutex<Option<crate::Catalogue>>>,
    quota: Arc<Mutex<QuotaSnapshot>>,
}

impl ChatCompletionsBackend {
    /// Build a NEAR AI backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    /// The key is optional so the backend can be registered without one. A
    /// credits backend with no key reports `authenticated: false` and shows up
    /// in `status` as something to connect, which is the contract
    /// `build_registry` states: a backend nobody has a credential for is still
    /// a backend the user should be able to see IronWire knows about.
    pub fn nearai(
        api_key: Option<SecretString>,
        base_url: Option<String>,
        models: Vec<(String, ModelTier)>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Self::new(
            BackendId::from("nearai"),
            "NEAR AI",
            BackendKind::Credits,
            api_key,
            base_url.unwrap_or_else(|| NEARAI_DEFAULT_BASE_URL.to_string()),
            models,
            timeout_secs,
        )
    }

    /// Build an arbitrary OpenAI-compatible backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    /// Build a backend for a model running on this machine or LAN.
    ///
    /// `BackendKind::Local` rather than `Credits`: free at the margin, not
    /// metered, and no consent gate. `api_key` is optional because most local
    /// servers take no auth — and `None` means no header, not an empty one.
    ///
    /// The base URL must include the OpenAI-compatible prefix, usually `/v1`.
    /// Ollama's native `/api/*` is a different protocol and is not supported.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn local(
        id: BackendId,
        name: &str,
        base_url: String,
        api_key: Option<SecretString>,
        models: Vec<(String, ModelTier)>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        Self::new(
            id,
            name,
            BackendKind::Local,
            api_key,
            base_url,
            models,
            timeout_secs,
        )
    }

    /// Build an arbitrary OpenAI-compatible backend.
    ///
    /// # Errors
    ///
    /// Propagates a reqwest client build failure.
    pub fn new(
        id: BackendId,
        name: &str,
        kind: BackendKind,
        api_key: Option<SecretString>,
        base_url: String,
        models: Vec<(String, ModelTier)>,
        timeout_secs: u64,
    ) -> reqwest::Result<Self> {
        let base_url = base_url.trim_end_matches('/').to_string();
        // A credential is only ever attached to the host it was configured for
        // (`docs/TRUST.md` I2). For a user-supplied endpoint that host *is* the
        // configured base URL, so derive it rather than hardcoding one.
        let issuer_host = host_of(&base_url);
        Ok(Self {
            id,
            name: name.to_string(),
            kind,
            api_key,
            issuer_host,
            base_url,
            client: reqwest::Client::builder()
                .read_timeout(std::time::Duration::from_secs(timeout_secs))
                .connect_timeout(std::time::Duration::from_secs(15))
                .pool_idle_timeout(std::time::Duration::from_secs(90))
                .build()?,
            capabilities: chat_capabilities(128_000),
            models,
            discovered: Arc::new(Mutex::new(None)),
            quota: Arc::new(Mutex::new(QuotaSnapshot::default())),
        })
    }

    /// Override the capability profile (context window, modalities).
    #[must_use]
    pub fn with_capabilities(mut self, capabilities: Capabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn bearer(&self) -> Option<Bearer> {
        Some(Bearer {
            token: self.api_key.clone()?,
            // Leaked to a `&'static str` because `Bearer` binds a credential to
            // a compile-time host for the first-party backends; a user-supplied
            // endpoint is configured once at startup, so this leaks once per
            // backend rather than per request.
            issuer_host: Box::leak(self.issuer_host.clone().into_boxed_str()),
        })
    }

    /// Attach the credential, if there is one.
    ///
    /// A local server usually takes none, and sending `Authorization: Bearer `
    /// with nothing after it is not the same as sending no header — some
    /// servers reject it outright.
    fn authorize(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.bearer() {
            Some(bearer) => builder.bearer_auth(bearer.token.expose_secret()),
            None => builder,
        }
    }
}

fn host_of(base_url: &str) -> String {
    base_url
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split('/').next())
        .unwrap_or(base_url)
        .to_string()
}

#[async_trait::async_trait]
impl Backend for ChatCompletionsBackend {
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

    async fn status(&self) -> BackendStatus {
        // Local capacity needs no credential, so "no key" is its ordinary
        // state rather than a fault to report.
        let authenticated = self.kind == BackendKind::Local
            || self
                .api_key
                .as_ref()
                .is_some_and(|key| !key.expose_secret().is_empty());
        BackendStatus {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: self.kind,
            authenticated,
            detail: (!authenticated).then(|| "no API key configured".to_string()),
            quota: self.quota(),
            models: self.models(),
        }
    }

    async fn send(&self, request: UpstreamRequest) -> Result<UpstreamResponse, UpstreamError> {
        let url = crate::endpoint_url(&self.base_url, &request.path);
        let mut builder = self
            .authorize(self.client.post(&url))
            .header("content-type", "application/json");
        for (name, value) in &request.headers {
            // The translated lane rebuilds the body, so inbound provider
            // headers describe a protocol this backend does not speak.
            if name.starts_with("anthropic-") || name == "content-type" {
                continue;
            }
            builder = builder.header(name, value);
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

        let observation = Observation {
            retry_after_secs: retry_after(&headers, Utc::now()),
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
        if let Some(secs) = observation.retry_after_secs {
            quota.primary = Headroom::Exhausted {
                // Clamped at the parse site too (`observe::retry_after`); this
                // is belt and braces, because `chrono` panics rather than
                // saturating and the input is upstream-controlled.
                until: now
                    + chrono::Duration::seconds(
                        i64::try_from(secs.min(crate::observe::MAX_RETRY_AFTER_SECS))
                            .unwrap_or(86_400),
                    ),
            };
        }
    }

    fn restore_quota(&self, snapshot: QuotaSnapshot) {
        match self.quota.lock() {
            Ok(mut guard) => *guard = snapshot,
            Err(poisoned) => *poisoned.into_inner() = snapshot,
        }
    }

    fn quota(&self) -> QuotaSnapshot {
        match self.quota.lock() {
            Ok(q) => q.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    async fn probe(&self) -> Result<(), UpstreamError> {
        let response = self
            .authorize(self.client.get(format!("{}/models", self.base_url)))
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .map_err(|e| UpstreamError::Transport {
                backend: self.id.clone(),
                detail: e.to_string(),
            })?;
        let status = response.status();
        if status.is_success() {
            // The probe already has the answer in its hand; parsing it is what
            // turns a configured guess into what the endpoint actually serves.
            if let Ok(body) = response.bytes().await
                && let Some(models) = crate::openai_responses::parse_model_list(&body)
                && !models.is_empty()
            {
                // A discovered local model is `Fast`, whatever its name
                // suggests. `from_model_hint` resolves an unrecognised slug to
                // `Frontier` — right for a hosted catalogue, catastrophic here,
                // because local capacity also sorts cheapest and a
                // `qwen3-coder:30b` reading as frontier-tier would take work
                // meant for Opus. The user opts a local model up the ladder by
                // declaring a tier in config; discovery never does.
                let models: crate::Catalogue = if self.kind == BackendKind::Local {
                    models
                        .into_iter()
                        .map(|(name, _)| (name, ModelTier::Fast))
                        .collect()
                } else {
                    models
                };
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
                detail: "the configured API key was rejected".to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(base: &str) -> ChatCompletionsBackend {
        ChatCompletionsBackend::nearai(
            Some(SecretString::from("near-key")),
            Some(base.to_string()),
            vec![("near-x".to_string(), ModelTier::Balanced)],
            60,
        )
        .expect("client builds")
    }

    #[test]
    fn the_credential_is_bound_to_the_configured_endpoint() {
        // TRUST.md I2 for a user-supplied host: derived, not hardcoded.
        let b = backend("https://api.near.ai/v1");
        assert_eq!(b.bearer().expect("a key").issuer_host, "api.near.ai");
        let b = backend("http://127.0.0.1:9000/v1");
        assert_eq!(b.bearer().expect("a key").issuer_host, "127.0.0.1:9000");
    }

    #[test]
    fn near_ai_is_credit_capacity_and_needs_no_client_identity() {
        let b = backend(NEARAI_DEFAULT_BASE_URL);
        assert_eq!(b.kind(), BackendKind::Credits);
        assert!(!b.requires_client_identity());
        assert_eq!(b.capabilities().protocol, Protocol::OpenAiChat);
    }

    #[test]
    fn a_chat_backend_is_a_different_family_from_the_anthropic_facade() {
        // Which is what makes it the translated lane's target.
        let b = backend(NEARAI_DEFAULT_BASE_URL);
        assert_ne!(
            b.capabilities().protocol.family(),
            Protocol::AnthropicMessages.family()
        );
    }

    #[test]
    fn trailing_slashes_do_not_double_up_in_the_url() {
        let b = backend("https://api.near.ai/v1/");
        assert_eq!(b.base_url, "https://api.near.ai/v1");
    }

    #[test]
    fn a_provider_stated_wait_marks_the_backend_exhausted() {
        let b = backend(NEARAI_DEFAULT_BASE_URL);
        b.record(&Observation {
            retry_after_secs: Some(60),
            ..Observation::default()
        });
        assert!(!b.quota().is_available(Utc::now()));
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;

    fn local(models: Vec<(String, ModelTier)>) -> ChatCompletionsBackend {
        ChatCompletionsBackend::local(
            BackendId::from("ollama"),
            "ollama",
            "http://127.0.0.1:11434/v1".to_string(),
            None,
            models,
            60,
        )
        .expect("client builds")
    }

    /// Most local servers take no auth, and `Authorization: Bearer ` with
    /// nothing after it is not the same as sending no header — some reject it.
    #[test]
    fn a_local_backend_without_a_key_carries_no_credential() {
        assert!(local(Vec::new()).bearer().is_none());
    }

    #[test]
    fn a_local_backend_with_a_key_is_bound_to_its_own_host() {
        let backend = ChatCompletionsBackend::local(
            BackendId::from("ollama"),
            "ollama",
            "http://127.0.0.1:11434/v1".to_string(),
            Some(SecretString::from("sk-local")),
            Vec::new(),
            60,
        )
        .expect("client builds");
        let bearer = backend.bearer().expect("a key was configured");
        assert_eq!(bearer.issuer_host, "127.0.0.1:11434");
    }

    /// Free at the margin, not metered, and no consent gate — the three things
    /// that make `Local` a different capacity class rather than a label.
    #[test]
    fn local_capacity_is_free_and_ungated() {
        let backend = local(Vec::new());
        assert_eq!(backend.kind(), BackendKind::Local);
        assert!(!backend.kind().is_metered());
        assert!(!backend.kind().requires_consent());
        assert!(!backend.requires_client_identity());
    }

    /// A local server that is not running is a backend `status` can explain,
    /// not a startup failure.
    #[tokio::test]
    async fn an_unreachable_local_server_still_reports_itself() {
        let status = local(Vec::new()).status().await;
        assert!(
            status.authenticated,
            "a local backend needs no credential, so it is never 'not logged in'"
        );
        assert_eq!(status.kind, BackendKind::Local);
    }
}

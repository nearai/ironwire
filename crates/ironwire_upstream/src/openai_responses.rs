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
use ironwire_core::protocol::{BackendId, BackendKind, ModelTier, Protocol, Wires};
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
        wires: Wires::only(Protocol::OpenAiResponses),
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

pub use crate::Catalogue;

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
    /// The provider's own models document, exactly as it answered.
    ///
    /// Kept beside the parsed catalogue rather than derived from it, because
    /// what Codex needs from this endpoint is everything we do *not* parse:
    /// per-model context windows, truncation policy, reasoning levels, the
    /// instructions template. See [`Backend::models_document`].
    document: Arc<Mutex<Option<Vec<u8>>>>,
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
            document: Arc::new(Mutex::new(None)),
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
            document: Arc::new(Mutex::new(None)),
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

    /// The `/models` URL for this backend.
    ///
    /// The client version matters for the subscription and nowhere else: the
    /// backend gates newer models behind it, so asking with a stale one
    /// silently returns a shorter list and IronWire would offer fewer models
    /// than Codex does for the same account (`crate::codex_version`).
    async fn models_url(&self) -> String {
        if matches!(self.auth, ResponsesAuth::Subscription { .. }) {
            let version = crate::codex_version::client_version().await;
            format!("{}/models?client_version={version}", self.base_url)
        } else {
            format!("{}/models", self.base_url)
        }
    }

    /// Keep both what the provider said and what we understood of it.
    ///
    /// The parsed catalogue is what routing needs; the raw bytes are what a
    /// client asking `/models` needs, and no summary of ours can stand in for
    /// them (see [`Backend::models_document`]).
    fn remember_catalogue(&self, body: &[u8]) {
        if let Some(models) = parse_model_list(body)
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
            match self.document.lock() {
                Ok(mut guard) => *guard = Some(body.to_vec()),
                Err(poisoned) => *poisoned.into_inner() = Some(body.to_vec()),
            }
        }
    }

    /// The last document this backend was given, if any.
    fn cached_document(&self) -> Option<Vec<u8>> {
        match self.document.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
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

    fn catalogue_from_provider(&self) -> bool {
        match self.discovered.lock() {
            Ok(guard) => guard.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
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
            .post(crate::endpoint_url(&self.base_url, &request.path))
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

    async fn models_document(&self) -> Option<Vec<u8>> {
        if let Some(cached) = self.cached_document() {
            return Some(cached);
        }
        // Not fetched yet — a client can ask for this before any probe has run,
        // and answering "here is my own idea of your models" would be worse
        // than the round trip.
        let (bearer, _) = self.credential().ok()?;
        self.check_host(&bearer).ok()?;
        let response = self
            .client
            .get(self.models_url().await)
            .bearer_auth(bearer.token.expose_secret())
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await
            .ok()?;
        if !response.status().is_success() {
            return None;
        }
        let body = response.bytes().await.ok()?;
        self.remember_catalogue(&body);
        self.cached_document()
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
        let url = self.models_url().await;

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
            if let Ok(body) = response.bytes().await {
                self.remember_catalogue(&body);
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
        .ok()
        // See `observe::anthropic_rate_limit`: a NaN survives `clamp` and then
        // compares false against every threshold, so the backend would look
        // healthy forever.
        .filter(|pct: &f32| pct.is_finite())?;
    // Clamped for the same reason as `retry-after`: this feeds a `Duration`,
    // and `chrono` panics rather than saturating when one is out of range.
    let resets_at = get(&format!("x-codex-{window}-reset-after-seconds"))
        .and_then(|v| v.parse::<i64>().ok())
        .map(|secs| {
            secs.clamp(
                0,
                i64::try_from(crate::observe::MAX_RETRY_AFTER_SECS).unwrap_or(86_400),
            )
        })
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

    let mut priced: Vec<(String, ModelTier, Option<f64>)> = items
        .iter()
        .filter_map(|item| {
            let id = item
                .get("id")
                .or_else(|| item.get("slug"))
                .or_else(|| item.get("model"))
                .and_then(serde_json::Value::as_str)
                .or_else(|| item.as_str())?;
            if !serves_text_chat(item) {
                return None;
            }
            let price = output_price(item);
            let tier = price.map_or_else(|| ModelTier::from_model_hint(id), tier_from_price);
            Some((id.to_string(), tier, price))
        })
        .collect();

    // Best first, by what the provider charges for it. Only when the provider
    // priced the catalogue: elsewhere the order it gave us *is* its ranking
    // (Codex sends an explicit `priority`), and re-sorting on a name would be
    // us overruling the provider with a guess.
    //
    // This matters because `pick_model` takes the first entry at a tier. Across
    // a fifty-model catalogue, "first" without an ordering means alphabetical —
    // which is how a fallback ended up on an old DeepSeek while the same
    // account could reach Claude and GPT-5.
    if priced.iter().any(|(_, _, price)| price.is_some()) {
        priced.sort_by(|a, b| {
            b.2.unwrap_or(0.0)
                .partial_cmp(&a.2.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    Some(priced.into_iter().map(|(id, tier, _)| (id, tier)).collect())
}

/// Dollars per million output tokens, where the catalogue states them.
fn output_price(item: &serde_json::Value) -> Option<f64> {
    let pricing = item.get("pricing")?;
    let read = |key: &str| -> Option<f64> {
        let value = pricing.get(key)?;
        value
            .as_f64()
            .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
    };
    // `output` is per million; `completion` is the same number per token, and
    // some catalogues carry only one of the two.
    read("output")
        .or_else(|| read("completion").map(|per_token| per_token * 1_000_000.0))
        .filter(|price| price.is_finite() && *price > 0.0)
}

/// Which tier a price puts a model in.
///
/// Price is the provider's own statement of what a model is worth, which is a
/// better tier signal than its name: a name test has to be updated for every
/// vendor's naming scheme and silently misfiles everything it has not seen,
/// while every catalogue that prices its models prices the capable ones higher.
///
/// The thresholds are in dollars per million output tokens, and they are chosen
/// where the market actually separates: frontier models sit at $10 and above
/// (Opus, GPT-5, Sonnet), the working middle from ~$2, and the small fast
/// models below that.
fn tier_from_price(output_usd_per_million: f64) -> ModelTier {
    if output_usd_per_million >= 10.0 {
        ModelTier::Frontier
    } else if output_usd_per_million >= 2.0 {
        ModelTier::Balanced
    } else {
        ModelTier::Fast
    }
}

/// Whether an entry describes something that can answer a chat turn.
///
/// A general endpoint's catalogue is not only chat models: NEAR AI's lists
/// image generators, a speech model and an embedding model alongside them.
/// Offering those as routing targets means a coding session can be handed to a
/// model that cannot read its prompt or cannot answer in words.
///
/// The test is the entry's *declared* modalities, not its name — a name pattern
/// is a guess about someone else's naming scheme, and this is stated in the
/// data. An entry that declares nothing is kept: no modality field is an
/// absence of evidence, and every catalogue that predates this (including
/// OpenAI's and Codex's) has none.
fn serves_text_chat(item: &serde_json::Value) -> bool {
    let declares = |field: &str, wanted: &str| -> Option<bool> {
        let list = item
            .get(field)
            .or_else(|| item.get("architecture").and_then(|a| a.get(field)))?
            .as_array()?;
        Some(list.iter().any(|m| m.as_str() == Some(wanted)))
    };
    declares("input_modalities", "text").unwrap_or(true)
        && declares("output_modalities", "text").unwrap_or(true)
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

    /// A slice of what NEAR AI actually returns, prices and all. Alphabetical,
    /// as the endpoint sends it — which is exactly the order that used to
    /// decide which model a fallback got.
    const PRICED_CATALOGUE: &str = r#"{"object":"list","data":[
        {"id":"anthropic/claude-opus-4-8","pricing":{"input":5.0,"output":25.0},
         "input_modalities":["text","image"],"output_modalities":["text"]},
        {"id":"deepseek-ai/DeepSeek-V4-Flash","pricing":{"input":0.17,"output":0.35},
         "input_modalities":["text"],"output_modalities":["text"]},
        {"id":"deepseek/deepseek-v3.2","pricing":{"input":0.27,"output":0.4},
         "input_modalities":["text"],"output_modalities":["text"]},
        {"id":"z-ai/glm-5.2","pricing":{"input":0.6,"output":4.4},
         "input_modalities":["text"],"output_modalities":["text"]},
        {"id":"Qwen/Qwen3-Embedding-0.6B","pricing":{"input":0.01,"output":0.01},
         "input_modalities":["text"],"output_modalities":["embedding"]}
    ]}"#;

    #[test]
    fn a_priced_catalogue_is_ordered_by_what_the_provider_charges() {
        let models = parse_model_list(PRICED_CATALOGUE.as_bytes()).expect("parses");
        let ids: Vec<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "anthropic/claude-opus-4-8",
                "z-ai/glm-5.2",
                "deepseek/deepseek-v3.2",
                "deepseek-ai/DeepSeek-V4-Flash",
            ],
            "the embedding model must be gone, and the rest ranked by price"
        );
    }

    /// The bug this ordering exists for: `pick_model` takes the first entry at
    /// a tier, so without a ranking a frontier request got whatever happened to
    /// sort first alphabetically.
    #[test]
    fn the_best_model_is_the_one_a_frontier_request_reaches_first() {
        let models = parse_model_list(PRICED_CATALOGUE.as_bytes()).expect("parses");
        let first_frontier = models
            .iter()
            .find(|(_, tier)| *tier == ModelTier::Frontier)
            .expect("a frontier model");
        assert_eq!(first_frontier.0, "anthropic/claude-opus-4-8");
    }

    #[test]
    fn price_places_a_model_in_its_tier() {
        let models = parse_model_list(PRICED_CATALOGUE.as_bytes()).expect("parses");
        let tier = |id: &str| {
            models
                .iter()
                .find(|(name, _)| name == id)
                .map(|(_, tier)| *tier)
        };
        assert_eq!(tier("anthropic/claude-opus-4-8"), Some(ModelTier::Frontier));
        assert_eq!(tier("z-ai/glm-5.2"), Some(ModelTier::Balanced));
        assert_eq!(tier("deepseek-ai/DeepSeek-V4-Flash"), Some(ModelTier::Fast));
    }

    /// Codex's catalogue carries no prices but does carry its own `priority`
    /// order. Re-sorting it on a name would be us overruling the provider.
    #[test]
    fn an_unpriced_catalogue_keeps_the_order_the_provider_sent() {
        let body =
            br#"{"models":[{"slug":"gpt-5.6-sol"},{"slug":"gpt-5.4"},{"slug":"gpt-5.4-mini"}]}"#;
        let models = parse_model_list(body).expect("parses");
        let ids: Vec<&str> = models.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["gpt-5.6-sol", "gpt-5.4", "gpt-5.4-mini"]);
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
            sub.capabilities().wires,
            key.capabilities().wires,
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

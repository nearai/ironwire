//! Observation: reading usage and quota off the wire.
//!
//! IronWire mostly does not construct request bodies, so it cannot compute
//! usage — and it should not want to. Both providers report usage and
//! rate-limit state; `docs/CRITIQUE.md` §4 is why we take what they give us and
//! say `unknown` for the rest rather than showing a number we made up.

use chrono::{DateTime, Utc};
use ironwire_core::quota::Headroom;
use serde::{Deserialize, Serialize};

/// Token usage for one exchange, as the provider reported it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageReading {
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Tokens written to the prompt cache.
    pub cache_creation_tokens: u64,
    /// Tokens served from the prompt cache.
    pub cache_read_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
}

impl UsageReading {
    /// Whether the provider told us anything at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Merge a later reading over an earlier one.
    ///
    /// Anthropic reports input counts in `message_start` and a *cumulative*
    /// output count in each `message_delta`, so output takes the maximum rather
    /// than a sum — adding them would inflate the number by the frame count.
    pub fn merge(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.max(other.input_tokens);
        self.cache_creation_tokens = self.cache_creation_tokens.max(other.cache_creation_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(other.cache_read_tokens);
        self.output_tokens = self.output_tokens.max(other.output_tokens);
    }

    /// Total tokens billed against a context window.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens + self.cache_creation_tokens + self.cache_read_tokens + self.output_tokens
    }
}

/// A rate-limit window as the provider reported it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitReading {
    /// Percentage of the window consumed.
    pub used_pct: f32,
    /// When it resets, if stated.
    pub resets_at: Option<DateTime<Utc>>,
}

impl RateLimitReading {
    /// Convert into the quota vocabulary, stamped with the observation time.
    #[must_use]
    pub fn into_headroom(self, observed_at: DateTime<Utc>) -> Headroom {
        Headroom::Observed {
            used_pct: self.used_pct,
            resets_at: self.resets_at,
            observed_at,
        }
    }
}

/// Everything one exchange told us.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Token usage, when reported.
    pub usage: Option<UsageReading>,
    /// Primary rate-limit window, when reported.
    pub primary: Option<RateLimitReading>,
    /// Secondary window, when reported.
    pub secondary: Option<RateLimitReading>,
    /// Provider-supplied retry delay from a 429.
    pub retry_after_secs: Option<u64>,
    /// Model the provider says actually served the request. Worth recording:
    /// providers sometimes silently substitute, and the user deserves to see
    /// what they actually got.
    pub served_model: Option<String>,
}

impl Observation {
    /// Whether anything was learned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.usage.is_none()
            && self.primary.is_none()
            && self.secondary.is_none()
            && self.retry_after_secs.is_none()
            && self.served_model.is_none()
    }
}

/// Read Anthropic's rate-limit headers.
///
/// Anthropic reports *remaining* against a limit; we convert to used-percent so
/// every provider lands in the same vocabulary.
#[must_use]
pub fn anthropic_rate_limit(headers: &[(String, String)]) -> Option<RateLimitReading> {
    let get = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let resets_at = get("anthropic-ratelimit-unified-reset")
        .and_then(|v| v.parse::<i64>().ok())
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
        .or_else(|| {
            get("anthropic-ratelimit-unified-reset")
                .and_then(|v| DateTime::parse_from_rfc3339(v).ok())
                .map(|d| d.with_timezone(&Utc))
        });

    // Prefer an explicit percentage where the provider gives one; otherwise
    // derive it from remaining/limit. If neither is present we return None —
    // that becomes `Headroom::Unknown`, not a guess.
    // `parse::<f32>` accepts "NaN" and "inf". A NaN survives `clamp` — it
    // propagates — and then `used_pct >= 90.0` is false forever, so a backend
    // reporting NaN would look permanently healthy while `status` printed
    // "NaN% used". Rejecting it yields `Unknown`, which is the honest answer.
    if let Some(pct) = get("anthropic-ratelimit-unified-used-percent")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|pct| pct.is_finite())
    {
        return Some(RateLimitReading {
            used_pct: pct.clamp(0.0, 100.0),
            resets_at,
        });
    }

    let remaining = get("anthropic-ratelimit-unified-remaining")?
        .parse::<f64>()
        .ok()?;
    let limit = get("anthropic-ratelimit-unified-limit")?
        .parse::<f64>()
        .ok()?;
    if limit <= 0.0 {
        return None;
    }
    if !limit.is_finite() || !remaining.is_finite() {
        return None;
    }
    let used_pct = (((limit - remaining) / limit) * 100.0).clamp(0.0, 100.0);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a percentage in 0..=100 is exactly representable in f32"
    )]
    Some(RateLimitReading {
        used_pct: used_pct as f32,
        resets_at,
    })
}

/// Longest `retry-after` IronWire will act on.
///
/// A provider asking us to wait longer than a day is telling us nothing useful:
/// the user will have restarted, the conversation is gone, and the value is
/// almost certainly a bug at the other end rather than an instruction. Clamping
/// keeps a plausible-looking header from becoming an arithmetic overflow.
pub const MAX_RETRY_AFTER_SECS: u64 = 24 * 60 * 60;

/// Read a `retry-after` header. Seconds form and HTTP-date form.
///
/// **Clamped**, because the value is upstream-controlled and feeds
/// `now + Duration::seconds(..)`. `chrono` panics rather than saturating on an
/// out-of-range duration, so an unclamped header was a remotely-triggerable
/// crash — and a crash here takes down every conversation on the machine, not
/// just the request that received the header.
#[must_use]
pub fn retry_after(headers: &[(String, String)], now: DateTime<Utc>) -> Option<u64> {
    let value = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("retry-after"))
        .map(|(_, v)| v.as_str())?;
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(secs.min(MAX_RETRY_AFTER_SECS));
    }
    let when = DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|d| d.with_timezone(&Utc))?;
    u64::try_from((when - now).num_seconds())
        .ok()
        .map(|secs| secs.min(MAX_RETRY_AFTER_SECS))
}

/// Read a usage object in Anthropic's shape.
#[must_use]
pub fn anthropic_usage(value: &serde_json::Value) -> Option<UsageReading> {
    let usage = value.as_object()?;
    let n = |key: &str| {
        usage
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let reading = UsageReading {
        input_tokens: n("input_tokens"),
        cache_creation_tokens: n("cache_creation_input_tokens"),
        cache_read_tokens: n("cache_read_input_tokens"),
        output_tokens: n("output_tokens"),
    };
    (!reading.is_empty()).then_some(reading)
}

/// Read a usage object in OpenAI's shape.
#[must_use]
pub fn openai_usage(value: &serde_json::Value) -> Option<UsageReading> {
    let usage = value.as_object()?;
    let n = |key: &str| {
        usage
            .get(key)
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let cached = value
        .pointer("/input_tokens_details/cached_tokens")
        .or_else(|| value.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let input = n("input_tokens").max(n("prompt_tokens"));
    let reading = UsageReading {
        input_tokens: input.saturating_sub(cached),
        cache_creation_tokens: 0,
        cache_read_tokens: cached,
        output_tokens: n("output_tokens").max(n("completion_tokens")),
    };
    (!reading.is_empty()).then_some(reading)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_700_000_000, 0).expect("valid timestamp")
    }

    #[test]
    fn cumulative_output_counts_are_maxed_not_summed() {
        // Anthropic re-reports a running total in every message_delta. Summing
        // would multiply the number by the frame count.
        let mut usage = UsageReading {
            input_tokens: 100,
            output_tokens: 10,
            ..Default::default()
        };
        usage.merge(UsageReading {
            output_tokens: 25,
            ..Default::default()
        });
        usage.merge(UsageReading {
            output_tokens: 40,
            ..Default::default()
        });
        assert_eq!(usage.output_tokens, 40);
        assert_eq!(usage.input_tokens, 100, "input must survive later merges");
    }

    #[test]
    fn rate_limit_is_derived_from_remaining_and_limit() {
        let reading = anthropic_rate_limit(&headers(&[
            ("anthropic-ratelimit-unified-limit", "1000"),
            ("anthropic-ratelimit-unified-remaining", "250"),
        ]))
        .expect("derivable");
        assert!((reading.used_pct - 75.0).abs() < 0.01);
    }

    #[test]
    fn an_explicit_percentage_wins_over_derivation() {
        let reading = anthropic_rate_limit(&headers(&[
            ("anthropic-ratelimit-unified-used-percent", "82"),
            ("anthropic-ratelimit-unified-limit", "1000"),
            ("anthropic-ratelimit-unified-remaining", "250"),
        ]))
        .expect("explicit");
        assert!((reading.used_pct - 82.0).abs() < 0.01);
    }

    #[test]
    fn no_rate_limit_headers_means_unknown_not_zero() {
        // Reporting 0% used when the provider said nothing would be a lie that
        // routing then acts on.
        assert!(anthropic_rate_limit(&headers(&[("content-type", "application/json")])).is_none());
        // A limit with no remaining is not enough to derive anything.
        assert!(
            anthropic_rate_limit(&headers(&[("anthropic-ratelimit-unified-limit", "1000")]))
                .is_none()
        );
    }

    #[test]
    fn a_zero_limit_does_not_divide_by_zero() {
        assert!(
            anthropic_rate_limit(&headers(&[
                ("anthropic-ratelimit-unified-limit", "0"),
                ("anthropic-ratelimit-unified-remaining", "0"),
            ]))
            .is_none()
        );
    }

    #[test]
    fn retry_after_parses_seconds_and_dates() {
        assert_eq!(
            retry_after(&headers(&[("retry-after", "30")]), now()),
            Some(30)
        );
        assert_eq!(
            retry_after(&headers(&[("Retry-After", " 45 ")]), now()),
            Some(45)
        );
        assert_eq!(retry_after(&headers(&[]), now()), None);
    }

    #[test]
    fn a_retry_after_date_in_the_past_is_not_a_huge_wait() {
        // Negative durations must not wrap into an enormous u64.
        let past = "Tue, 14 Nov 2023 22:13:00 GMT";
        assert_eq!(retry_after(&headers(&[("retry-after", past)]), now()), None);
    }

    #[test]
    fn anthropic_usage_separates_cache_reads_from_fresh_input() {
        let usage = anthropic_usage(&json!({
            "input_tokens": 12,
            "cache_creation_input_tokens": 2048,
            "cache_read_input_tokens": 98_000,
            "output_tokens": 137
        }))
        .expect("parses");
        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.cache_read_tokens, 98_000);
        assert_eq!(usage.total(), 12 + 2048 + 98_000 + 137);
    }

    #[test]
    fn openai_cached_tokens_do_not_double_count_as_input() {
        let usage = openai_usage(&json!({
            "input_tokens": 1000,
            "input_tokens_details": {"cached_tokens": 800},
            "output_tokens": 50
        }))
        .expect("parses");
        assert_eq!(usage.input_tokens, 200);
        assert_eq!(usage.cache_read_tokens, 800);
        assert_eq!(usage.output_tokens, 50);
    }

    #[test]
    fn an_empty_usage_object_is_no_observation() {
        assert!(anthropic_usage(&json!({})).is_none());
        assert!(openai_usage(&json!({})).is_none());
        assert!(Observation::default().is_empty());
    }
}

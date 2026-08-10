//! Reducing captured per-token log-probabilities to a few numbers.
//!
//! # Why reduce at all
//!
//! Raw per-token distributions cannot travel. Trace Commons caps an ingest
//! body at 2 MiB and top-5 logprobs for a typical trace is several times that,
//! so anything that wants to carry this signal has to reduce it first. IronWire
//! is where the raw data already is, which makes it the only place the
//! reduction can happen without the distributions leaving the machine.
//!
//! The privacy argument runs the same way. Distributions are conditioned on
//! the entire context, which makes them more sensitive than the text they
//! describe; four aggregate numbers give an attacker essentially nothing to
//! invert.
//!
//! # These are not dataset cartography
//!
//! The vocabulary comes from Swayamdipta et al., *Dataset Cartography* (2020),
//! which defines confidence, variability and correctness **across training
//! epochs against a gold label**. Single-pass generation has neither epochs nor
//! a gold label.
//!
//! | Field | Cartography | Here |
//! |---|---|---|
//! | `mean_confidence` | mean p(gold) across epochs | mean p(chosen) across the trace |
//! | `variability` | s.d. of p(gold) across **epochs** | s.d. of p(chosen) across **tokens** |
//! | `correctness` | fraction of epochs predicted right | not derivable here at all |
//!
//! `variability` differs in kind rather than degree, and the two must not be
//! compared. The names match the Trace Commons envelope field they feed, which
//! is the only reason to keep them.

use serde::{Deserialize, Serialize};

/// At or above this mean confidence, a steady trace is [`ConfidenceBucket::Easy`].
///
/// Provisional rather than calibrated: no corpus has been measured against it.
/// **Mirrors the constant of the same name in `trace_commons_protocol`** — the
/// two must move together, or producer and server disagree about what a bucket
/// means.
pub const EASY_MEAN_CONFIDENCE: f32 = 0.75;

/// At or above this dispersion a trace is [`ConfidenceBucket::Ambiguous`]
/// whatever its mean. Mirrors the server constant; see above.
pub const AMBIGUOUS_VARIABILITY: f32 = 0.25;

/// Coarse shape of a trace's confidence profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceBucket {
    /// Confident throughout: the model was rarely in doubt.
    Easy,
    /// Swung between certainty and doubt. Checked before the mean, because
    /// averaging is exactly what hides this case.
    Ambiguous,
    /// Steadily unconfident: uniformly unsure rather than torn.
    Hard,
}

/// Aggregate confidence signals for one trace.
///
/// `correctness` is deliberately absent: no arrangement of log-probabilities
/// answers whether the work was right. It needs an outcome signal, and
/// whatever assembles a contribution supplies it separately rather than
/// letting confidence stand in for it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceAggregates {
    /// Mean probability of the tokens the model emitted, in `0.0..=1.0`.
    pub mean_confidence: f32,
    /// Population standard deviation of those probabilities across tokens.
    pub variability: f32,
    /// Bucket derived from the two figures above.
    pub bucket: ConfidenceBucket,
    /// How many tokens the aggregate is over. A mean over eleven tokens and a
    /// mean over eleven thousand are not the same claim, and a consumer that
    /// cannot tell them apart will treat them as though they were.
    pub token_count: usize,
}

/// Reduce per-token probabilities to aggregates.
///
/// `probabilities` are p(chosen token) — `exp(logprob)`, not the logprob.
/// Returns `None` for an empty slice: nothing observed means nothing claimed,
/// and a confident-looking zero would be worse than an absence.
///
/// Values that are not probabilities are ignored rather than clamped. A caller
/// that hands us `1.7` has a bug upstream, and folding a repaired value into
/// the mean would hide it.
#[must_use]
pub fn reduce_token_confidences(probabilities: &[f32]) -> Option<ConfidenceAggregates> {
    let usable: Vec<f32> = probabilities
        .iter()
        .copied()
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
        .collect();
    if usable.is_empty() {
        return None;
    }

    let count = usable.len() as f32;
    let mean = (usable.iter().sum::<f32>() / count).clamp(0.0, 1.0);
    let variance = usable.iter().map(|p| (p - mean).powi(2)).sum::<f32>() / count;
    let variability = variance.sqrt().clamp(0.0, 1.0);

    let bucket = if variability >= AMBIGUOUS_VARIABILITY {
        // Dispersion first: a run that swings between certainty and doubt is
        // the interesting case, and its mean hides exactly that.
        ConfidenceBucket::Ambiguous
    } else if mean >= EASY_MEAN_CONFIDENCE {
        ConfidenceBucket::Easy
    } else {
        ConfidenceBucket::Hard
    };

    Some(ConfidenceAggregates {
        mean_confidence: mean,
        variability,
        bucket,
        token_count: usable.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(actual: f32, expected: f32) {
        assert!(
            (actual - expected).abs() < 1e-4,
            "expected ~{expected}, got {actual}"
        );
    }

    /// Nothing observed means nothing claimed. Capture is off by default, so
    /// this is the overwhelmingly common case and it must not look like a
    /// measured zero.
    #[test]
    fn nothing_captured_yields_nothing() {
        assert!(reduce_token_confidences(&[]).is_none());
    }

    #[test]
    fn all_values_unusable_yields_nothing() {
        assert!(reduce_token_confidences(&[f32::NAN, 2.0, -1.0]).is_none());
    }

    #[test]
    fn mean_and_variability_are_the_population_statistics() {
        let a = reduce_token_confidences(&[0.2, 0.4, 0.6, 0.8]).expect("aggregates");
        approx(a.mean_confidence, 0.5);
        approx(a.variability, 0.223_607);
        assert_eq!(a.token_count, 4);
    }

    #[test]
    fn a_single_token_has_no_variability() {
        let a = reduce_token_confidences(&[0.42]).expect("aggregates");
        approx(a.mean_confidence, 0.42);
        approx(a.variability, 0.0);
        assert_eq!(a.token_count, 1);
    }

    /// Ignored, not clamped — otherwise a caller's bug is folded into the mean
    /// and never seen again.
    #[test]
    fn unusable_values_are_ignored_not_repaired() {
        let a = reduce_token_confidences(&[0.5, f32::INFINITY, 1.5, 0.5]).expect("aggregates");
        approx(a.mean_confidence, 0.5);
        assert_eq!(a.token_count, 2, "only the two real probabilities count");
    }

    #[test]
    fn steady_and_confident_is_easy() {
        let a = reduce_token_confidences(&[0.95, 0.93, 0.97]).expect("aggregates");
        assert_eq!(a.bucket, ConfidenceBucket::Easy);
    }

    #[test]
    fn steady_and_unconfident_is_hard() {
        let a = reduce_token_confidences(&[0.10, 0.12, 0.08]).expect("aggregates");
        assert_eq!(a.bucket, ConfidenceBucket::Hard);
    }

    /// Dispersion wins regardless of the mean.
    #[test]
    fn high_variability_is_ambiguous_whatever_the_mean() {
        for probs in [
            vec![0.99, 0.01, 0.99, 0.01],
            vec![1.0, 0.4, 1.0, 0.4],
            vec![0.2, 0.9, 0.1, 0.8],
        ] {
            let a = reduce_token_confidences(&probs).expect("aggregates");
            assert_eq!(a.bucket, ConfidenceBucket::Ambiguous, "for {probs:?}");
        }
    }

    /// Everything emitted here has to satisfy the bounds Trace Commons
    /// enforces on the envelope, or a contribution built from it is rejected
    /// at ingest.
    #[test]
    fn output_always_fits_the_envelope_bounds() {
        let corpora: Vec<Vec<f32>> = vec![
            vec![0.0, 0.0],
            vec![1.0, 1.0],
            vec![0.0, 1.0],
            vec![0.5],
            (0..500).map(|i| (i % 101) as f32 / 100.0).collect(),
        ];
        for probs in corpora {
            let a = reduce_token_confidences(&probs).expect("aggregates");
            for value in [a.mean_confidence, a.variability] {
                assert!(
                    value.is_finite() && (0.0..=1.0).contains(&value),
                    "out of bounds: {value} from {probs:?}"
                );
            }
        }
    }

    /// The token count is what lets a consumer tell a mean over eleven tokens
    /// from a mean over eleven thousand.
    #[test]
    fn token_count_reports_what_the_mean_is_over() {
        let a = reduce_token_confidences(&[0.5; 1000]).expect("aggregates");
        assert_eq!(a.token_count, 1000);
    }
}

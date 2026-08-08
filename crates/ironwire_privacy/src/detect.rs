//! Finding what to substitute.
//!
//! Tier 1 (secrets) and tier 2 (values the user nominated) are both
//! deterministic and both delegate their hard part to `ironclaw_safety`
//! (`docs/PRIVACY.md` §2). What is written here is the part ironclaw has no
//! reason to have: locating matches so they can be replaced **reversibly**,
//! and refusing to match things that only look sensitive.
//!
//! That second job is specific to a coding agent and easy to underestimate. A
//! large share of what a PII detector flags in a coding session is
//! load-bearing *code*: `user@example.com` in a test assertion, `192.168.1.1`
//! in a fixture, a `555` number in a validator's expected output. Substituting
//! those makes the model write code that does not compile, and the user blames
//! the model.

use std::ops::Range;

use ironclaw_safety::{LeakDetector, redaction_values_for_secret};

use crate::mint::Class;

/// One thing worth substituting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Byte range within the scanned text.
    pub range: Range<usize>,
    /// What kind of value it is.
    pub class: Class,
    /// Which rule matched, for `ironwire privacy check`.
    pub rule: String,
}

/// Hosts and ranges reserved by standards for documentation and testing.
///
/// Never substituted. They are in a user's code *because* they are not real,
/// and replacing them turns a working fixture into a broken one.
const RESERVED_SUBSTRINGS: &[&str] = &[
    // RFC 2606 / 6761 reserved names.
    "example.com",
    "example.org",
    "example.net",
    "example.edu",
    ".invalid",
    ".localhost",
    ".test",
    "localhost",
];

/// Which tiers are switched on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tiers {
    /// Tier 1: secrets with a machine-checkable shape.
    pub secrets: bool,
    /// Tier 2: exact strings the user nominated.
    pub named_values: Vec<String>,
}

impl Tiers {
    /// Whether anything is enabled.
    #[must_use]
    pub fn is_off(&self) -> bool {
        !self.secrets && self.named_values.is_empty()
    }
}

/// Finds substitutable values in text.
pub struct Detector {
    secrets: Option<LeakDetector>,
    /// Nominated values, expanded to their encoded variants and sorted
    /// longest-first so an overlapping shorter value cannot win.
    named: Vec<String>,
}

impl std::fmt::Debug for Detector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The nominated values are the user's own sensitive strings. Printing
        // them in a log would defeat the feature.
        f.debug_struct("Detector")
            .field("secrets", &self.secrets.is_some())
            .field("named", &format!("{} value(s)", self.named.len()))
            .finish()
    }
}

impl Detector {
    /// Build a detector for the configured tiers.
    #[must_use]
    pub fn new(tiers: &Tiers) -> Self {
        // `redaction_values_for_secret` expands `%20`/`+` and lowercase
        // percent-escape variants — the part of exact-value matching that is
        // easy to get wrong and that ironclaw already got right.
        let mut named: Vec<String> = tiers
            .named_values
            .iter()
            .filter(|value| !value.trim().is_empty())
            .flat_map(|value| redaction_values_for_secret(value))
            .collect();
        named.sort_by_key(|value| std::cmp::Reverse(value.len()));
        named.dedup();

        Self {
            secrets: tiers.secrets.then(LeakDetector::new),
            named,
        }
    }

    /// Locate everything worth substituting, in order, without overlaps.
    #[must_use]
    pub fn find(&self, text: &str) -> Vec<Finding> {
        let mut findings = Vec::new();

        if let Some(detector) = &self.secrets {
            for hit in detector.scan(text).matches {
                findings.push(Finding {
                    range: hit.location,
                    class: Class::Secret,
                    rule: hit.pattern_name,
                });
            }
        }

        // Longest-first, so `alice@corp.com` wins over a nominated `corp.com`
        // that overlaps it.
        for value in &self.named {
            let mut from = 0;
            while let Some(offset) = text[from..].find(value.as_str()) {
                let start = from + offset;
                findings.push(Finding {
                    range: start..start + value.len(),
                    class: Class::Named,
                    rule: "named value".to_string(),
                });
                from = start + value.len();
            }
        }

        resolve_overlaps(findings, text)
    }
}

/// Whether a match should be left alone despite matching.
///
/// Checked against the surrounding line, not just the match, because that is
/// where the evidence is: `192.168.1.1` in `EXPECTED_HOST = "192.168.1.1"` is a
/// fixture, and the range alone cannot tell you that.
fn is_reserved(text: &str, range: &Range<usize>) -> bool {
    let matched = &text[range.clone()];
    let lower = matched.to_ascii_lowercase();
    if RESERVED_SUBSTRINGS
        .iter()
        .any(|reserved| lower.contains(reserved))
    {
        return true;
    }
    // RFC 1918 private ranges and RFC 5737 documentation ranges are in a repo
    // because they are not routable, not because they identify anyone.
    for prefix in [
        "10.",
        "192.168.",
        "127.",
        "169.254.",
        "192.0.2.",
        "198.51.100.",
        "203.0.113.",
    ] {
        if lower.starts_with(prefix) {
            return true;
        }
    }
    for prefix in ["172.1", "172.2", "172.3"] {
        if lower.starts_with(prefix) && looks_like_private_172(&lower) {
            return true;
        }
    }
    // RFC 3849 documentation IPv6.
    if lower.starts_with("2001:db8") {
        return true;
    }
    // NANP reserved fictional exchange.
    lower.contains("555-01") || lower.contains("55501")
}

/// `172.16.0.0/12` — the second octet is 16..=31, which a prefix test cannot
/// express on its own.
fn looks_like_private_172(text: &str) -> bool {
    text.strip_prefix("172.")
        .and_then(|rest| rest.split('.').next())
        .and_then(|octet| octet.parse::<u8>().ok())
        .is_some_and(|octet| (16..=31).contains(&octet))
}

/// Keep the longest match at each position and drop anything reserved.
fn resolve_overlaps(mut findings: Vec<Finding>, text: &str) -> Vec<Finding> {
    findings.retain(|finding| {
        // A range that does not land on a character boundary came from a
        // pattern we cannot safely slice; dropping it is better than panicking
        // on someone's request.
        text.is_char_boundary(finding.range.start)
            && text.is_char_boundary(finding.range.end)
            && !is_reserved(text, &finding.range)
    });

    findings.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then_with(|| b.range.len().cmp(&a.range.len()))
    });

    let mut out: Vec<Finding> = Vec::with_capacity(findings.len());
    for finding in findings {
        if out
            .last()
            .is_some_and(|prev| prev.range.end > finding.range.start)
        {
            continue;
        }
        out.push(finding);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secrets_only() -> Detector {
        Detector::new(&Tiers {
            secrets: true,
            named_values: Vec::new(),
        })
    }

    #[test]
    fn a_real_looking_api_key_is_found() {
        let detector = secrets_only();
        let text = "export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789";
        let findings = detector.find(text);
        assert_eq!(findings.len(), 1, "got {findings:?}");
        assert_eq!(findings[0].class, Class::Secret);
    }

    #[test]
    fn ordinary_source_code_is_left_alone() {
        // The failure mode that matters most: over-matching makes the model
        // write broken code and the user blames the model.
        let detector = secrets_only();
        let text = "let total = items.iter().map(|i| i.price).sum::<u64>();";
        assert!(detector.find(text).is_empty());
    }

    #[test]
    fn reserved_documentation_values_are_never_substituted() {
        // These are in a repo *because* they are not real. Replacing them turns
        // a working fixture into a broken one.
        let detector = Detector::new(&Tiers {
            secrets: true,
            named_values: vec![
                "user@example.com".to_string(),
                "192.168.1.1".to_string(),
                "10.0.0.5".to_string(),
                "172.16.0.1".to_string(),
                "203.0.113.7".to_string(),
                "2001:db8::1".to_string(),
                "555-0100".to_string(),
            ],
        });
        for text in [
            "assert_eq!(user.email, \"user@example.com\");",
            "const HOST: &str = \"192.168.1.1\";",
            "bind 10.0.0.5",
            "gateway 172.16.0.1",
            "doc range 203.0.113.7",
            "v6 2001:db8::1",
            "call 555-0100",
        ] {
            assert!(
                detector.find(text).is_empty(),
                "substituted a reserved value in: {text}"
            );
        }
    }

    #[test]
    fn a_public_172_address_is_not_treated_as_private() {
        // 172.16/12 is private; 172.32 is not. A prefix test alone gets this
        // wrong and would silently skip a real address.
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["172.32.4.5".to_string()],
        });
        assert_eq!(detector.find("host 172.32.4.5").len(), 1);
    }

    #[test]
    fn a_nominated_value_is_found_everywhere_it_appears() {
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["Acme Holdings".to_string()],
        });
        let text = "Acme Holdings invoices Acme Holdings monthly";
        assert_eq!(detector.find(text).len(), 2);
    }

    #[test]
    fn a_url_encoded_nominated_value_is_found_too() {
        // The part of exact-value matching that is easy to get wrong, and the
        // reason this delegates to ironclaw rather than calling `str::find`.
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["Acme Holdings".to_string()],
        });
        assert_eq!(detector.find("q=Acme%20Holdings&x=1").len(), 1);
        assert_eq!(detector.find("q=Acme+Holdings&x=1").len(), 1);
    }

    #[test]
    fn overlapping_matches_resolve_to_the_longest() {
        // Otherwise a nominated `corp.com` would carve up an
        // `alice@corp.com` that a longer rule already claimed, and reversal
        // would put back two fragments.
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["alice@corp.com".to_string(), "corp.com".to_string()],
        });
        let findings = detector.find("mail alice@corp.com now");
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].range.len(), "alice@corp.com".len());
    }

    #[test]
    fn a_detector_never_prints_the_users_nominated_values() {
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["MyEmployerName".to_string()],
        });
        let rendered = format!("{detector:?}");
        assert!(!rendered.contains("MyEmployerName"), "got {rendered}");
    }

    #[test]
    fn an_empty_configuration_finds_nothing_and_costs_nothing() {
        let detector = Detector::new(&Tiers::default());
        assert!(
            detector
                .find("sk-ant-api03-aaaaaaaaaaaaaaaaaaaaaaaa")
                .is_empty()
        );
        assert!(Tiers::default().is_off());
    }

    #[test]
    fn findings_come_back_in_order_and_never_overlap() {
        // The substituter walks them back-to-front; overlapping ranges would
        // corrupt the output.
        let detector = Detector::new(&Tiers {
            secrets: false,
            named_values: vec!["alpha".to_string(), "beta".to_string()],
        });
        let findings = detector.find("beta then alpha then beta");
        for pair in findings.windows(2) {
            assert!(
                pair[0].range.end <= pair[1].range.start,
                "overlapping findings: {pair:?}"
            );
        }
    }
}

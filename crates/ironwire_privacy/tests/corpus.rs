//! Corpus: real-looking source must pass through untouched.
//!
//! `docs/PRIVACY.md` §6. The failure mode this guards against is not a privacy
//! failure at all — it is the filter substituting values that are *load-bearing
//! code*, so the model writes something that does not compile and the user
//! blames the model.
//!
//! Every fixture below is the kind of thing that actually appears in a repo a
//! coding agent is working on.

use ironwire_privacy::{Detector, Salt, Tiers};

fn deterministic_tiers() -> Tiers {
    Tiers {
        secrets: true,
        named_values: Vec::new(),
    }
}

/// Files a coding agent routinely reads, none of which contain a real secret.
const CLEAN_FIXTURES: &[(&str, &str)] = &[
    (
        "a test asserting on a documentation email",
        r#"
#[test]
fn parses_an_address() {
    let user = User::parse("user@example.com").unwrap();
    assert_eq!(user.domain(), "example.com");
    assert_eq!(User::parse("admin@example.org").unwrap().local(), "admin");
}
"#,
    ),
    (
        "a config full of private and documentation addresses",
        r#"
[network]
bind = "127.0.0.1:8080"
gateway = "192.168.1.1"
peers = ["10.0.0.5", "172.16.4.9", "169.254.1.1"]
doc_example = "203.0.113.42"
ipv6_doc = "2001:db8::dead:beef"
"#,
    ),
    (
        "a phone-number validator's fixtures",
        r#"
const VALID: &[&str] = &["555-0100", "555-0142", "(555) 018-7000"];
const INVALID: &[&str] = &["", "abc", "1"];
"#,
    ),
    (
        "ordinary application code",
        r#"
pub fn reconcile(items: &[Item], budget: u64) -> Vec<Plan> {
    let mut remaining = budget;
    items
        .iter()
        .filter(|item| item.enabled && item.price <= remaining)
        .map(|item| {
            remaining = remaining.saturating_sub(item.price);
            Plan::new(item.id, item.price)
        })
        .collect()
}
"#,
    ),
    (
        "a lockfile-ish blob of hashes",
        r#"
checksum = "d975f5c8e0a1b2c3d4e5f60718293a4b5c6d7e8f9012a3b4c5d6e7f8091a2b3c"
integrity = "sha512-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789+/=="
revision = "4685961ab8c1d2e3f405162738495a6b7c8d9e0f"
"#,
    ),
    (
        "a UUID-heavy fixture",
        r#"
let ids = [
    "36afe797-0000-4444-8888-aaaaaaaaaaaa",
    "01998f6e-0000-7000-8000-000000000000",
];
"#,
    ),
    (
        "prose with no sensitive content",
        "The reconciliation pass runs after every write and is idempotent. \
         It reads the ledger, compares it against the projection, and emits a \
         plan describing what must change.",
    ),
];

#[test]
fn ordinary_source_produces_no_substitutions() {
    let detector = Detector::new(&deterministic_tiers());
    let mut failures = Vec::new();

    for (name, fixture) in CLEAN_FIXTURES {
        let findings = detector.find(fixture);
        if !findings.is_empty() {
            let matched: Vec<&str> = findings.iter().map(|f| &fixture[f.range.clone()]).collect();
            failures.push(format!("{name}: {matched:?}"));
        }
    }

    assert!(
        failures.is_empty(),
        "the filter would have rewritten code that is not sensitive, which \
         makes the model produce something broken and the user blame the \
         model:\n  {}",
        failures.join("\n  ")
    );
}

/// The other half: things that genuinely are secrets must still be caught, or
/// the corpus test above would be trivially satisfiable by matching nothing.
const SECRET_FIXTURES: &[(&str, &str)] = &[
    ("github token", "ghp_abcdefghijklmnopqrstuvwxyz0123456789"),
    (
        "github fine-grained PAT",
        "github_pat_11ABCDEFG0123456789012_abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456",
    ),
    ("aws access key", "AKIAIOSFODNN7EXAMPLE"),
    ("stripe key", "sk_live_abcdefghijklmnopqrstuvwx"),
    ("google api key", "AIzaSyA1234567890abcdefghijklmnopqrstuv"),
    ("slack token", "xoxb-123456789012-abcdefghijklmnop"),
    (
        "private key header",
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEow...",
    ),
];

/// A bare hex blob is a hash; the same blob labelled as a token is a secret.
#[test]
fn a_hex_blob_counts_only_when_something_calls_it_a_credential() {
    let detector = Detector::new(&deterministic_tiers());
    let blob = "d975f5c8e0a1b2c3d4e5f60718293a4b5c6d7e8f9012a3b4c5d6e7f8091a2b3c";

    assert!(
        detector.find(&format!("checksum = \"{blob}\"")).is_empty(),
        "a lockfile checksum was treated as a secret; the model would rewrite \
         the lockfile around a placeholder and the build would break"
    );
    assert!(
        detector.find(&format!("revision = \"{blob}\"")).is_empty(),
        "a git revision was treated as a secret"
    );
    assert_eq!(
        detector.find(&format!("api_token = \"{blob}\"")).len(),
        1,
        "the same blob, labelled a token, must be caught"
    );
    assert_eq!(
        detector
            .find(&format!("Authorization: Bearer {blob}"))
            .len(),
        1,
        "a bearer credential must be caught"
    );
}

#[test]
fn genuine_secrets_are_still_caught() {
    let detector = Detector::new(&deterministic_tiers());
    let mut missed = Vec::new();

    for (name, fixture) in SECRET_FIXTURES {
        let text = format!("config value:\n  key = \"{fixture}\"\n");
        if detector.find(&text).is_empty() {
            missed.push(*name);
        }
    }

    assert!(
        missed.is_empty(),
        "the corpus test above would be trivially satisfiable if these were \
         missed too. Not caught: {missed:?}"
    );
}

#[test]
fn a_clean_file_is_returned_completely_unchanged() {
    // Not merely "no findings" — the substituter must produce an identical
    // document, so a request that matched nothing is byte-identical upstream.
    let detector = Detector::new(&deterministic_tiers());
    for (name, fixture) in CLEAN_FIXTURES {
        let body = serde_json::json!({
            "messages": [{"role": "user", "content": *fixture}]
        });
        let result = ironwire_privacy::substitute(
            &detector,
            &Salt::fixed(1),
            &ironwire_privacy::Exemptions::default(),
            &body,
        );
        assert_eq!(result.body, body, "{name} was altered");
        assert_eq!(result.substitutions, 0, "{name} produced substitutions");
    }
}

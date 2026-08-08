//! Verifying, storing and loading a quirks document.
//!
//! Three rules, in order of how badly they fail if broken:
//!
//! 1. **Verify before parse.** An unsigned document is never deserialised, let
//!    alone applied.
//! 2. **Never go backwards.** A document whose serial is at or below the one
//!    already installed is refused — that is what stops a rollback attack from
//!    re-exposing a provider workaround we already fixed.
//! 3. **Never fail closed onto nothing.** Any error leaves the previously
//!    installed document (or the compiled-in default) in place. A quirks
//!    problem must not stop the proxy from proxying.

use std::path::Path;

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

use crate::schema::{Quirks, SCHEMA_VERSION};

/// Why a document was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum QuirksError {
    /// The signing key baked into this binary is not a valid ed25519 key. A
    /// build problem, not a runtime one.
    #[error("the built-in quirks signing key is malformed")]
    MalformedKey,

    /// The signature is not valid hex, or not 64 bytes.
    #[error("the quirks signature is malformed")]
    MalformedSignature,

    /// The signature does not verify against the built-in key.
    #[error("the quirks document is not signed by a key this build trusts")]
    BadSignature,

    /// Signed, but not parseable as a quirks document.
    #[error("the quirks document is signed but not readable: {0}")]
    Malformed(String),

    /// Written for a schema this binary does not understand.
    #[error("quirks schema {found} is newer than this build understands ({SCHEMA_VERSION})")]
    SchemaTooNew {
        /// Schema the document declares.
        found: u32,
    },

    /// Older than what is installed — the rollback guard.
    #[error("refusing quirks serial {offered}; serial {installed} is already installed")]
    Rollback {
        /// Serial the document offers.
        offered: u64,
        /// Serial already in force.
        installed: u64,
    },
}

/// A signed quirks document as it travels and as it is stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignedQuirks {
    /// The document body, verbatim — the exact bytes that were signed. Kept as
    /// a string rather than a parsed value so re-verification after a restart
    /// checks the same bytes, not a re-serialisation of them.
    pub document: String,
    /// Detached ed25519 signature over `document`, hex-encoded.
    pub signature: String,
}

impl SignedQuirks {
    /// Verify against `verifying_key` and parse.
    ///
    /// # Errors
    ///
    /// [`QuirksError`] for a bad key, bad signature, unreadable body, or a
    /// schema this build does not understand.
    pub fn verify(&self, verifying_key: &[u8; 32]) -> Result<Quirks, QuirksError> {
        let key = VerifyingKey::from_bytes(verifying_key).map_err(|_| QuirksError::MalformedKey)?;
        let raw =
            hex::decode(self.signature.trim()).map_err(|_| QuirksError::MalformedSignature)?;
        let bytes: [u8; 64] = raw
            .try_into()
            .map_err(|_| QuirksError::MalformedSignature)?;
        key.verify(self.document.as_bytes(), &Signature::from_bytes(&bytes))
            .map_err(|_| QuirksError::BadSignature)?;

        // Only now is it safe to look at the contents.
        let quirks: Quirks = serde_json::from_str(&self.document)
            .map_err(|e| QuirksError::Malformed(e.to_string()))?;
        if quirks.schema_version > SCHEMA_VERSION {
            return Err(QuirksError::SchemaTooNew {
                found: quirks.schema_version,
            });
        }
        Ok(quirks)
    }
}

/// The quirks in force, plus the machinery to replace them.
#[derive(Debug, Clone)]
pub struct QuirksStore {
    verifying_key: [u8; 32],
    current: Quirks,
}

impl QuirksStore {
    /// Start from the compiled-in defaults.
    #[must_use]
    pub fn new(verifying_key: [u8; 32]) -> Self {
        Self {
            verifying_key,
            current: Quirks::default(),
        }
    }

    /// Load the document cached at `path`, falling back to the compiled-in
    /// defaults on any problem.
    ///
    /// Deliberately infallible: a corrupt or tampered cache must degrade to
    /// "the values this binary shipped with", never to "the daemon will not
    /// start".
    #[must_use]
    pub fn load(verifying_key: [u8; 32], path: &Path) -> Self {
        let mut store = Self::new(verifying_key);
        let Ok(raw) = std::fs::read_to_string(path) else {
            return store;
        };
        let Ok(signed) = serde_json::from_str::<SignedQuirks>(&raw) else {
            tracing::warn!(path = %path.display(), "cached quirks are unreadable; using built-ins");
            return store;
        };
        match store.apply(&signed) {
            Ok(()) => store,
            Err(error) => {
                tracing::warn!(%error, "cached quirks rejected; using built-ins");
                store
            }
        }
    }

    /// Verify and install a document.
    ///
    /// # Errors
    ///
    /// [`QuirksError`] when the document is not trustworthy or not newer.
    pub fn apply(&mut self, signed: &SignedQuirks) -> Result<(), QuirksError> {
        let candidate = signed.verify(&self.verifying_key)?;
        if candidate.serial <= self.current.serial && self.current.serial != 0 {
            return Err(QuirksError::Rollback {
                offered: candidate.serial,
                installed: self.current.serial,
            });
        }
        self.current = candidate;
        Ok(())
    }

    /// Persist a document that has already been applied.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures. A cache we could not write is not fatal — the
    /// document is in force for this process either way.
    pub fn persist(signed: &SignedQuirks, path: &Path) -> std::io::Result<()> {
        // Atomic: a half-written document is rejected at verification and the
        // built-ins take over, which is safe but means a crash mid-write
        // silently discards a provider fix the user had already received.
        ironwire_core::atomic::write(path, &serde_json::to_string_pretty(signed)?)
    }

    /// The quirks currently in force.
    #[must_use]
    pub fn current(&self) -> &Quirks {
        &self.current
    }

    /// Serial in force; `0` means "compiled-in defaults, nothing installed".
    #[must_use]
    pub fn serial(&self) -> u64 {
        self.current.serial
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

    fn keypair() -> (SigningKey, [u8; 32]) {
        // Fixed seed: the test is about the protocol, not about randomness.
        let signing = SigningKey::from_bytes(&[7u8; 32]);
        let verifying = signing.verifying_key().to_bytes();
        (signing, verifying)
    }

    fn sign(signing: &SigningKey, document: &str) -> SignedQuirks {
        SignedQuirks {
            document: document.to_string(),
            signature: hex::encode(signing.sign(document.as_bytes()).to_bytes()),
        }
    }

    fn document(serial: u64, oauth_beta: &str) -> String {
        serde_json::json!({
            "schema_version": 1,
            "serial": serial,
            "issued_at": "2026-08-08T00:00:00Z",
            "anthropic": {"api_version": "2023-06-01", "oauth_beta": oauth_beta},
        })
        .to_string()
    }

    #[test]
    fn a_properly_signed_document_is_applied() {
        let (signing, verifying) = keypair();
        let mut store = QuirksStore::new(verifying);
        assert_eq!(store.current().anthropic.oauth_beta, "oauth-2025-04-20");

        store
            .apply(&sign(&signing, &document(1, "oauth-2026-09-01")))
            .expect("applies");
        assert_eq!(store.current().anthropic.oauth_beta, "oauth-2026-09-01");
        assert_eq!(store.serial(), 1);
    }

    #[test]
    fn a_document_signed_by_another_key_is_refused() {
        let (_, verifying) = keypair();
        let attacker = SigningKey::from_bytes(&[9u8; 32]);
        let mut store = QuirksStore::new(verifying);
        assert_eq!(
            store.apply(&sign(&attacker, &document(1, "oauth-evil"))),
            Err(QuirksError::BadSignature)
        );
        // And nothing changed.
        assert_eq!(store.current().anthropic.oauth_beta, "oauth-2025-04-20");
    }

    #[test]
    fn tampering_with_the_body_invalidates_the_signature() {
        let (signing, verifying) = keypair();
        let mut signed = sign(&signing, &document(1, "oauth-good"));
        signed.document = document(1, "oauth-tampered");
        let mut store = QuirksStore::new(verifying);
        assert_eq!(store.apply(&signed), Err(QuirksError::BadSignature));
    }

    #[test]
    fn an_older_serial_is_refused_as_a_rollback() {
        // The attack this guards: replay an old, signed document to re-expose
        // a provider workaround we already corrected.
        let (signing, verifying) = keypair();
        let mut store = QuirksStore::new(verifying);
        store
            .apply(&sign(&signing, &document(5, "oauth-current")))
            .expect("applies");
        assert_eq!(
            store.apply(&sign(&signing, &document(4, "oauth-old"))),
            Err(QuirksError::Rollback {
                offered: 4,
                installed: 5
            })
        );
        assert_eq!(store.current().anthropic.oauth_beta, "oauth-current");
    }

    #[test]
    fn replaying_the_same_serial_is_refused() {
        let (signing, verifying) = keypair();
        let mut store = QuirksStore::new(verifying);
        store
            .apply(&sign(&signing, &document(5, "a")))
            .expect("applies");
        assert!(matches!(
            store.apply(&sign(&signing, &document(5, "b"))),
            Err(QuirksError::Rollback { .. })
        ));
    }

    #[test]
    fn a_newer_schema_is_refused_rather_than_half_applied() {
        // Half-understanding a provider workaround is worse than using the
        // values we shipped with.
        let (signing, verifying) = keypair();
        let document = serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "serial": 1,
            "issued_at": "2026-08-08T00:00:00Z",
        })
        .to_string();
        let mut store = QuirksStore::new(verifying);
        assert_eq!(
            store.apply(&sign(&signing, &document)),
            Err(QuirksError::SchemaTooNew {
                found: SCHEMA_VERSION + 1
            })
        );
    }

    #[test]
    fn a_signed_but_unparseable_body_is_refused_after_verification() {
        let (signing, verifying) = keypair();
        let mut store = QuirksStore::new(verifying);
        let err = store
            .apply(&sign(&signing, "not a quirks document"))
            .expect_err("must refuse");
        assert!(matches!(err, QuirksError::Malformed(_)));
    }

    #[test]
    fn a_corrupt_cache_degrades_to_the_built_ins_rather_than_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("quirks.json");
        std::fs::write(&path, "{ not json").expect("writes");
        let (_, verifying) = keypair();
        let store = QuirksStore::load(verifying, &path);
        assert_eq!(store.serial(), 0);
        assert_eq!(store.current().anthropic.oauth_beta, "oauth-2025-04-20");
    }

    #[test]
    fn a_missing_cache_is_not_an_error() {
        let (_, verifying) = keypair();
        let store = QuirksStore::load(verifying, Path::new("/nonexistent/quirks.json"));
        assert_eq!(store.serial(), 0);
    }

    #[test]
    fn a_persisted_document_survives_a_restart_and_is_re_verified() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("quirks.json");
        let (signing, verifying) = keypair();

        let signed = sign(&signing, &document(3, "oauth-persisted"));
        let mut store = QuirksStore::new(verifying);
        store.apply(&signed).expect("applies");
        QuirksStore::persist(&signed, &path).expect("persists");

        let reloaded = QuirksStore::load(verifying, &path);
        assert_eq!(reloaded.serial(), 3);
        assert_eq!(reloaded.current().anthropic.oauth_beta, "oauth-persisted");
    }

    #[test]
    fn a_cache_tampered_with_on_disk_is_rejected_on_reload() {
        // Local tampering is in scope: the cache sits in a directory another
        // process on the machine may be able to write.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("quirks.json");
        let (signing, verifying) = keypair();
        let mut signed = sign(&signing, &document(3, "oauth-good"));
        signed.document = document(3, "oauth-tampered");
        QuirksStore::persist(&signed, &path).expect("persists");

        let reloaded = QuirksStore::load(verifying, &path);
        assert_eq!(reloaded.serial(), 0, "tampered cache must not take effect");
        assert_eq!(reloaded.current().anthropic.oauth_beta, "oauth-2025-04-20");
    }

    #[test]
    fn a_malformed_signature_is_refused_before_anything_is_parsed() {
        let (_, verifying) = keypair();
        let mut store = QuirksStore::new(verifying);
        for signature in ["", "zz", &"a".repeat(200)] {
            let signed = SignedQuirks {
                document: document(1, "x"),
                signature: signature.to_string(),
            };
            assert_eq!(
                store.apply(&signed),
                Err(QuirksError::MalformedSignature),
                "signature {signature:?} should be rejected"
            );
        }
    }
}

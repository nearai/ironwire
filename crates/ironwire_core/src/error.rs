//! Core error type.

use std::path::PathBuf;

/// Errors raised by configuration and policy loading.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A config file could not be read.
    #[error("could not read {path}: {source}")]
    ConfigRead {
        /// Path we tried.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A config file was not valid TOML, or had the wrong shape.
    ///
    /// The message names the fix, because this error blocks *every* command —
    /// a user who cannot parse their config also cannot run `status` to find
    /// out what is wrong, and a bare TOML diagnostic leaves them guessing
    /// whether IronWire is broken or their file is.
    #[error(
        "{path} could not be read as configuration.\n\n{detail}\n\n\
         IronWire runs fine with no config at all — move that file aside to get \
         the defaults back, or run `ironwire init --write` afterwards for a \
         commented one."
    )]
    ConfigParse {
        /// Path we tried.
        path: PathBuf,
        /// Underlying parse error.
        ///
        /// Named `detail` rather than `source` on purpose. `thiserror` treats a
        /// field called `source` as the error chain, and since the message
        /// above already quotes it, the chain would print the same TOML
        /// diagnostic a second time with the actionable sentence buried between
        /// the two copies.
        detail: toml::de::Error,
    },

    /// The home directory could not be determined.
    #[error("could not determine a home directory for $IRONWIRE_HOME")]
    NoHome,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

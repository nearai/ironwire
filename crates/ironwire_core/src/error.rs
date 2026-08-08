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
    #[error("invalid config at {path}: {source}")]
    ConfigParse {
        /// Path we tried.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: toml::de::Error,
    },

    /// The home directory could not be determined.
    #[error("could not determine a home directory for $IRONWIRE_HOME")]
    NoHome,
}

/// Result alias for this crate.
pub type Result<T> = std::result::Result<T, Error>;

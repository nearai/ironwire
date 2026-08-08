//! Provider façades: the native API surfaces IronWire presents on loopback.

pub mod anthropic;
pub mod error;
pub mod openai;

pub use error::FacadeError;

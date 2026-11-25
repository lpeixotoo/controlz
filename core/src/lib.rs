//! Device control core for Controlz: LG webOS TVs (local network) and
//! LG ThinQ air conditioners (ThinQ Connect cloud API).

mod thinq;
mod webos;
mod wol;

pub use thinq::{AcState, ThinqClient, ThinqDevice};
pub use webos::{TvApp, TvClient, TvInput, TvVolume};

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum ControlError {
    #[error("Network error: {message}")]
    Network { message: String },
    #[error("TV pairing failed: {message}")]
    Pairing { message: String },
    #[error("Device returned an error: {message}")]
    Device { message: String },
    #[error("Invalid input: {message}")]
    Invalid { message: String },
}

impl ControlError {
    pub(crate) fn network(e: impl std::fmt::Display) -> Self {
        Self::Network { message: e.to_string() }
    }
    pub(crate) fn device(e: impl std::fmt::Display) -> Self {
        Self::Device { message: e.to_string() }
    }
    pub(crate) fn invalid(e: impl std::fmt::Display) -> Self {
        Self::Invalid { message: e.to_string() }
    }
}

pub type Result<T> = std::result::Result<T, ControlError>;

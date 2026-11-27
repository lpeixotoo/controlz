//! Error type shared by every core module and exported to Swift.

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
    #[error("Config error: {message}")]
    Config { message: String },
}

impl ControlError {
    pub(crate) fn network(e: impl std::fmt::Display) -> Self {
        Self::Network {
            message: e.to_string(),
        }
    }

    pub(crate) fn device(e: impl std::fmt::Display) -> Self {
        Self::Device {
            message: e.to_string(),
        }
    }

    pub(crate) fn invalid(e: impl std::fmt::Display) -> Self {
        Self::Invalid {
            message: e.to_string(),
        }
    }

    pub(crate) fn config(e: impl std::fmt::Display) -> Self {
        Self::Config {
            message: e.to_string(),
        }
    }
}

pub type Result<T> = std::result::Result<T, ControlError>;

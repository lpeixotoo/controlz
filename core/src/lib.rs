//! Device control core for Controlz: LG webOS TVs and other LG appliances,
//! controlled over the local network only (no cloud).

pub mod ac;
mod config;
mod error;
mod secret;
mod webos;
mod wol;

pub use config::{AcConfig, CONFIG_KEYS, Config, TvConfig};
pub use error::{ControlError, Result};
pub use secret::Secret;
pub use wol::send_magic_packet;

uniffi::setup_scaffolding!();

//! Device control core for Controlz: LG webOS TVs and other LG appliances,
//! controlled over the local network only (no cloud).

mod error;
mod webos;
mod wol;

pub use error::{ControlError, Result};
pub use wol::send_magic_packet;

uniffi::setup_scaffolding!();

//! Local AC route (D20): a clean-room stand-in for LG's `ThinQ2` cloud.
//!
//! `tls` holds the legacy TLS 1.2 profile the LCW-003 Wi-Fi module needs;
//! `pki` holds our CA, the server certificate and CSR signing.

pub mod pki;
pub mod tls;

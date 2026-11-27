//! Local AC route (D20): a clean-room stand-in for LG's `ThinQ2` cloud.
//!
//! `tls` holds the legacy TLS 1.2 profile the LCW-003 Wi-Fi module needs;
//! `pki` holds our CAs, the server certificate and CSR signing; `setup` is
//! the `SoftAP` setup client (port 5500); `cloud` is the local cloud HTTPS
//! server the module calls while it is being provisioned.

pub mod cloud;
pub mod pki;
pub mod setup;
pub mod tls;

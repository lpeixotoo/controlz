//! Secrets in the macOS Keychain (D10): generic passwords under one service
//! shared by the CLI and the app; the account is the secret name.

use controlz_core::Secret;
use security_framework::passwords::{
    delete_generic_password, get_generic_password, set_generic_password,
};

/// Keychain service for every Controlz secret.
pub const SERVICE: &str = "dev.controlz";

/// Secret names the CLI accepts (Keychain account names).
pub const SECRET_NAMES: &[&str] = &["tv.client-key"];

/// `errSecItemNotFound`.
const ITEM_NOT_FOUND: i32 = -25300;

/// Error type for CLI commands: messages only, never secret values.
pub type Error = Box<dyn std::error::Error>;

/// Fails unless `name` is one of [`SECRET_NAMES`].
pub fn check_name(name: &str) -> Result<(), Error> {
    if SECRET_NAMES.contains(&name) {
        Ok(())
    } else {
        Err(format!("unknown secret {name} (known: {})", SECRET_NAMES.join(", ")).into())
    }
}

/// Creates or replaces the Keychain item for `name` under `service`.
pub fn set(service: &str, name: &str, value: &Secret) -> Result<(), Error> {
    tracing::debug!(service, name, "writing Keychain item");
    set_generic_password(service, name, value.expose().as_bytes())
        .map_err(|e| format!("Keychain write failed for {name}: {e}").into())
}

/// Reads the Keychain item for `name`; `Ok(None)` when it does not exist.
pub fn get(service: &str, name: &str) -> Result<Option<Secret>, Error> {
    tracing::debug!(service, name, "reading Keychain item");
    match get_generic_password(service, name) {
        Ok(bytes) => {
            let value = String::from_utf8(bytes)
                .map_err(|_| format!("Keychain item {name} is not valid UTF-8"))?;
            Ok(Some(Secret::new(value)))
        }
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
        Err(e) => Err(format!("Keychain read failed for {name}: {e}").into()),
    }
}

/// Deletes the item; `Ok(false)` when it did not exist.
pub fn delete(service: &str, name: &str) -> Result<bool, Error> {
    tracing::debug!(service, name, "deleting Keychain item");
    match delete_generic_password(service, name) {
        Ok(()) => Ok(true),
        Err(e) if e.code() == ITEM_NOT_FOUND => Ok(false),
        Err(e) => Err(format!("Keychain delete failed for {name}: {e}").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SERVICE: &str = "dev.controlz.test";

    #[test]
    fn only_known_names() {
        assert!(check_name("tv.client-key").is_ok());
        assert!(check_name("tv.host").is_err());
    }

    /// Touches the real login Keychain, so it only runs on request:
    /// `cargo test -p controlz -- --ignored keychain_round_trip`.
    #[test]
    #[ignore = "uses the real macOS Keychain"]
    fn keychain_round_trip() {
        let name = "tv.client-key";
        let value = Secret::new("fake-client-key-for-tests");
        let _ = delete(TEST_SERVICE, name);
        assert!(get(TEST_SERVICE, name).unwrap().is_none());
        set(TEST_SERVICE, name, &value).unwrap();
        assert_eq!(
            get(TEST_SERVICE, name)
                .unwrap()
                .map(|s| s.expose().to_owned()),
            Some("fake-client-key-for-tests".to_owned())
        );
        assert!(delete(TEST_SERVICE, name).unwrap());
        assert!(!delete(TEST_SERVICE, name).unwrap());
    }
}

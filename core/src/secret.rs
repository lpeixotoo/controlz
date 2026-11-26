//! `Secret`: a string that never shows its value in `Debug`, `Display` or logs.

use std::fmt;

/// A secret value (for example the TV client-key).
///
/// `Debug` and `Display` always print `***`, so the value cannot leak through
/// `{:?}`, error messages or `tracing` fields. Read it only with [`Secret::expose`].
///
/// Equality is intentionally absent: a derived `==` compares in non-constant
/// time. If an auth path ever needs to compare secrets, use a constant-time
/// comparison on [`Secret::expose`].
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps a raw value so it is masked everywhere except [`Secret::expose`].
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value. Call this only at the point where it is sent or stored.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::sync::{Arc, Mutex};

    const RAW: &str = "fake-client-key-for-tests";

    #[derive(Debug)]
    #[allow(dead_code)] // fields are read only through Debug
    struct Holder {
        name: &'static str,
        key: Secret,
    }

    #[test]
    fn debug_and_display_hide_value() {
        let secret = Secret::new(RAW);
        let outputs = [
            format!("{secret:?}"),
            format!("{secret:#?}"),
            format!("{secret}"),
            format!(
                "{:?}",
                Holder {
                    name: "tv",
                    key: secret.clone()
                }
            ),
            format!("{:#?}", Some(vec![secret.clone()])),
        ];
        for out in &outputs {
            assert!(!out.contains(RAW), "secret leaked: {out}");
            assert!(out.contains("***"), "missing mask: {out}");
        }
        assert_eq!(secret.expose(), RAW);
    }

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn logs_hide_value() {
        let buffer = Buffer::default();
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();

        let secret = Secret::new(RAW);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(key = ?secret, "debug field");
            tracing::info!(key = %secret, "display field");
            tracing::debug!("inline {secret} and {secret:?}");
            let span = tracing::info_span!("pairing", key = ?secret);
            let _guard = span.enter();
            tracing::trace!("inside span");
        });

        let logs = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("debug field"), "logs not captured: {logs}");
        assert!(logs.contains("inside span"), "logs not captured: {logs}");
        assert!(!logs.contains(RAW), "secret leaked into logs: {logs}");
        assert!(logs.contains("***"), "missing mask in logs: {logs}");
    }
}
